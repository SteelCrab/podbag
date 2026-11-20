//! `podbag tui`: containers, images, volumes and networks in the terminal. A client of
//! the Docker API: lists refresh on events (`GET /events`), or every two seconds when the
//! stream is closed. Requests run in threads; the screen never waits for them.

mod app;
mod view;

use app::{App, Effect, Lists, Msg, Verb};
use podbag::client::{Client, Closer, Endpoint, Kind, podbag_program};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// How often the lists are read while the events stream is closed.
const POLL: Duration = Duration::from_secs(2);
/// Events come in bursts (a `run` is create, attach, start…): one refresh for them.
const SETTLE: Duration = Duration::from_millis(150);
/// How often the lists are read while events come, for the times they show.
const AGES: Duration = Duration::from_secs(15);

pub fn run(once: bool) -> i32 {
    let exe = podbag_program();
    let endpoint = match Endpoint::find(&exe) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("podbag: {e}");
            return 1;
        }
    };
    if let Err(e) = endpoint.ensure_running(&exe, true) {
        eprintln!("podbag: {e}");
        return 1;
    }
    let engine = endpoint.describe();
    let client = Client::new(endpoint);
    if once {
        return print_once(&client, engine);
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        eprintln!("podbag: the TUI needs a terminal");
        return 1;
    }
    let mut terminal = ratatui::init();
    let result = Runtime::new(client, exe, engine).run(&mut terminal);
    ratatui::restore();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("podbag: {e}");
            1
        }
    }
}

