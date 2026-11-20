//! `podbag tui`: containers, images, volumes and networks in the terminal. A client of
//! the Docker API (podbag's, Docker's or a socket's): lists refresh on events
//! (`GET /events`), or every two seconds when the stream is closed. Requests run in
//! threads; the screen never waits for them.

mod app;
mod mouse;
mod view;

use app::{App, Effect, Lists, Msg, Verb};
use podbag::client::choice::{self, docker_down};
use podbag::client::{ApiError, Choice, Client, Closer, Endpoint, Kind, Target, podbag_program};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the lists are read while the events stream is closed.
const POLL: Duration = Duration::from_secs(2);
/// Events come in bursts (a `run` is create, attach, start…): one refresh for them.
const SETTLE: Duration = Duration::from_millis(150);
/// How often the lists are read while events come, for the times they show.
const AGES: Duration = Duration::from_secs(15);
/// How often the VM's state is read (from files: it never wakes the VM).
const VM_EVERY: Duration = Duration::from_secs(2);

pub fn run(once: bool, engine: Option<Choice>, mouse: bool) -> i32 {
    let exe = podbag_program();
    let target = match Target::find(engine.as_ref(), &exe) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("podbag: {e}");
            return 1;
        }
    };
    let ready = target.ensure_running(&exe, true);
    if once {
        if let Err(e) = ready {
            eprintln!("podbag: {e}");
            return 1;
        }
        return print_once(&target);
    }
    // Docker is never started: the TUI says so, and E goes back to podbag.
    if let (Err(e), false) = (&ready, target.is_docker()) {
        eprintln!("podbag: {e}");
        return 1;
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        eprintln!("podbag: the TUI needs a terminal");
        return 1;
    }
    let mut runtime = Runtime::new(target, exe, Engines::real());
    runtime.app.mouse = mouse;
    runtime.app.update = podbag::upgrade::known_newer().map(|v| v.to_string());
    let tx = runtime.sender();
    std::thread::spawn(move || {
        tx.send(Msg::Update(
            podbag::upgrade::newer_release().map(|v| v.to_string()),
        ));
    });
    let mut terminal = ratatui::init();
    // ratatui's hook restores the terminal on a panic; the mouse must go back too.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    if mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    let result = runtime.run(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
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
fn print_once(target: &Target) -> i32 {
    let mut app = App::new(target.choice.label(), target.endpoint.describe());
    app.now = now();
    app.vm = vm_state(target);
    match load_text(&Client::new(target.endpoint.clone()), target.is_docker()) {
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
fn load(client: &Client) -> Result<Lists, ApiError> {
    Ok(Lists {
        containers: client.containers()?,
        images: client.images()?,
        volumes: client.volumes()?,
        networks: client.networks()?,
    })
}

/// All four lists, or what to show: when Docker does not answer, how to start it.
fn load_text(client: &Client, docker: bool) -> Result<Lists, String> {
    load(client).map_err(|e| {
        if docker && e.status == 0 {
            docker_down().to_string()
        } else {
            e.message
        }
    })
}

/// The macOS VM's state, for podbag only. Elsewhere there is none to show (on Windows
/// the TUI runs in the WSL distribution).
#[allow(unused_variables)]
fn vm_state(target: &Target) -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    if target.choice == Choice::Podbag {
        return podbag::macos::host::paths()
            .ok()
            .map(|p| podbag::macos::cli::state(&p));
    }
    None
}

/// The shell for `e`: `podbag exec -it` for podbag, `docker exec -it` for Docker (on its
/// socket). None when Docker's CLI is missing.
fn shell_command(
    target: &Target,
    exe: &Path,
    docker: Option<&Path>,
    id: &str,
) -> Option<std::process::Command> {
    let program = match (&target.choice, docker) {
        (Choice::Podbag, _) | (Choice::Socket(_), None) => exe,
        (_, Some(docker)) => docker,
        (Choice::Docker, None) => return None,
    };
    let mut c = std::process::Command::new(program);
    c.args(["exec", "-it", id, "sh"]);
    if program != exe
        && let Endpoint::Socket { path, .. } = &target.endpoint
    {
        c.env("DOCKER_HOST", format!("unix://{}", path.display()));
        c.env_remove("DOCKER_CONTEXT");
    }
    Some(c)
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

type FindEngine = Box<dyn Fn(&Choice) -> Result<Target, String>>;
type SaveChoice = Box<dyn Fn(&Choice) -> Result<(), String>>;
type VmState = Box<dyn Fn(&Target) -> Option<&'static str>>;

/// How the TUI finds, saves and looks at engines (tests give fakes).
struct Engines {
    find: FindEngine,
    save: SaveChoice,
    vm: VmState,
    docker_cli: Option<PathBuf>,
}

impl Engines {
    fn real() -> Engines {
        let exe = podbag_program();
        Engines {
            find: Box::new(move |c| Target::find(Some(c), &exe)),
            save: Box::new(choice::save),
            vm: Box::new(vm_state),
            docker_cli: choice::find_docker_cli(),
        }
    }
}

/// Sends to the screen, marked with the engine's generation: what an older engine sends
/// is dropped.
#[derive(Clone)]
struct Tx {
    generation: u64,
    tx: Sender<(u64, Msg)>,
}

impl Tx {
    /// False when the screen has gone.
    fn send(&self, msg: Msg) -> bool {
        self.tx.send((self.generation, msg)).is_ok()
    }
}

struct Runtime {
    target: Target,
    client: Client,
    exe: PathBuf,
    engines: Engines,
    app: App,
    tx: Sender<(u64, Msg)>,
    rx: Receiver<(u64, Msg)>,
    /// The engine's generation: a new one on each switch.
    generation: u64,
    /// The generation the events thread must serve (it ends on another).
    current: Arc<AtomicU64>,
    /// Ends the open events stream.
    events: Arc<Mutex<Option<Closer>>>,
    /// A refresh is running.
    loading: bool,
    next_refresh: Option<Instant>,
    next_vm: Instant,
    poll: Duration,
    /// The open log stream.
    logs: Option<Closer>,
    asked_features: bool,
}

impl Runtime {
    fn new(target: Target, exe: PathBuf, engines: Engines) -> Runtime {
        let (tx, rx) = channel();
        let mut app = App::new(target.choice.label(), target.endpoint.describe());
        app.shell = shell_command(&target, &exe, engines.docker_cli.as_deref(), "").is_some();
        app.vm = (engines.vm)(&target);
        Runtime {
            client: Client::new(target.endpoint.clone()),
            target,
            exe,
            engines,
            app,
            tx,
            rx,
            generation: 0,
            current: Arc::default(),
            events: Arc::default(),
            loading: false,
            next_refresh: Some(Instant::now()),
            next_vm: Instant::now() + VM_EVERY,
            poll: POLL,
            logs: None,
            asked_features: false,
        }
    }

    fn sender(&self) -> Tx {
        Tx {
            generation: self.generation,
            tx: self.tx.clone(),
        }
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        self.watch_events();
        loop {
            self.app.now = now();
            terminal.draw(|f| view::draw(f, &mut self.app))?;
            if event::poll(Duration::from_millis(100))? {
                let effect = match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => self.app.on_key(k),
                    Event::Mouse(m) => self.app.on_mouse(m, Instant::now()),
                    _ => Effect::None,
                };
                match effect {
                    Effect::Quit => {
                        self.close_logs();
                        self.close_events();
                        return Ok(());
                    }
                    Effect::Shell { id, name } => self.shell(&id, &name, terminal)?,
                    e => self.apply(e),
                }
            }
            while let Ok((generation, msg)) = self.rx.try_recv() {
                self.on_tagged(generation, msg);
            }
            if !self.loading && self.next_refresh.is_some_and(|t| Instant::now() >= t) {
                self.refresh();
            }
            if Instant::now() >= self.next_vm {
                self.next_vm = Instant::now() + VM_EVERY;
                self.app.vm = (self.engines.vm)(&self.target);
            }
        }
    }

    /// A message from a thread; dropped when it is of an older engine.
    fn on_tagged(&mut self, generation: u64, msg: Msg) {
        if generation == self.generation || matches!(msg, Msg::Update(_)) {
            self.on_msg(msg);
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
                self.soon(if self.app.live { AGES } else { self.poll });
                if let (Ok(l), false) = (result, self.asked_features) {
                    self.asked_features = true;
                    let sample = l
                        .containers
                        .iter()
                        .find(|c| c.running())
                        .map(|c| c.id.clone());
                    let (client, tx) = (self.client.clone(), self.sender());
                    std::thread::spawn(move || {
                        tx.send(Msg::Features(client.features(sample.as_deref())));
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
        let (client, tx, docker) = (self.client.clone(), self.sender(), self.target.is_docker());
        std::thread::spawn(move || {
            tx.send(Msg::Lists(load_text(&client, docker)));
        });
    }

    /// The events stream, opened again when it closes. It ends when the engine changes.
    fn watch_events(&self) {
        let (client, tx, poll) = (self.client.clone(), self.sender(), self.poll);
        let (current, slot, generation) =
            (self.current.clone(), self.events.clone(), self.generation);
        let mine = move || current.load(Ordering::SeqCst) == generation;
        std::thread::spawn(move || {
            while mine() {
                if let Ok(events) = client.events() {
                    *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(events.closer());
                    // Switched while it opened: the switch may have missed this closer.
                    if !mine() {
                        events.closer().close();
                        return;
                    }
                    if !tx.send(Msg::Live(true)) {
                        return;
                    }
                    for e in events {
                        let Ok(e) = e else { break };
                        // Exec and attach events change no list.
                        let quiet = e.action.starts_with("exec_")
                            || matches!(e.action.as_str(), "attach" | "detach" | "resize");
                        if !quiet && !tx.send(Msg::Changed) {
                            return;
                        }
                    }
                }
                if !mine() || !tx.send(Msg::Live(false)) {
                    return;
                }
                std::thread::sleep(poll);
            }
        });
    }

    fn close_events(&self) {
        if let Some(c) = self.events.lock().unwrap_or_else(|p| p.into_inner()).take() {
            c.close();
        }
    }

    fn close_logs(&mut self) {
        if let Some(c) = self.logs.take() {
            c.close();
        }
    }

    /// From podbag to Docker, or back (from a socket: to podbag). The choice is saved.
    fn switch_engine(&mut self) {
        let next = match self.target.choice {
            Choice::Podbag => Choice::Docker,
            _ => Choice::Podbag,
        };
        let target = match (self.engines.find)(&next) {
            Ok(t) => t,
            Err(e) => {
                self.app.message = Some((format!("Cannot use {}: {e}", next.label()), true));
                return;
            }
        };
        self.close_logs();
        self.generation += 1;
        self.current.store(self.generation, Ordering::SeqCst);
        self.close_events();
        self.client = Client::new(target.endpoint.clone());
        let shell = shell_command(&target, &self.exe, self.engines.docker_cli.as_deref(), "");
        self.app.switched(
            target.choice.label(),
            target.endpoint.describe(),
            shell.is_some(),
        );
        if let Err(e) = (self.engines.save)(&next) {
            self.app.message = Some((format!("Cannot save the engine choice: {e}"), true));
        }
        self.app.vm = (self.engines.vm)(&target);
        self.target = target;
        self.asked_features = false;
        // Make sure it answers (podbag may start; Docker never does), then the lists.
        self.loading = true;
        self.next_refresh = None;
        let (target, exe, client, tx) = (
            self.target.clone(),
            self.exe.clone(),
            self.client.clone(),
            self.sender(),
        );
        std::thread::spawn(move || {
            let lists = target
                .ensure_running(&exe, true)
                .and_then(|()| load_text(&client, target.is_docker()));
            tx.send(Msg::Lists(lists));
        });
        self.watch_events();
    }

    /// Run an effect in a thread (the shell, which takes the terminal, is not here).
    fn apply(&mut self, effect: Effect) {
        let (client, tx) = (self.client.clone(), self.sender());
        match effect {
            Effect::None | Effect::Quit | Effect::Shell { .. } => {}
            Effect::Refresh => self.soon(Duration::ZERO),
            Effect::CloseLogs => self.close_logs(),
            Effect::SwitchEngine => self.switch_engine(),
            Effect::Mouse(on) => {
                let _ = if on {
                    execute!(std::io::stdout(), EnableMouseCapture)
                } else {
                    execute!(std::io::stdout(), DisableMouseCapture)
                };
            }
            Effect::Act {
                kind,
                id,
                name,
                verb,
            } => {
                self.app.busy = Some(format!("{} {name}", verb.doing()));
                std::thread::spawn(move || {
                    tx.send(act(&client, kind, &id, &name, verb));
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
                    tx.send(msg);
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
                                if !tx.send(Msg::LogLine(generation, line)) {
                                    return;
                                }
                            }
                            tx.send(Msg::LogEnd(generation, None));
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
                    let done = client.pull(&image, |line| {
                        tx.send(Msg::Progress(format!("Pulling {image}: {line}")));
                    });
                    tx.send(match done {
                        Ok(()) => Msg::Done(format!("Pulled {image}"), false),
                        Err(e) => Msg::Done(format!("Cannot pull {image}: {e}"), true),
                    });
                });
            }
            Effect::Prune(kind) => {
                std::thread::spawn(move || {
                    tx.send(prune(&client, kind));
                });
            }
            Effect::Rename { id, to } => {
                std::thread::spawn(move || {
                    tx.send(match client.rename(&id, &to) {
                        Ok(()) => Msg::Done(format!("Renamed to {to}"), false),
                        Err(e) => Msg::Done(format!("Cannot rename: {e}"), true),
                    });
                });
            }
        }
    }

    /// A shell in the container on the normal screen (no mouse there), then back.
    fn shell(
        &mut self,
        id: &str,
        name: &str,
        terminal: &mut DefaultTerminal,
    ) -> std::io::Result<()> {
        let Some(mut command) = shell_command(
            &self.target,
            &self.exe,
            self.engines.docker_cli.as_deref(),
            id,
        ) else {
            self.app.message = Some(("No shell: the docker CLI is not installed".into(), true));
            return Ok(());
        };
        use ratatui::crossterm::terminal as term;
        execute!(
            std::io::stdout(),
            DisableMouseCapture,
            term::LeaveAlternateScreen
        )?;
        term::disable_raw_mode()?;
        terminal.show_cursor()?;
        let status = command.status();
        term::enable_raw_mode()?;
        execute!(std::io::stdout(), term::EnterAlternateScreen)?;
        if self.app.mouse {
            execute!(std::io::stdout(), EnableMouseCapture)?;
        }
        terminal.clear()?;
        let code = status.ok().and_then(|s| s.code());
        let text = match code {
            Some(0) => format!("The shell in {name} ended"),
            Some(c) => format!("The shell in {name} ended with exit code {c}"),
            None => format!("Cannot open a shell in {name}"),
        };
        self.app.message = Some((text, code != Some(0)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, small_engine};
    use podbag::client::{Choice, Target};
    use std::path::Path;

    #[test]
    fn the_lists_come_from_the_engine() {
        let engine = FakeEngine::start(small_engine);
        let lists = load(&engine.client).unwrap();
        assert_eq!(lists.containers.len(), 2);
        assert_eq!(lists.images.len(), 2);
        assert_eq!(lists.volumes[0].name, "pgdata");
        assert_eq!(lists.networks.len(), 4);
        let mut app = App::new("podbag".into(), "test".into());
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

    /// An engine like Docker: other containers, and pause and rename work.
    fn docker_like(method: &str, path: &str) -> podbag::client::fake::Answer {
        let path = path.split('?').next().unwrap_or(path);
        match (method, path) {
            ("GET", "/containers/json") => podbag::client::fake::json(
                200,
                serde_json::json!([{"Id": "d0c0", "Names": ["/db"], "Image": "postgres",
                    "Command": "postgres", "Created": 1700000000, "State": "running",
                    "Status": "Up 1 minute", "Ports": []}]),
            ),
            (_, p) if p.ends_with("/pause") || p.ends_with("/rename") => {
                podbag::client::fake::json(
                    404,
                    serde_json::json!({"message": "No such container: podbag-feature-check-0000"}),
                )
            }
            (m, p) => small_engine(m, p),
        }
    }

    fn target(choice: Choice, engine: &FakeEngine) -> Target {
        Target {
            choice,
            endpoint: engine.client.endpoint.clone(),
        }
    }

    /// Messages until `done` holds (or a few seconds pass).
    fn pump(rt: &mut Runtime, done: impl Fn(&Runtime) -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !done(rt) {
            assert!(
                Instant::now() < end,
                "timed out; message: {:?}",
                rt.app.message
            );
            if let Ok((generation, msg)) = rt.rx.recv_timeout(Duration::from_millis(20)) {
                rt.on_tagged(generation, msg);
            }
            if !rt.loading && rt.next_refresh.is_some_and(|t| Instant::now() >= t) {
                rt.refresh();
            }
        }
    }

    fn names(rt: &Runtime) -> Vec<String> {
        rt.app
            .lists
            .containers
            .iter()
            .map(|c| c.name().to_string())
            .collect()
    }

    #[test]
    fn e_switches_between_podbag_and_docker() {
        let podbag = std::sync::Arc::new(FakeEngine::start(small_engine));
        let docker = std::sync::Arc::new(FakeEngine::start(docker_like));
        let saved: std::sync::Arc<std::sync::Mutex<Vec<Choice>>> = Default::default();
        let (p, d, log) = (podbag.clone(), docker.clone(), saved.clone());
        let engines = Engines {
            find: Box::new(move |c| {
                Ok(match c {
                    Choice::Docker => target(Choice::Docker, &d),
                    _ => target(Choice::Podbag, &p),
                })
            }),
            save: Box::new(move |c| {
                log.lock().unwrap().push(c.clone());
                Ok(())
            }),
            vm: Box::new(|t| (t.choice == Choice::Podbag).then_some("standby")),
            docker_cli: None,
        };
        let start = target(Choice::Podbag, &podbag);
        let mut rt = Runtime::new(start, PathBuf::from("podbag"), engines);
        rt.poll = Duration::from_millis(30);
        rt.watch_events();
        assert_eq!(rt.app.engine, "podbag");
        assert_eq!(rt.app.vm, Some("standby"));
        assert!(rt.app.shell);
        pump(&mut rt, |rt| rt.app.loaded && rt.asked_features);
        assert_eq!(names(&rt), ["web", "job"]);
        pump(&mut rt, |rt| rt.app.live);
        assert!(!rt.app.features.pause);

        // To Docker: saved, a new client, the lists and the features again.
        rt.apply(Effect::SwitchEngine);
        assert_eq!(rt.app.engine, "Docker");
        assert_eq!(rt.app.vm, None);
        assert!(!rt.app.loaded);
        // No docker CLI: no shell.
        assert!(!rt.app.shell);
        assert_eq!(*saved.lock().unwrap(), [Choice::Docker]);
        pump(&mut rt, |rt| rt.app.loaded && rt.app.features.pause);
        assert_eq!(names(&rt), ["db"]);
        assert!(rt.app.features.rename);

        // The old events thread has ended: podbag hears no more of it.
        let before = podbag
            .requests()
            .iter()
            .filter(|r| *r == "GET /events")
            .count();
        std::thread::sleep(Duration::from_millis(300));
        let after = podbag
            .requests()
            .iter()
            .filter(|r| *r == "GET /events")
            .count();
        assert_eq!(before, after);
        assert!(docker.requests().iter().any(|r| r == "GET /events"));

        // A late answer of the old engine is dropped.
        rt.on_tagged(rt.generation - 1, Msg::Lists(Ok(Lists::default())));
        assert_eq!(names(&rt), ["db"]);

        // And back.
        rt.apply(Effect::SwitchEngine);
        assert_eq!(rt.app.engine, "podbag");
        assert_eq!(*saved.lock().unwrap(), [Choice::Docker, Choice::Podbag]);
        pump(&mut rt, |rt| rt.app.loaded && rt.asked_features);
        assert_eq!(names(&rt), ["web", "job"]);
        assert!(!rt.app.features.pause);
    }

    #[test]
    fn docker_that_does_not_answer_is_said_plainly() {
        let podbag = std::sync::Arc::new(FakeEngine::start(small_engine));
        let p = podbag.clone();
        let engines = Engines {
            find: Box::new(move |c| {
                Ok(match c {
                    Choice::Docker => Target {
                        choice: Choice::Docker,
                        endpoint: podbag::client::Endpoint::Socket {
                            path: "/nonexistent/docker.sock".into(),
                            own: false,
                        },
                    },
                    _ => target(Choice::Podbag, &p),
                })
            }),
            save: Box::new(|_| Ok(())),
            vm: Box::new(|_| None),
            docker_cli: None,
        };
        let mut rt = Runtime::new(
            target(Choice::Podbag, &podbag),
            PathBuf::from("podbag"),
            engines,
        );
        rt.apply(Effect::SwitchEngine);
        pump(&mut rt, |rt| rt.app.message.is_some() && !rt.loading);
        assert_eq!(
            rt.app.message,
            Some((podbag::client::choice::docker_down().to_string(), true))
        );
        // Polled again: still the same words, not a socket error.
        rt.refresh();
        pump(&mut rt, |rt| !rt.loading);
        assert_eq!(
            rt.app.message,
            Some((podbag::client::choice::docker_down().to_string(), true))
        );
    }

    #[test]
    fn the_shell_uses_the_engines_cli() {
        let exe = Path::new("/usr/local/bin/podbag");
        let docker = Path::new("/usr/bin/docker");
        let argv = |c: &std::process::Command| {
            std::iter::once(c.get_program())
                .chain(c.get_args())
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let host = |c: &std::process::Command| {
            c.get_envs()
                .find(|(k, _)| *k == "DOCKER_HOST")
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        let sock = |choice| Target {
            choice,
            endpoint: podbag::client::Endpoint::Socket {
                path: "/run/d.sock".into(),
                own: false,
            },
        };
        let c = shell_command(&sock(Choice::Podbag), exe, Some(docker), "web").unwrap();
        assert_eq!(
            argv(&c),
            ["/usr/local/bin/podbag", "exec", "-it", "web", "sh"]
        );
        assert_eq!(host(&c), None);
        let c = shell_command(&sock(Choice::Docker), exe, Some(docker), "web").unwrap();
        assert_eq!(argv(&c), ["/usr/bin/docker", "exec", "-it", "web", "sh"]);
        assert_eq!(host(&c).as_deref(), Some("unix:///run/d.sock"));
        // Without the docker CLI, Docker has no shell.
        assert!(shell_command(&sock(Choice::Docker), exe, None, "web").is_none());
        // Docker through `docker system dial-stdio`: the CLI finds it itself.
        let dial = Target {
            choice: Choice::Docker,
            endpoint: podbag::client::Endpoint::Command(vec!["docker".into()]),
        };
        let c = shell_command(&dial, exe, Some(docker), "web").unwrap();
        assert_eq!(host(&c), None);
        // Another socket: the docker CLI on it.
        let other = sock(Choice::Socket("/run/d.sock".into()));
        let c = shell_command(&other, exe, Some(docker), "web").unwrap();
        assert_eq!(host(&c).as_deref(), Some("unix:///run/d.sock"));
    }
}
