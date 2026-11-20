//! `podbag tui`: containers, images, volumes and networks in the terminal. A client of
//! the Docker API (podbag's, Docker's or a socket's): lists refresh on events
//! (`GET /events`), or every two seconds when the stream is closed. Requests run in
//! threads; the screen never waits for them.

mod app;
mod dashboard;
mod dialog;
mod mouse;
mod preferences;
mod pull;
mod resources;
mod tutorial;
mod view;

use app::{App, Effect, Lists, Msg, Verb};
use podbag::client::choice::{self, docker_down};
use podbag::client::{
    ApiError, Choice, Client, Closer, DockerTarget, Endpoint, Kind, Location, Target,
    podbag_program,
};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
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
/// How often the VM's state and the docker command's target are read (from files: it
/// never wakes the VM).
const VM_EVERY: Duration = Duration::from_secs(2);
/// A search starts when the typing stops this long.
const SEARCH_WAIT: Duration = Duration::from_millis(300);
/// The most search results shown.
const SEARCH_LIMIT: usize = 10;

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
    // An unavailable browsing engine must not hide settings, labs or other engines.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        eprintln!("podbag: the TUI needs a terminal");
        return 1;
    }
    let mut runtime = Runtime::new(target, exe, Engines::real());
    runtime.app.appearance = preferences::Appearance::load();
    if let Err(e) = ready {
        runtime.app.message = Some((e, true));
    }
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
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste
        );
        hook(info);
    }));
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    if mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    let result = runtime.run(&mut terminal);
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
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
        Ok(l) if l.errors[0].is_some() => {
            eprintln!("podbag: {}", l.errors[0].as_deref().unwrap_or_default());
            return 1;
        }
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
    fn read<T>(
        result: Result<Vec<T>, ApiError>,
        error: &mut Option<String>,
    ) -> Result<Vec<T>, ApiError> {
        match result {
            Ok(items) => Ok(items),
            Err(e) if e.status != 0 => {
                *error = Some(e.message);
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }
    let mut lists = Lists::default();
    lists.containers = read(client.containers(), &mut lists.errors[0])?;
    lists.images = read(client.images(), &mut lists.errors[1])?;
    lists.volumes = read(client.volumes(), &mut lists.errors[2])?;
    lists.networks = read(client.networks(), &mut lists.errors[3])?;
    Ok(lists)
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
/// Windows VM state is available in Settings).
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
    if id.starts_with('-')
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    {
        return None;
    }
    let argv = target.cli(exe, docker)?;
    let mut c = std::process::Command::new(&argv[0]);
    c.args(&argv[1..]).args(["exec", "-it", id, "sh"]);
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
/// The account logged in at a location.
type Login = Arc<dyn Fn(&Location) -> Option<String> + Send + Sync>;
/// The login header for a pull of this image. A secret: never shown or logged.
type Auth = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// How the TUI finds, saves and looks at engines (tests give fakes).
struct Engines {
    find: FindEngine,
    save: SaveChoice,
    vm: VmState,
    docker_cli: Option<PathBuf>,
    /// Where the `docker` command goes, and switching it (None: no docker CLI).
    docker: Option<DockerCmd>,
    login: Login,
    auth: Auth,
}

/// The `docker` command's target, and `D`'s switch.
#[derive(Clone)]
struct DockerCmd {
    target: Arc<dyn Fn() -> DockerTarget + Send + Sync>,
    use_podbag: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    /// Gives the context now in use.
    switch_back: Arc<dyn Fn() -> Result<String, String> + Send + Sync>,
    previous: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl DockerCmd {
    fn real(docker: PathBuf, podbag_socket: PathBuf) -> DockerCmd {
        let (d1, d2, s1) = (docker.clone(), docker, podbag_socket.clone());
        DockerCmd {
            target: Arc::new(move || DockerTarget::current(&podbag_socket)),
            use_podbag: Arc::new(move || podbag::client::use_podbag(&d1, &s1)),
            switch_back: Arc::new(move || podbag::client::switch_back(&d2)),
            previous: Arc::new(podbag::client::previous_context),
        }
    }
}

impl Engines {
    fn real() -> Engines {
        let exe = podbag_program();
        let docker_cli = choice::find_docker_cli();
        let podbag_socket = match Endpoint::podbag_default(&exe) {
            Ok(Endpoint::Socket { path, .. }) => Some(path),
            _ if cfg!(windows) => Some(PathBuf::new()),
            _ => None,
        };
        Engines {
            find: Box::new(move |c| Target::find(Some(c), &exe)),
            save: Box::new(choice::save),
            vm: Box::new(vm_state),
            docker: docker_cli
                .clone()
                .zip(podbag_socket)
                .map(|(cli, socket)| DockerCmd::real(cli, socket)),
            docker_cli,
            login: Arc::new(podbag::client::login_state),
            auth: Arc::new(podbag::client::registry_auth),
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
    lab: podbag::dashboard::lab::Lab,
    vm_stopped: bool,
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
    /// A search to start once the typing stops.
    pending_search: Option<(String, Instant)>,
}

impl Runtime {
    fn new(target: Target, exe: PathBuf, engines: Engines) -> Runtime {
        let (tx, rx) = channel();
        let mut app = App::new(target.choice.label(), target.endpoint.describe());
        app.shell = shell_command(&target, &exe, engines.docker_cli.as_deref(), "").is_some();
        app.vm = (engines.vm)(&target);
        if let Some(d) = &engines.docker {
            app.docker = Some((d.target)());
            app.docker_previous = (d.previous)();
        }
        Runtime {
            lab: Default::default(),
            vm_stopped: false,
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
            pending_search: None,
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
                    Event::Paste(s) => self.app.paste(&s),
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
                    Effect::Dashboard
                    | Effect::Page(_)
                    | Effect::CreateVolume
                    | Effect::ChooseRegistry
                    | Effect::RegistryLogin(_) => self.open_dashboard(terminal, effect)?,
                    e => self.apply(e),
                }
            }
            while let Ok((generation, msg)) = self.rx.try_recv() {
                self.on_tagged(generation, msg);
            }
            if !self.vm_stopped
                && !self.loading
                && self.next_refresh.is_some_and(|t| Instant::now() >= t)
            {
                self.refresh();
            }
            self.search_if_due();
            if Instant::now() >= self.next_vm {
                self.next_vm = Instant::now() + VM_EVERY;
                self.app.vm = (self.engines.vm)(&self.target);
                if let Some(d) = &self.engines.docker {
                    self.app.docker = Some((d.target)());
                    self.app.docker_previous = (d.previous)();
                }
            }
        }
    }

    fn open_dashboard(
        &mut self,
        terminal: &mut DefaultTerminal,
        effect: Effect,
    ) -> std::io::Result<()> {
        use dashboard::Change;
        if self.app.busy.is_some() {
            return Ok(());
        }
        // A watch retry can wake a stopped VM. Stop watching before VM controls open.
        self.generation += 1;
        self.current.store(self.generation, Ordering::SeqCst);
        self.close_events();
        self.close_logs();
        self.loading = false;
        self.asked_features = false;
        self.app.live = false;
        let mut ui = dialog::Screen {
            terminal,
            look: &mut self.app.appearance,
        };
        let result = match effect {
            Effect::CreateVolume => resources::volume(&mut ui, &self.client).map(|_| Change::None),
            Effect::RegistryLogin(place) => {
                resources::login(&mut ui, &self.exe, &place).map(|_| Change::None)
            }
            Effect::ChooseRegistry => resources::choose_registry(&mut ui).map(|place| {
                if let app::Mode::Pull(form) = &mut self.app.mode {
                    form.set_place(&place);
                }
                Change::None
            }),
            Effect::Page(page) => {
                dashboard::page(&mut ui, &self.target, &self.exe, &mut self.lab, page)
            }
            _ => dashboard::open(&mut ui, &self.target, &self.exe, &mut self.lab),
        };
        match result {
            Ok(Change::Engine(choice)) => {
                if self.select_engine(choice) {
                    return Ok(());
                }
            }
            Ok(Change::Vm(on)) => {
                self.vm_stopped = !on;
                if !on {
                    self.app.set_lists(Lists::default());
                    self.app.live = false;
                }
                self.app.vm = (self.engines.vm)(&self.target);
            }
            #[cfg(unix)]
            Ok(Change::UpgradeTerminal) => {
                let mut command = std::process::Command::new("sudo");
                command.arg(&self.exe).arg("upgrade");
                self.run_shell(&mut command, "podbag upgrade", terminal)?;
            }
            Ok(Change::Terminal) => {
                #[cfg(windows)]
                let mut shell = std::process::Command::new("cmd.exe");
                #[cfg(not(windows))]
                let mut shell = std::process::Command::new(
                    std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()),
                );
                self.run_shell(&mut shell, "terminal", terminal)?;
            }
            Ok(Change::Pull) => {
                let login = self.app.open_pull();
                self.apply(login);
            }
            Ok(Change::Notice(text)) => self.app.message = Some((text, false)),
            Ok(Change::None) => {
                if let app::Mode::Pull(form) = &self.app.mode {
                    self.apply(Effect::Login(form.place()));
                }
            }
            Err(dialog::Error::Back) => {}
            Err(dialog::Error::Failed(msg)) => {
                let mut screen = dialog::Screen {
                    terminal,
                    look: &mut self.app.appearance,
                };
                let title = screen.t("error.title");
                let message = screen.t(if msg == "kubectl-missing" {
                    "kube.missing"
                } else {
                    &msg
                });
                if let Err(dialog::Error::Io(e)) = screen.message(&title, &message) {
                    return Err(e);
                }
            }
            Err(dialog::Error::Io(e)) => return Err(e),
        }
        if !self.vm_stopped {
            self.watch_events();
            self.soon(Duration::ZERO);
        }
        Ok(())
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
                    {
                        let mut active = slot.lock().unwrap_or_else(|p| p.into_inner());
                        if !mine() {
                            events.closer().close();
                            return;
                        }
                        *active = Some(events.closer());
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

    /// Start the waiting search when the typing has stopped.
    fn search_if_due(&mut self) {
        let Some((term, _)) = self.pending_search.take_if(|(_, at)| Instant::now() >= *at) else {
            return;
        };
        let (client, tx) = (self.client.clone(), self.sender());
        std::thread::spawn(move || {
            let found = client.search(&term, SEARCH_LIMIT);
            tx.send(Msg::Found(term, found));
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
        self.select_engine(next);
    }

    fn select_engine(&mut self, next: Choice) -> bool {
        let target = match (self.engines.find)(&next) {
            Ok(t) => t,
            Err(e) => {
                self.app.message = Some((format!("Cannot use {}: {e}", next.label()), true));
                return false;
            }
        };
        self.vm_stopped = false;
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
        self.pending_search = None;
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
        true
    }

    /// Run an effect in a thread (the shell, which takes the terminal, is not here).
    fn apply(&mut self, effect: Effect) {
        let (client, tx) = (self.client.clone(), self.sender());
        match effect {
            Effect::None
            | Effect::Quit
            | Effect::Shell { .. }
            | Effect::Dashboard
            | Effect::Page(_)
            | Effect::CreateVolume
            | Effect::RegistryLogin(_)
            | Effect::ChooseRegistry => {}
            Effect::SaveLayout => {
                let look = &self.app.appearance;
                match preferences::Appearance::set_sidebar(
                    Some(look.sidebar),
                    Some(look.sidebar_width),
                ) {
                    Ok(saved) => self.app.appearance = saved,
                    Err(e) => {
                        self.app.message = Some((e, true));
                    }
                }
            }
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
            Effect::Login(location) => {
                if matches!(&self.app.mode, app::Mode::Pull(form) if !form.on_host)
                    && let Err(error) = resources::save_registry_host(&location)
                {
                    if let app::Mode::Pull(form) = &mut self.app.mode {
                        form.error = Some(error);
                    }
                    return;
                }
                let login = self.engines.login.clone();
                std::thread::spawn(move || {
                    let account = login(&location);
                    tx.send(Msg::Login(location, account));
                });
            }
            Effect::Search(term) => {
                self.pending_search = Some((term, Instant::now() + SEARCH_WAIT));
            }
            Effect::DockerCli { to_podbag } => {
                let Some(d) = self.engines.docker.clone() else {
                    return;
                };
                self.app.busy = Some("Switching the docker command".into());
                std::thread::spawn(move || {
                    let done = if to_podbag {
                        (d.use_podbag)().map(|()| "podbag".to_string())
                    } else {
                        (d.switch_back)()
                    };
                    tx.send(Msg::DockerTarget((d.target)(), (d.previous)()));
                    tx.send(match done {
                        Ok(to) => Msg::Done(format!("The docker command now goes to {to}"), false),
                        Err(e) => Msg::Done(format!("Cannot switch the docker command: {e}"), true),
                    });
                });
            }
            Effect::Pull(image) => {
                let local = self.target.allows_registry_auth();
                let auth = self.engines.auth.clone();
                std::thread::spawn(move || {
                    // The login stays in this thread: it is sent to the engine only.
                    let login = if local { auth(&image) } else { None };
                    let done = client.pull_auth(&image, login.as_deref(), |line| {
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
        self.run_shell(&mut command, name, terminal)
    }

    fn run_shell(
        &mut self,
        command: &mut std::process::Command,
        name: &str,
        terminal: &mut DefaultTerminal,
    ) -> std::io::Result<()> {
        use ratatui::crossterm::terminal as term;
        execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            term::LeaveAlternateScreen
        )?;
        term::disable_raw_mode()?;
        terminal.show_cursor()?;
        let status = command.status();
        term::enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            term::EnterAlternateScreen,
            EnableBracketedPaste
        )?;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, small_engine};
    use podbag::client::{Choice, DockerTarget, Location, Target};
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
                "DELETE /images/sha256:22",
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
            docker: None,
            login: Arc::new(|_| None),
            auth: Arc::new(|_| None),
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
            docker: None,
            login: Arc::new(|_| None),
            auth: Arc::new(|_| None),
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

    /// Engines with only `engine`, and the given login and auth.
    fn one_engine(engine: &FakeEngine, auth: Auth, docker: Option<DockerCmd>) -> Runtime {
        let t = target(Choice::Podbag, engine);
        let found = t.clone();
        let engines = Engines {
            find: Box::new(move |_| Ok(found.clone())),
            save: Box::new(|_| Ok(())),
            vm: Box::new(|_| None),
            docker_cli: None,
            docker,
            login: Arc::new(|l| (*l == Location::GitHub).then(|| "octo".to_string())),
            auth,
        };
        Runtime::new(t, PathBuf::from("podbag"), engines)
    }

    fn wait_for(rt: &mut Runtime, done: impl Fn(&Msg) -> bool) -> Msg {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < end, "timed out");
            if let Ok((_, msg)) = rt.rx.recv_timeout(Duration::from_millis(20)) {
                if done(&msg) {
                    return msg;
                }
                rt.on_msg(msg);
            }
        }
    }

    #[test]
    fn a_pull_sends_the_login_of_its_registry() {
        let engine = FakeEngine::start(small_engine);
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = asked.clone();
        let auth: Auth = Arc::new(move |image| {
            log.lock().unwrap().push(image.to_string());
            Some(format!("made-at-run-time-{}", image.len()))
        });
        let mut rt = one_engine(&engine, auth, None);
        rt.apply(Effect::Pull("ghcr.io/me/app:1".into()));
        let done = wait_for(&mut rt, |m| matches!(m, Msg::Done(..)));
        assert!(
            matches!(done, Msg::Done(ref t, false) if t == "Pulled ghcr.io/me/app:1"),
            "{done:?}"
        );
        assert_eq!(*asked.lock().unwrap(), ["ghcr.io/me/app:1"]);
        assert!(
            engine
                .requests()
                .contains(&"POST /images/create?fromImage=ghcr.io%2Fme%2Fapp&tag=1".to_string())
        );
        // The login of a location.
        rt.apply(Effect::Login(Location::GitHub));
        let m = wait_for(&mut rt, |m| matches!(m, Msg::Login(..)));
        assert!(
            matches!(m, Msg::Login(Location::GitHub, Some(ref u)) if u == "octo"),
            "{m:?}"
        );
    }

    #[test]
    fn search_waits_for_typing_to_stop() {
        let docker = FakeEngine::start(|method, path| match path.split('?').next() {
            Some("/images/search") => podbag::client::fake::json(
                200,
                serde_json::json!([{"name": "nginx", "description": "web", "star_count": 9,
                    "is_official": true}]),
            ),
            _ => small_engine(method, path),
        });
        let mut rt = one_engine(&docker, Arc::new(|_| None), None);
        rt.apply(Effect::Search("ng".into()));
        rt.apply(Effect::Search("ngi".into()));
        let end = Instant::now() + Duration::from_secs(5);
        let found = loop {
            assert!(Instant::now() < end, "timed out");
            rt.search_if_due();
            if let Ok((_, m @ Msg::Found(..))) = rt.rx.recv_timeout(Duration::from_millis(20)) {
                break m;
            }
        };
        let Msg::Found(term, Ok(results)) = found else {
            panic!("{found:?}")
        };
        assert_eq!(term, "ngi");
        assert_eq!(results[0].name, "nginx");
        let searches: Vec<String> = docker
            .requests()
            .into_iter()
            .filter(|r| r.contains("/images/search"))
            .collect();
        assert_eq!(searches, ["GET /images/search?term=ngi&limit=10"]);
    }

    #[test]
    fn d_runs_the_docker_cli_switch_and_shows_the_new_target() {
        let engine = FakeEngine::start(small_engine);
        let at_podbag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (a, b, c) = (at_podbag.clone(), at_podbag.clone(), at_podbag.clone());
        let docker = DockerCmd {
            target: Arc::new(move || DockerTarget {
                context: Some(
                    if a.load(Ordering::SeqCst) {
                        "podbag"
                    } else {
                        "desktop-linux"
                    }
                    .into(),
                ),
                host: "unix:///x.sock".into(),
                name: if a.load(Ordering::SeqCst) {
                    "podbag"
                } else {
                    "Docker Desktop"
                }
                .into(),
                is_podbag: a.load(Ordering::SeqCst),
                from_env: false,
            }),
            use_podbag: Arc::new(move || {
                b.store(true, Ordering::SeqCst);
                Ok(())
            }),
            switch_back: Arc::new(move || {
                c.store(false, Ordering::SeqCst);
                Ok("desktop-linux".into())
            }),
            previous: Arc::new(|| Some("desktop-linux".into())),
        };
        let mut rt = one_engine(&engine, Arc::new(|_| None), Some(docker));
        assert_eq!(rt.app.docker.as_ref().unwrap().name, "Docker Desktop");
        rt.apply(Effect::DockerCli { to_podbag: true });
        let done = wait_for(&mut rt, |m| matches!(m, Msg::Done(..)));
        assert!(
            matches!(done, Msg::Done(ref t, false) if t.contains("podbag")),
            "{done:?}"
        );
        assert_eq!(rt.app.docker.as_ref().unwrap().name, "podbag");
        assert_eq!(rt.app.docker_previous.as_deref(), Some("desktop-linux"));
        rt.apply(Effect::DockerCli { to_podbag: false });
        let done = wait_for(&mut rt, |m| matches!(m, Msg::Done(..)));
        assert!(
            matches!(done, Msg::Done(ref t, false) if t.contains("desktop-linux")),
            "{done:?}"
        );
        assert_eq!(rt.app.docker.as_ref().unwrap().name, "Docker Desktop");
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
        assert_eq!(
            argv(&c),
            [
                "/usr/bin/docker",
                "--host=unix:///run/d.sock",
                "exec",
                "-it",
                "web",
                "sh"
            ]
        );
        assert!(argv(&c).contains(&"--host=unix:///run/d.sock".into()));
        // Without the docker CLI, Docker has no shell.
        assert!(shell_command(&sock(Choice::Docker), exe, None, "web").is_none());
        // Docker through `docker system dial-stdio`: the CLI finds it itself.
        let dial = Target {
            choice: Choice::Docker,
            endpoint: podbag::client::Endpoint::Command(vec![
                "docker".into(),
                "--context=team".into(),
                "system".into(),
                "dial-stdio".into(),
            ]),
        };
        let c = shell_command(&dial, exe, Some(docker), "web").unwrap();
        assert!(argv(&c).contains(&"--context=team".into()));
        assert_eq!(host(&c), None);
        // Another socket: the docker CLI on it.
        let other = sock(Choice::Socket("/run/d.sock".into()));
        let c = shell_command(&other, exe, Some(docker), "web").unwrap();
        assert!(argv(&c).contains(&"--host=unix:///run/d.sock".into()));
    }
}