/// One screen of the containers list, as text (no terminal needed).
fn print_once(client: &Client, engine: String) -> i32 {
    let mut app = App::new(engine);
    app.now = now();
    match load(client) {
        Ok(l) => app.set_lists(l),
        Err(e) => {
            eprintln!("podbag: {e}");
            return 1;
        }
    }
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let Ok(mut t) = ratatui::Terminal::new(backend);
    let Ok(_) = t.draw(|f| view::draw(f, &mut app));
    println!("{}", view::buffer_text(t.backend().buffer()));
    0
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// All four lists.
fn load(client: &Client) -> Result<Lists, String> {
    let err = |e: podbag::client::ApiError| e.message;
    Ok(Lists {
        containers: client.containers().map_err(err)?,
        images: client.images().map_err(err)?,
        volumes: client.volumes().map_err(err)?,
        networks: client.networks().map_err(err)?,
    })
}

/// Act on one object; what to show.
fn act(client: &Client, kind: Kind, id: &str, name: &str, verb: Verb) -> Msg {
    let done = match verb {
        Verb::Remove { force } => client.remove(kind, id, force),
        v => client.container_action(id, v.action()),
    };
    match done {
        Ok(()) => Msg::Done(format!("{} {name}", verb.done()), false),
        Err(e) => Msg::Done(format!("Cannot {} {name}: {e}", verb.action()), true),
    }
}

fn prune(client: &Client, kind: Kind) -> Msg {
    match client.prune(kind) {
        Ok(p) => Msg::Done(
            format!(
                "Removed {} {}, {} freed",
                p.count,
                kind.segment(),
                podbag::image::human_size(p.bytes)
            ),
            false,
        ),
        Err(e) => Msg::Done(format!("Cannot prune {}: {e}", kind.segment()), true),
    }
}

struct Runtime {
    client: Client,
    exe: PathBuf,
    app: App,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// A refresh is running.
    loading: bool,
    next_refresh: Option<Instant>,
    /// The open log stream.
    logs: Option<Closer>,
    asked_features: bool,
}

impl Runtime {
    fn new(client: Client, exe: PathBuf, engine: String) -> Runtime {
        let (tx, rx) = channel();
        Runtime {
            client,
            exe,
            app: App::new(engine),
            tx,
            rx,
            loading: false,
            next_refresh: Some(Instant::now()),
            logs: None,
            asked_features: false,
        }
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        self.watch_events();
        loop {
            self.app.now = now();
            terminal.draw(|f| view::draw(f, &mut self.app))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
                && k.kind != KeyEventKind::Release
            {
                let effect = self.app.on_key(k);
                if effect == Effect::Quit {
                    self.close_logs();
                    return Ok(());
                }
                self.apply(effect, terminal)?;
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.on_msg(msg);
            }
            if !self.loading && self.next_refresh.is_some_and(|t| Instant::now() >= t) {
                self.refresh();
            }
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match &msg {
            Msg::Changed => self.soon(SETTLE),
            Msg::Live(_) => self.soon(Duration::ZERO),
            Msg::Lists(result) => {
                self.loading = false;
                // Without events, read again soon; with them, now and then for the
                // times shown ("Up 5 minutes").
                self.soon(if self.app.live { AGES } else { POLL });
                if let (Ok(l), false) = (result, self.asked_features) {
                    self.asked_features = true;
                    let sample = l
                        .containers
                        .iter()
                        .find(|c| c.running())
                        .map(|c| c.id.clone());
                    let (client, tx) = (self.client.clone(), self.tx.clone());
                    std::thread::spawn(move || {
                        let _ = tx.send(Msg::Features(client.features(sample.as_deref())));
                    });
                }
            }
            Msg::Done(..) => self.soon(Duration::ZERO),
            _ => {}
        }
        self.app.on_msg(msg);
    }

    /// Refresh within `after` (an earlier one stays).
    fn soon(&mut self, after: Duration) {
        let at = Instant::now() + after;
        if self.next_refresh.is_none_or(|t| t > at) {
            self.next_refresh = Some(at);
        }
    }

    fn refresh(&mut self) {
        self.loading = true;
        self.next_refresh = None;
        let (client, tx) = (self.client.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Lists(load(&client)));
        });
    }

    /// The events stream, opened again when it closes.
    fn watch_events(&self) {
        let (client, tx) = (self.client.clone(), self.tx.clone());
        std::thread::spawn(move || {
            loop {
                if let Ok(events) = client.events() {
                    if tx.send(Msg::Live(true)).is_err() {
                        return;
                    }
                    for e in events {
                        let Ok(e) = e else { break };
                        // Exec and attach events change no list.
                        let quiet = e.action.starts_with("exec_")
                            || matches!(e.action.as_str(), "attach" | "detach" | "resize");
                        if !quiet && tx.send(Msg::Changed).is_err() {
                            return;
                        }
                    }
                }
                if tx.send(Msg::Live(false)).is_err() {
                    return;
                }
                std::thread::sleep(POLL);
            }
        });
    }

    fn close_logs(&mut self) {
        if let Some(c) = self.logs.take() {
            c.close();
        }
    }

    /// Run an effect: in a thread, except the shell, which takes the terminal.
    fn apply(&mut self, effect: Effect, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        let (client, tx) = (self.client.clone(), self.tx.clone());
        match effect {
            Effect::None | Effect::Quit => {}
            Effect::Refresh => self.soon(Duration::ZERO),
            Effect::CloseLogs => self.close_logs(),
            Effect::Shell { id, name } => {
                let code = self.shell(&id, terminal)?;
                let text = match code {
                    Some(0) => format!("The shell in {name} ended"),
                    Some(c) => format!("The shell in {name} ended with exit code {c}"),
                    None => format!("Cannot open a shell in {name}"),
                };
                self.app.message = Some((text, code != Some(0)));
            }
            Effect::Act {
                kind,
                id,
                name,
                verb,
            } => {
                self.app.busy = Some(format!("{} {name}", verb.doing()));
                std::thread::spawn(move || {
                    let _ = tx.send(act(&client, kind, &id, &name, verb));
                });
            }
            Effect::Inspect {
                kind,
                id,
                name,
                running,
            } => {
                let features = self.app.features;
                std::thread::spawn(move || {
                    let msg = match client.inspect(kind, &id) {
                        Ok(v) => {
                            let mut text = serde_json::to_string_pretty(&v).unwrap_or_default();
                            if kind == Kind::Containers && running {
                                for (on, what) in [(features.top, "top"), (features.stats, "stats")]
                                {
                                    if let (true, Ok(v)) = (on, client.container_json(&id, what)) {
                                        text.push_str(&format!(
                                            "\n\n{what}:\n{}",
                                            serde_json::to_string_pretty(&v).unwrap_or_default()
                                        ));
                                    }
                                }
                            }
                            Msg::Text(
                                format!("Inspect: {name}"),
                                text.lines().map(str::to_string).collect(),
                            )
                        }
                        Err(e) => Msg::Done(format!("Cannot inspect {name}: {e}"), true),
                    };
                    let _ = tx.send(msg);
                });
            }
            Effect::Logs { id, name } => {
                self.close_logs();
                let generation = self.app.open_logs(&name);
                match client.logs(&id, 1000, true) {
                    Ok(lines) => {
                        self.logs = Some(lines.closer());
                        std::thread::spawn(move || {
                            for line in lines {
                                let Ok(line) = line else { break };
                                if tx.send(Msg::LogLine(generation, line)).is_err() {
                                    return;
                                }
                            }
                            let _ = tx.send(Msg::LogEnd(generation, None));
                        });
                    }
                    Err(e) => {
                        self.app.on_msg(Msg::LogEnd(
                            generation,
                            Some(format!("Cannot read the logs of {name}: {e}")),
                        ));
                    }
                }
            }
            Effect::Pull(image) => {
                std::thread::spawn(move || {
                    let progress = tx.clone();
                    let done = client.pull(&image, |line| {
                        let _ = progress.send(Msg::Progress(format!("Pulling {image}: {line}")));
                    });
                    let _ = tx.send(match done {
                        Ok(()) => Msg::Done(format!("Pulled {image}"), false),
                        Err(e) => Msg::Done(format!("Cannot pull {image}: {e}"), true),
                    });
                });
            }
            Effect::Prune(kind) => {
                std::thread::spawn(move || {
                    let _ = tx.send(prune(&client, kind));
                });
            }
            Effect::Rename { id, to } => {
                std::thread::spawn(move || {
                    let _ = tx.send(match client.rename(&id, &to) {
                        Ok(()) => Msg::Done(format!("Renamed to {to}"), false),
                        Err(e) => Msg::Done(format!("Cannot rename: {e}"), true),
                    });
                });
            }
        }
        Ok(())
    }

    /// `podbag exec -it <id> sh` on the normal screen, then back. Its exit code.
    fn shell(&mut self, id: &str, terminal: &mut DefaultTerminal) -> std::io::Result<Option<i32>> {
        use ratatui::crossterm::{execute, terminal as term};
        execute!(std::io::stdout(), term::LeaveAlternateScreen)?;
        term::disable_raw_mode()?;
        terminal.show_cursor()?;
        let status = std::process::Command::new(&self.exe)
            .args(["exec", "-it", id, "sh"])
            .status();
        term::enable_raw_mode()?;
        execute!(std::io::stdout(), term::EnterAlternateScreen)?;
        terminal.clear()?;
        Ok(status.ok().and_then(|s| s.code()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, small_engine};

    #[test]
    fn the_lists_come_from_the_engine() {
        let engine = FakeEngine::start(small_engine);
        let lists = load(&engine.client).unwrap();
        assert_eq!(lists.containers.len(), 2);
        assert_eq!(lists.images.len(), 2);
        assert_eq!(lists.volumes[0].name, "pgdata");
        assert_eq!(lists.networks.len(), 4);
        let mut app = App::new("test".into());
        app.set_lists(lists);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut t = ratatui::Terminal::new(backend).unwrap();
        t.draw(|f| view::draw(f, &mut app)).unwrap();
        let s = view::buffer_text(t.backend().buffer());
        assert!(s.contains("web") && s.contains("job"), "{s}");
    }

    #[test]
    fn actions_send_requests_and_report_back() {
        let engine = FakeEngine::start(small_engine);
        let c = &engine.client;
        let done = |m: Msg| match m {
            Msg::Done(text, error) => (text, error),
            other => panic!("{other:?}"),
        };
        let web = |verb| done(act(c, Kind::Containers, "web", "web", verb));
        assert_eq!(web(Verb::Stop), ("Stopped web".into(), false));
        assert_eq!(
            web(Verb::Remove { force: true }),
            ("Removed web".into(), false)
        );
        let (text, error) = web(Verb::Pause);
        assert!(error && text.contains("not supported"), "{text}");
        assert_eq!(
            done(act(
                c,
                Kind::Images,
                "sha256:22",
                "alpine",
                Verb::Remove { force: false }
            )),
            ("Removed alpine".into(), false)
        );
        let (text, error) = done(prune(c, Kind::Containers));
        assert!(!error && text.starts_with("Removed 1 containers"), "{text}");
        assert_eq!(
            engine.requests(),
            [
                "POST /containers/web/stop",
                "DELETE /containers/web?force=1",
                "POST /containers/web/pause",
                "DELETE /images/sha256%3A22",
                "POST /containers/prune",
            ]
        );
        assert_eq!(c.features(Some("web")), podbag::client::Features::default());
    }
}
