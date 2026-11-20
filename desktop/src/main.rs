//! podbag's desktop app: a window for containers, images, volumes and networks, and a
//! tray (menu bar) item. A client of the Docker API on podbag's socket, like
//! `podbag tui`. The page is static HTML, CSS and JavaScript in `ui/`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod cluster_migration;
mod commands;
mod dockercmd;
mod images;
mod kubernetes;
mod lab;
mod migration;
mod settings;
mod strings;

use commands::EngineStatus;
use podbag::client::{Choice, Client, Closer, Endpoint, Features, Target, podbag_program};
use serde::Serialize;
use serde_json::Value;
use settings::Settings;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent, Wry};

/// What the tray shows of the engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum EngineState {
    #[default]
    Unknown,
    Ready,
    Down,
    VmRunning,
    VmStopped,
}

struct TrayItems {
    menu: Menu<Wry>,
    engine: MenuItem<Wry>,
    /// "Update available": in the menu only while one is known.
    update: MenuItem<Wry>,
    update_shown: bool,
    status: MenuItem<Wry>,
    running: MenuItem<Wry>,
    open: MenuItem<Wry>,
    vm: Option<MenuItem<Wry>>,
    /// Where the `docker` command goes: podbag, or back where it was.
    docker: Submenu<Wry>,
    docker_podbag: CheckMenuItem<Wry>,
    docker_other: CheckMenuItem<Wry>,
    settings: MenuItem<Wry>,
    quit: MenuItem<Wry>,
}

struct AppState {
    migration: Mutex<()>,
    /// The engine this window uses, and its client.
    session: Mutex<Arc<Session>>,
    podbag: PathBuf,
    /// The engine's version (`podbag --version`), once asked.
    engine_version: Mutex<Option<String>>,
    version_asked: AtomicBool,
    /// Where the `docker` command goes, as last read (files only).
    docker: Mutex<Option<dockercmd::DockerCommand>>,
    /// The window and tray share the same Docker CLI configuration.
    docker_switch: Mutex<()>,
    star_prompt: AtomicBool,
    settings: Mutex<Settings>,
    settings_file: Mutex<Option<PathBuf>>,
    visible: AtomicBool,
    tray: Mutex<Option<TrayItems>>,
    /// macOS: Settings… in the app menu.
    app_settings: Mutex<Option<MenuItem<Wry>>>,
}

struct Session {
    id: u64,
    retired: AtomicBool,
    target: Target,
    client: Client,
    /// The events stream, while the window is open.
    watching: AtomicBool,
    watch_generation: AtomicU64,
    watch: Mutex<Option<Closer>>,
    /// The open log stream and its generation.
    logs: Mutex<Option<Closer>>,
    log_generation: AtomicU64,
    engine: Mutex<EngineState>,
    /// Running containers, as last seen.
    running: Mutex<Option<usize>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl AppState {
    fn new() -> AppState {
        let podbag = podbag_program();
        let target = Target::find(None, &podbag).unwrap_or_else(|_| Target {
            choice: Choice::Podbag,
            endpoint: Endpoint::Socket {
                path: PathBuf::from("docker.sock"),
                own: false,
            },
        });
        AppState {
            migration: Mutex::new(()),
            session: Mutex::new(Arc::new(Session::new(1, target))),
            podbag,
            engine_version: Mutex::new(None),
            version_asked: AtomicBool::new(false),
            docker: Mutex::new(None),
            docker_switch: Mutex::new(()),
            star_prompt: AtomicBool::new(false),
            settings: Mutex::new(Settings::default()),
            settings_file: Mutex::new(None),
            visible: AtomicBool::new(true),
            tray: Mutex::new(None),
            app_settings: Mutex::new(None),
        }
    }

    fn current(&self) -> Arc<Session> {
        lock(&self.session).clone()
    }

    fn session(&self, id: u64) -> Result<Arc<Session>, String> {
        let session = self.current();
        if session.id != id {
            return Err("The engine has changed. Reopen this view.".into());
        }
        Ok(session)
    }

    fn switch(&self, token: u64, name: &str, file: &std::path::Path) -> Result<(), String> {
        let mut current = lock(&self.session);
        if current.id != token {
            return Err("The engine has changed.".into());
        }
        let target = commands::choose(name, file, &self.podbag)?;
        current.retired.store(true, Ordering::SeqCst);
        current.stop_streams();
        *current = Arc::new(Session::new(token + 1, target));
        Ok(())
    }

    /// The engine's version, asked once (`podbag --version` needs no engine).
    fn engine_version(&self) -> Option<String> {
        if !self.version_asked.swap(true, Ordering::SeqCst) {
            *lock(&self.engine_version) = cli::engine_version(&self.podbag);
        }
        lock(&self.engine_version).clone()
    }

    /// A newer release, as the last check found it (a file; no network).
    fn known_update(&self) -> Option<String> {
        let (_, latest) = cli::saved_check(&update_file()?)?;
        let current = lock(&self.engine_version).clone()?;
        cli::is_newer(&latest, &current).then_some(latest)
    }

    fn language(&self) -> &'static str {
        strings::resolve(&lock(&self.settings).language, sys_locale::get_locale())
    }
}

impl Session {
    fn new(id: u64, target: Target) -> Self {
        Self {
            id,
            retired: AtomicBool::new(false),
            client: Client::new(target.endpoint.clone()),
            target,
            watching: AtomicBool::new(false),
            watch_generation: AtomicU64::new(0),
            watch: Mutex::new(None),
            logs: Mutex::new(None),
            log_generation: AtomicU64::new(0),
            engine: Mutex::new(EngineState::Unknown),
            running: Mutex::new(None),
        }
    }

    fn emit(
        &self,
        app: &AppHandle,
        event: &str,
        value: impl Serialize + Clone,
    ) -> tauri::Result<()> {
        app.emit(&format!("{event}-{}", self.id), value)
    }

    fn stop_watch(&self) {
        self.watching.store(false, Ordering::SeqCst);
        self.watch_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = lock(&self.watch).take() {
            c.close();
        }
    }

    fn stop_streams(&self) {
        self.stop_watch();
        self.log_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = lock(&self.logs).take() {
            c.close();
        }
    }
}

// The window's commands. They run off the main thread: requests may take a while.

#[tauri::command]
fn connection(state: State<'_, AppState>) -> (u64, String, bool) {
    let current = state.current();
    (
        current.id,
        current.target.choice.to_setting(),
        current.target.choice == Choice::Podbag,
    )
}

#[tauri::command(async)]
fn contexts() -> Vec<String> {
    podbag::client::choice::docker_contexts()
}

#[tauri::command(async)]
fn engine(
    app: AppHandle,
    state: State<'_, AppState>,
    token: u64,
    start: bool,
) -> Result<EngineStatus, String> {
    let session = state.session(token)?;
    let status = commands::engine(&session.target, &session.client, &state.podbag, start);
    *lock(&session.engine) = match (status.vm.as_deref(), status.reachable) {
        (Some("running"), _) => EngineState::VmRunning,
        (Some(_), false) => EngineState::VmStopped,
        (_, true) => EngineState::Ready,
        (_, false) => EngineState::Down,
    };
    update_tray(&app);
    Ok(status)
}

#[tauri::command(async)]
fn list(
    app: AppHandle,
    state: State<'_, AppState>,
    token: u64,
    kind: String,
) -> Result<Value, String> {
    let session = state.session(token)?;
    let v = commands::list(&session.client, &kind)?;
    if kind == "containers" {
        *lock(&session.running) = Some(commands::running(&v));
        update_tray(&app);
    }
    Ok(v)
}

#[tauri::command(async)]
fn inspect(
    state: State<'_, AppState>,
    token: u64,
    kind: String,
    id: String,
) -> Result<Value, String> {
    let session = state.session(token)?;
    commands::inspect(&session.client, &kind, &id)
}

#[tauri::command(async)]
fn act(state: State<'_, AppState>, token: u64, id: String, action: String) -> Result<(), String> {
    let session = state.session(token)?;
    commands::act(&session.client, &id, &action)
}

#[tauri::command(async)]
fn remove(
    state: State<'_, AppState>,
    token: u64,
    kind: String,
    id: String,
    force: bool,
) -> Result<(), String> {
    let session = state.session(token)?;
    commands::remove(&session.client, &kind, &id, force)
}

#[tauri::command(async)]
fn create_volume(state: State<'_, AppState>, token: u64, name: String) -> Result<(), String> {
    let session = state.session(token)?;
    session
        .client
        .call(
            "POST",
            "/volumes/create",
            Some(&serde_json::json!({ "Name": name })),
        )
        .map(|_| ())
        .map_err(|e| e.message)
}

#[tauri::command(async)]
fn prune(
    state: State<'_, AppState>,
    token: u64,
    kind: String,
) -> Result<podbag::client::Pruned, String> {
    let session = state.session(token)?;
    commands::prune(&session.client, &kind)
}

#[tauri::command(async)]
fn rename(state: State<'_, AppState>, token: u64, id: String, name: String) -> Result<(), String> {
    let session = state.session(token)?;
    commands::rename(&session.client, &id, &name)
}

#[tauri::command(async)]
fn container_json(
    state: State<'_, AppState>,
    token: u64,
    id: String,
    what: String,
) -> Result<Value, String> {
    let session = state.session(token)?;
    commands::container_json(&session.client, &id, &what)
}

#[tauri::command(async)]
fn features(
    state: State<'_, AppState>,
    token: u64,
    sample: Option<String>,
) -> Result<Features, String> {
    let session = state.session(token)?;
    Ok(session.client.features(sample.as_deref()))
}

#[tauri::command(async)]
/// Pull `image` from `location` (with `host` for ECR, GAR, ACR or another registry), with the
/// login for its registry when the engine is local. Gives the reference pulled.
fn pull(
    app: AppHandle,
    state: State<'_, AppState>,
    token: u64,
    image: String,
    location: Option<String>,
    host: Option<String>,
) -> Result<String, String> {
    let session = state.session(token)?;
    let location = match &location {
        Some(id) => Some(images::location(id, host.as_deref().unwrap_or(""))?),
        None => None,
    };
    let reference = images::reference(&image, location.as_ref())?;
    images::pull(
        &session.client,
        &session.target,
        &reference,
        podbag::client::registry_auth,
        |line| {
            let _ = session.emit(&app, "pull-progress", line);
        },
    )?;
    Ok(reference)
}

/// Search Docker Hub through the engine.
#[tauri::command(async)]
fn search(state: State<'_, AppState>, token: u64, term: String) -> Result<images::Search, String> {
    Ok(images::search(&state.session(token)?.client, &term))
}

/// The pull locations and who is logged in there (account names only).
#[tauri::command(async)]
fn registries(hosts: Vec<(String, String)>) -> Vec<images::LocationInfo> {
    images::locations(&images::current_logins(), &hosts)
}

/// Log in to a location. The password goes to the login program's stdin only.
#[tauri::command(async)]
fn registry_login(
    state: State<'_, AppState>,
    location: String,
    host: String,
    user: String,
    password: String,
) -> Result<(), String> {
    let loc = images::location(&location, &host)?;
    if loc.needs_host() && loc.host().is_empty() {
        return Err("the registry needs a host".into());
    }
    let server = if loc == podbag::client::Location::DockerHub {
        String::new()
    } else {
        loc.host()
    };
    let docker = podbag::client::find_docker_cli();
    podbag::client::login(&state.podbag, docker.as_deref(), &server, &user, &password)
}

/// podbag's own socket (where `docker` goes after "Use podbag for docker").
fn podbag_socket(podbag: &std::path::Path) -> PathBuf {
    match Endpoint::podbag_default(podbag) {
        Ok(Endpoint::Socket { path, .. }) => path,
        _ => PathBuf::new(),
    }
}

/// Where the `docker` command goes. It reads files only.
#[tauri::command(async)]
fn docker_command(state: State<'_, AppState>) -> dockercmd::DockerCommand {
    let now = dockercmd::current(&podbag_socket(&state.podbag));
    *lock(&state.docker) = Some(now.clone());
    now
}

/// Point the `docker` command at podbag (`use`), or back where it was (`back`).
#[tauri::command(async)]
fn docker_switch(app: AppHandle, to: String) -> Result<dockercmd::DockerCommand, String> {
    switch_docker(&app, &to)
}

fn switch_docker(app: &AppHandle, to: &str) -> Result<dockercmd::DockerCommand, String> {
    let state = app.state::<AppState>();
    let _switch = state
        .docker_switch
        .try_lock()
        .map_err(|_| "a Docker CLI switch is already in progress")?;
    let docker = podbag::client::find_docker_cli().ok_or("the docker CLI is not installed")?;
    let socket = podbag_socket(&state.podbag);
    let _ = app.emit("docker-switching", to);
    let result = match to {
        "use" => podbag::client::use_podbag(&docker, &socket),
        "back" => podbag::client::switch_back(&docker).map(|_| ()),
        _ => Err(format!("unknown switch {to}")),
    };
    let now = dockercmd::current(&socket);
    *lock(&state.docker) = Some(now.clone());
    update_tray(app);
    let _ = app.emit("docker-changed", &now);
    result.map(|()| now)
}

fn docker_client(podbag: &std::path::Path) -> Result<Client, String> {
    Target::find(Some(&Choice::Docker), podbag).map(|t| Client::new(t.endpoint))
}

/// Docker's images, to copy to podbag. Only read; Docker is never started.
#[tauri::command(async)]
fn docker_images(
    state: State<'_, AppState>,
    hosts: Option<Vec<(String, String)>>,
) -> Result<Vec<images::ImportImage>, String> {
    let client = docker_client(&state.podbag)?;
    if client.ping().is_err() {
        return Err(podbag::client::choice::docker_down().into());
    }
    let configured: Vec<_> = hosts
        .unwrap_or_default()
        .iter()
        .filter(|(id, host)| id != "custom" && !host.is_empty())
        .filter_map(|(id, host)| images::location(id, host).ok())
        .filter(|location| location.needs_host())
        .collect();
    let images = client.images().map_err(|e| e.message)?;
    Ok(images
        .into_iter()
        .filter_map(|image| images::ImportImage::from_image(image, &configured))
        .collect())
}

/// Copy images from Docker to podbag (this window must use podbag).
#[tauri::command(async)]
fn import_images(state: State<'_, AppState>, token: u64, names: Vec<String>) -> Result<(), String> {
    let session = state.session(token)?;
    if session.target.choice != Choice::Podbag {
        return Err("this window does not use podbag".into());
    }
    let from = docker_client(&state.podbag)?;
    images::import(&from, &session.client, &names)
}

#[derive(Clone, Serialize)]
struct LogBatch {
    generation: u64,
    lines: Vec<String>,
}

/// Follow a container's log: `logs` events with this generation, until `logs_stop`.
#[tauri::command(async)]
fn logs_start(
    app: AppHandle,
    state: State<'_, AppState>,
    token: u64,
    id: String,
) -> Result<u64, String> {
    let session = state.session(token)?;
    if let Some(c) = lock(&session.logs).take() {
        c.close();
    }
    let generation = session.log_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let lines = session.client.logs(&id, 500, true).map_err(|e| e.message)?;
    {
        let mut active = lock(&session.logs);
        if session.retired.load(Ordering::SeqCst)
            || generation != session.log_generation.load(Ordering::SeqCst)
        {
            lines.closer().close();
            return Err("The log view has closed.".into());
        }
        *active = Some(lines.closer());
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        commands::follow_logs(lines, move |lines| {
            let current = session.log_generation.load(Ordering::SeqCst);
            current == generation
                && session
                    .emit(&handle, "logs", LogBatch { generation, lines })
                    .is_ok()
        });
    });
    Ok(generation)
}

#[tauri::command(async)]
fn logs_stop(state: State<'_, AppState>, token: u64) -> Result<(), String> {
    let session = state.session(token)?;
    session.log_generation.fetch_add(1, Ordering::SeqCst);
    if let Some(c) = lock(&session.logs).take() {
        c.close();
    }
    Ok(())
}

/// Watch the engine's events while `on`: a `changed` event for each change.
#[tauri::command(async)]
fn watch(app: AppHandle, state: State<'_, AppState>, token: u64, on: bool) -> Result<(), String> {
    let session = state.session(token)?;
    if !on {
        session.stop_watch();
        return Ok(());
    }
    if session.watching.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    // A thread of an earlier watch that still sleeps sees another generation and ends.
    let generation = session.watch_generation.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let current = || {
            !session.retired.load(Ordering::SeqCst)
                && session.watching.load(Ordering::SeqCst)
                && session.watch_generation.load(Ordering::SeqCst) == generation
        };
        while current() {
            if let Ok(events) = session.client.events() {
                {
                    let mut stream = lock(&session.watch);
                    if !current() {
                        events.closer().close();
                        break;
                    }
                    *stream = Some(events.closer());
                }
                let _ = session.emit(&app, "live", true);
                for e in events {
                    let Ok(e) = e else { break };
                    let quiet = e.action.starts_with("exec_")
                        || matches!(e.action.as_str(), "attach" | "detach" | "resize");
                    if !quiet {
                        let _ = session.emit(&app, "changed", e.kind);
                    }
                }
            }
            let _ = session.emit(&app, "live", false);
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    Ok(())
}

#[tauri::command(async)]
fn open_shell(state: State<'_, AppState>, token: u64, id: String) -> Result<(), String> {
    let session = state.session(token)?;
    let docker = podbag::client::choice::docker_cli();
    let shell = commands::shell_command(&session.target, &state.podbag, &docker, &id)?;
    commands::open_shell(&shell)
}

/// Query the companion CLI so preferences and boot always use the same VM home.
#[tauri::command(async)]
fn vm_resources(
    state: State<'_, AppState>,
    token: u64,
    values: Option<VmResources>,
) -> Result<Value, String> {
    let session = state.session(token)?;
    if !cfg!(target_os = "macos") || session.target.choice != Choice::Podbag {
        return Err("VM resources require the local macOS engine".into());
    }
    let mut command = std::process::Command::new(&state.podbag);
    command.args(["vm", "resources"]);
    if let Some(values) = values {
        command.args([values.cpus.to_string(), values.memory_mib.to_string()]);
    }
    let out = command
        .output()
        .map_err(|e| format!("{}: {e}", state.podbag.display()))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VmResources {
    cpus: usize,
    memory_mib: u64,
}

/// A terminal window: `plain`, or `upgrade` (`sudo podbag upgrade`, where the user types
/// the password). The error is the command to run by hand.
#[tauri::command(async)]
fn open_terminal(state: State<'_, AppState>, what: String) -> Result<(), String> {
    match what.as_str() {
        "plain" => commands::open_terminal(None)
            .then_some(())
            .ok_or_else(String::new),
        "upgrade" if !cfg!(windows) => {
            let argv = [
                "sudo".to_string(),
                state.podbag.display().to_string(),
                "upgrade".to_string(),
            ];
            commands::open_terminal(Some(&argv))
                .then_some(())
                .ok_or_else(|| cli::admin_command(&state.podbag))
        }
        _ => Err(format!("unknown terminal {what}")),
    }
}

/// `podbag vm start`, `stop` or `restart` (macOS and Windows).
#[tauri::command(async)]
fn vm(
    app: AppHandle,
    state: State<'_, AppState>,
    token: u64,
    action: String,
) -> Result<(), String> {
    let session = state.session(token)?;
    run_vm(&app, &session, &action)
}

fn run_vm(app: &AppHandle, session: &Session, action: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    if session.target.choice != Choice::Podbag {
        return Err("VM controls require Podbag".into());
    }
    if matches!(action, "stop" | "restart") {
        session.stop_streams();
    }
    let done = cli::vm_action(&state.podbag, action);
    refresh_tray(app, false);
    let _ = session.emit(app, "engine-changed", action);
    done
}

/// `podbag vm status --json`: null when the engine is not podbag, or on Linux (no VM).
#[tauri::command(async)]
fn vm_status(state: State<'_, AppState>, token: u64) -> Result<Value, String> {
    if state.session(token)?.target.choice != Choice::Podbag || commands::platform() == "linux" {
        return Ok(Value::Null);
    }
    cli::vm_status(&state.podbag)
}

/// `podbag vm set`: what it printed. With `restart`, the VM restarts now.
#[tauri::command(async)]
fn vm_set(app: AppHandle, token: u64, settings: cli::VmSettings) -> Result<String, String> {
    let state = app.state::<AppState>();
    let session = state.session(token)?;
    if session.target.choice != Choice::Podbag {
        return Err("VM controls require Podbag".into());
    }
    if settings.restart {
        session.stop_streams();
    }
    let done = cli::vm_set(&state.podbag, &settings);
    if settings.restart {
        refresh_tray(&app, false);
        let _ = session.emit(&app, "engine-changed", "restart");
    }
    done
}

/// Switch this window to `podbag` or `docker`; saved for the TUI too.
#[tauri::command(async)]
fn set_engine(
    app: AppHandle,
    state: State<'_, AppState>,
    token: u64,
    engine: String,
) -> Result<(), String> {
    let file = podbag::client::ui_dir()
        .ok_or("cannot find the folder for ui.json")?
        .join("ui.json");
    state.switch(token, &engine, &file)?;
    update_tray(&app);
    Ok(())
}

fn update_file() -> Option<PathBuf> {
    podbag::client::ui_dir().map(|d| d.join("update.json"))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The versions and the last update check. `check`: `never`, `due` (when the window
/// opens: at most once a day) or `now`.
#[tauri::command(async)]
fn updates(app: AppHandle, state: State<'_, AppState>, check: String) -> cli::Updates {
    let check = match check.as_str() {
        "now" => cli::Check::Now,
        "due" => cli::Check::IfDue,
        _ => cli::Check::Never,
    };
    let file = update_file();
    let u = cli::updates(&state.podbag, file.as_deref(), check, unix_now());
    *lock(&state.engine_version) = u.engine.clone();
    state.version_asked.store(true, Ordering::SeqCst);
    update_tray(&app);
    u
}

/// `podbag upgrade --yes`.
#[tauri::command(async)]
fn upgrade(app: AppHandle, state: State<'_, AppState>) -> cli::Upgraded {
    let done = cli::upgrade(&state.podbag);
    if done.ok {
        *lock(&state.engine_version) = cli::engine_version(&state.podbag);
        update_tray(&app);
    }
    done
}

#[tauri::command]
fn take_star_prompt(state: State<'_, AppState>) -> bool {
    state.star_prompt.swap(false, Ordering::SeqCst)
}

#[tauri::command(async)]
fn open_repository() -> Result<(), String> {
    open::that_detached(env!("CARGO_PKG_REPOSITORY")).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    lock(&state.settings).clone()
}

#[tauri::command]
fn set_settings(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    let settings = settings.checked();
    if settings.theme != lock(&state.settings).theme {
        window
            .set_theme(settings.theme.native())
            .map_err(|e| e.to_string())?;
    }
    *lock(&state.settings) = settings.clone();
    update_tray(window.app_handle());
    match lock(&state.settings_file).as_ref() {
        Some(file) => settings.save(file),
        None => Ok(()),
    }
}

// The tray.

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    app.state::<AppState>()
        .visible
        .store(true, Ordering::SeqCst);
    let _ = app.emit("shown", ());
}

/// Open the window (or bring it forward) on the Settings page.
fn open_settings(app: &AppHandle) {
    show_window(app);
    let _ = app.emit("open-settings", ());
}

fn update_tray(app: &AppHandle) {
    let state = app.state::<AppState>();
    let session = state.current();
    let lang = state.language();
    let engine = *lock(&session.engine);
    let running = *lock(&session.running);
    let target = &session.target;
    let update = state.known_update();
    let docker = lock(&state.docker).clone();
    if let Some(item) = lock(&state.app_settings).as_ref() {
        let _ = item.set_text(strings::text(lang, "tray.settings", None));
    }
    let mut tray = lock(&state.tray);
    let Some(items) = tray.as_mut() else {
        return;
    };
    let t = |key: &str| strings::text(lang, key, None);
    let _ = items.engine.set_text(strings::fill(
        lang,
        "tray.engine",
        &[("name", &target.choice.label())],
    ));
    match &update {
        Some(v) => {
            let _ = items
                .update
                .set_text(strings::fill(lang, "tray.update", &[("version", v)]));
            if !items.update_shown {
                // After the separator, above Open podbag.
                items.update_shown = items.menu.insert(&items.update, 4).is_ok();
            }
        }
        None if items.update_shown => {
            items.update_shown = items.menu.remove(&items.update).is_err();
        }
        None => {}
    }
    let status = match engine {
        EngineState::Unknown => t("tray.checking"),
        EngineState::Ready => t("tray.engineRunning"),
        EngineState::Down => t("tray.engineDown"),
        EngineState::VmRunning => t("tray.vmRunning"),
        EngineState::VmStopped => t("tray.vmStopped"),
    };
    let _ = items.status.set_text(status);
    let running = match (engine, running) {
        (EngineState::VmStopped, _) => strings::text(lang, "tray.running", Some(0)),
        (_, Some(n)) => strings::text(lang, "tray.running", Some(n)),
        (_, None) => t("tray.runningUnknown"),
    };
    let _ = items.running.set_text(running);
    let _ = items.open.set_text(t("tray.open"));
    let _ = items.settings.set_text(t("tray.settings"));
    match &docker {
        Some(d) if d.installed => {
            let _ = items
                .docker
                .set_text(strings::fill(lang, "tray.docker", &[("name", &d.name)]));
            let _ = items.docker.set_enabled(true);
            let _ = items.docker_podbag.set_checked(d.is_podbag);
            let _ = items.docker_podbag.set_enabled(!d.is_podbag);
            let other = if d.is_podbag {
                let back = d.previous.clone().unwrap_or_else(|| "default".into());
                strings::fill(lang, "tray.dockerBack", &[("name", &back)])
            } else {
                d.name.clone()
            };
            let _ = items.docker_other.set_text(other);
            let _ = items.docker_other.set_checked(!d.is_podbag);
            let _ = items.docker_other.set_enabled(d.is_podbag);
        }
        _ => {
            let _ = items.docker.set_text(t("tray.dockerMissing"));
            let _ = items.docker.set_enabled(false);
        }
    }
    let _ = items.quit.set_text(t("tray.quit"));
    if let Some(vm) = &items.vm {
        let key = if engine == EngineState::VmRunning {
            "tray.stopVm"
        } else {
            "tray.startVm"
        };
        let _ = vm.set_text(t(key));
        let _ = vm.set_enabled(engine != EngineState::Unknown && target.choice == Choice::Podbag);
    }
}

/// Read the engine's state for the tray. The VM's state costs nothing (macOS reads
/// files); the API is asked only while the window is open, or when `asked` (a click on
/// the tray), so a hidden app keeps no VM or service awake.
fn refresh_tray(app: &AppHandle, asked: bool) {
    let state = app.state::<AppState>();
    let session = state.current();
    // Once: `podbag --version` needs no engine and wakes nothing.
    let _ = state.engine_version();
    // The docker CLI's files only.
    *lock(&state.docker) = Some(dockercmd::current(&podbag_socket(&state.podbag)));
    let own = session.target.choice == Choice::Podbag
        && matches!(session.client.endpoint, Endpoint::Socket { own: true, .. });
    let ask_api = asked || state.visible.load(Ordering::SeqCst);
    let vm = if own && cfg!(target_os = "macos") {
        commands::vm_status(&state.podbag)
    } else {
        None
    };
    let engine = match vm.as_deref() {
        Some("running") => EngineState::VmRunning,
        Some(_) => EngineState::VmStopped,
        None if ask_api => {
            if session.client.ping().is_ok() {
                EngineState::Ready
            } else {
                EngineState::Down
            }
        }
        None => *lock(&session.engine),
    };
    let previous = std::mem::replace(&mut *lock(&session.engine), engine);
    if previous != engine
        && let Some(vm) = vm
    {
        let _ = session.emit(app, "vm-status", vm);
    }
    if ask_api
        && matches!(engine, EngineState::Ready | EngineState::VmRunning)
        && let Ok(list) = session.client.containers()
    {
        *lock(&session.running) = Some(list.iter().filter(|c| c.state == "running").count());
    }
    update_tray(app);
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let item = |id: &str, enabled: bool| MenuItem::with_id(app, id, id, enabled, None::<&str>);
    let engine = item("engine", false)?;
    let update = item("update", true)?;
    let status = item("status", false)?;
    let running = item("running", false)?;
    let open = item("open", true)?;
    let vm = if cfg!(target_os = "macos") {
        Some(item("vm", false)?)
    } else {
        None
    };
    let settings = item("settings", true)?;
    let quit = item("quit", true)?;
    let check = |id: &str| CheckMenuItem::with_id(app, id, id, true, false, None::<&str>);
    let docker_podbag = check("docker-podbag")?;
    let docker_other = check("docker-other")?;
    let docker = Submenu::with_id_and_items(
        app,
        "docker",
        "docker",
        false,
        &[&docker_podbag, &docker_other],
    )?;
    let _ = docker_podbag.set_text("podbag");
    let separator = PredefinedMenuItem::separator(app)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let mut entries: Vec<&dyn tauri::menu::IsMenuItem<Wry>> =
        vec![&engine, &status, &running, &separator, &open, &settings];
    if let Some(vm) = &vm {
        entries.push(vm);
    }
    entries.push(&docker);
    entries.push(&separator2);
    entries.push(&quit);
    let menu = Menu::with_items(app, &entries)?;
    let items = TrayItems {
        menu: menu.clone(),
        engine,
        update,
        update_shown: false,
        status,
        running,
        open,
        vm,
        docker,
        docker_podbag,
        docker_other,
        settings,
        quit,
    };
    let icon = if cfg!(target_os = "macos") {
        tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))?
    } else {
        tauri::image::Image::from_bytes(include_bytes!("../icons/64x64.png"))?
    };
    TrayIconBuilder::with_id("podbag")
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("podbag")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "update" => {
                show_window(app);
                let _ = app.emit("open-updates", ());
            }
            "settings" => open_settings(app),
            id @ ("docker-podbag" | "docker-other") => {
                let app = app.clone();
                let to = if id == "docker-podbag" { "use" } else { "back" };
                std::thread::spawn(move || {
                    if let Err(e) = switch_docker(&app, to) {
                        eprintln!("podbag-desktop: {e}");
                    }
                    update_tray(&app);
                });
            }
            "quit" => {
                app.state::<AppState>().current().stop_streams();
                app.exit(0);
            }
            "vm" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    let session = app.state::<AppState>().current();
                    let running = *lock(&session.engine) == EngineState::VmRunning;
                    let _ = run_vm(&app, &session, if running { "stop" } else { "start" });
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. }
            ) {
                let app = tray.app_handle().clone();
                std::thread::spawn(move || refresh_tray(&app, true));
            }
        })
        .build(app)?;
    *lock(&app.state::<AppState>().tray) = Some(items);
    update_tray(app.handle());
    Ok(())
}

/// macOS: the app menu gets Settings… (⌘,). Other systems have no app menu: the page
/// takes Ctrl+, itself.
fn build_app_menu(app: &tauri::App) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let menu = Menu::default(app.handle())?;
        if let Some(tauri::menu::MenuItemKind::Submenu(first)) = menu.items()?.into_iter().next() {
            let label = strings::text(app.state::<AppState>().language(), "tray.settings", None);
            let settings =
                MenuItem::with_id(app, "app-settings", label, true, Some("CmdOrCtrl+,"))?;
            first.insert(&settings, 2)?;
            first.insert(&PredefinedMenuItem::separator(app)?, 3)?;
            *lock(&app.state::<AppState>().app_settings) = Some(settings);
        }
        app.set_menu(menu)?;
        app.on_menu_event(|app, event| {
            if event.id() == "app-settings" {
                open_settings(app);
            }
        });
    }
    let _ = app;
    Ok(())
}

#[tauri::command(async)]
fn practice_volumes(
    action: String,
    session: Option<String>,
    name: Option<String>,
) -> Result<Option<podbag::volume_lesson::gui::Snapshot>, String> {
    use podbag::volume_lesson::{directory, gui};
    let dir = directory().map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        gui::view(
            &dir,
            &action,
            session.as_deref(),
            name.as_deref().unwrap_or(""),
        )
        .map_err(|e| e.to_string())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if !dir.join(gui::ACTIVE).exists() && action == "status" {
            return Ok(None);
        }
        let path = podbag::wsl::wsl_path(&dir.to_string_lossy())?;
        let out = std::process::Command::new("wsl.exe")
            .args([
                "-d",
                podbag::wsl::DISTRO,
                "--exec",
                podbag::wsl::ENGINE,
                "lab",
                "desktop-view",
                &path,
                &action,
            ])
            .args(session)
            .args(name)
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn volume_tools_lab(
    state: State<'_, AppState>,
    action: &str,
    package: Option<String>,
    lang: Option<String>,
) -> Result<Option<podbag::volume_lesson::Installed>, String> {
    use podbag::volume_lesson as lesson;
    let dir = lesson::directory().map_err(|e| e.to_string())?;
    match action {
        "status" => {}
        "install" => lesson::install(&dir, package.as_deref().ok_or("Choose a module file.")?)
            .map_err(|e| e.to_string())?,
        "remove" => lesson::remove(&dir).map_err(|e| e.to_string())?,
        "start" => {
            if lesson::read(&dir).map_err(|e| e.to_string())?.is_none() {
                return Err("Install the module first.".into());
            }
            let argv = [
                state.podbag.to_string_lossy().into_owned(),
                "lab".into(),
                "start".into(),
                "--lang".into(),
                lang.unwrap_or_else(|| "en".into()),
                "--library".into(),
                dir.to_string_lossy().into_owned(),
            ];
            #[cfg(target_os = "macos")]
            let opened = commands::open_practice_terminal(&argv).map_err(|e| e.to_string())?;
            #[cfg(not(target_os = "macos"))]
            let opened = commands::open_terminal(Some(&argv));
            if !opened {
                return Err("Open a terminal and run podbag lab start.".into());
            }
        }
        _ => return Err("Unknown lesson action.".into()),
    }
    lesson::read(&dir).map_err(|e| e.to_string())
}

#[tauri::command]
fn tutorial_lab(
    lab: State<'_, Mutex<lab::Lab>>,
    action: lab::Action,
    value: Option<String>,
) -> Result<lab::Snapshot, String> {
    lock(&lab)
        .run(action, value.as_deref())
        .map_err(|e| e.to_string())
}

fn main() {
    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.append_invoke_initialization_script("window.podbagWindowToolbar = true;");
    let app = builder
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .manage(AppState::new())
        .manage(Mutex::new(lab::Lab::default()))
        .invoke_handler(tauri::generate_handler![
            engine,
            tutorial_lab,
            volume_tools_lab,
            practice_volumes,
            list,
            kubernetes::kube_counts,
            migration::migration_sources,
            migration::migration_list,
            migration::migrate,
            kubernetes::kube_list,
            kubernetes::kube_contexts,
            connection,
            contexts,
            create_volume,
            inspect,
            act,
            remove,
            prune,
            rename,
            container_json,
            features,
            pull,
            logs_start,
            logs_stop,
            watch,
            open_shell,
            open_terminal,
            vm,
            vm_status,
            vm_set,
            set_engine,
            updates,
            upgrade,
            search,
            registries,
            registry_login,
            docker_command,
            docker_switch,
            docker_images,
            import_images,
            vm_resources,
            take_star_prompt,
            open_repository,
            get_settings,
            set_settings,
        ])
        .setup(|app| {
            let state = app.state::<AppState>();
            if let Ok(dir) = app.path().app_config_dir() {
                // Check for an existing profile before the webview creates its data directory.
                #[cfg(target_os = "macos")]
                let profile = app.path().home_dir().map(|home| {
                    home.join("Library/WebKit")
                        .join(&app.config().identifier)
                        .join("WebsiteData")
                });
                #[cfg(not(target_os = "macos"))]
                let profile = app.path().app_local_data_dir();
                state.star_prompt.store(
                    profile.is_ok_and(|path| settings::take_star_prompt(&dir, &path)),
                    Ordering::SeqCst,
                );
                let file = dir.join("settings.json");
                *lock(&state.settings) = Settings::load(&file);
                *lock(&state.settings_file) = Some(file);
            }
            // Create the window with the saved appearance before the webview paints.
            let config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .ok_or("missing main window configuration")?;
            let theme = lock(&state.settings).theme.native();
            tauri::WebviewWindowBuilder::from_config(app, config)?
                .theme(theme)
                .build()?;
            build_tray(app)?;
            build_app_menu(app)?;
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                loop {
                    refresh_tray(&handle, false);
                    std::thread::sleep(Duration::from_secs(5));
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // GTK applies portal theme changes to every window, including pinned themes.
            #[cfg(target_os = "linux")]
            if let WindowEvent::ThemeChanged(actual) = event {
                let preferred = lock(&window.state::<AppState>().settings).theme.native();
                if let Some(theme) = preferred
                    && theme != *actual
                {
                    let _ = window.set_theme(Some(theme));
                }
            }
            // Closing the window hides it; the tray item stays, and Quit ends the app.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                let state = window.state::<AppState>();
                state.visible.store(false, Ordering::SeqCst);
                state.current().stop_streams();
            }
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("podbag-desktop: {e}");
            std::process::exit(1);
        });
    app.run(|app, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            let lab = app.state::<Mutex<lab::Lab>>();
            let _ = lock(&lab).run(lab::Action::End, None);
        }
        // macOS: a click on the Dock icon brings the window back.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            show_window(app);
        }
        let _ = (app, event);
    });
}

#[cfg(all(unix, test))]
mod connection_tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, small_engine};

    #[test]
    fn switching_rejects_stale_actions_even_when_resource_names_match() {
        let first = FakeEngine::start(small_engine);
        let second = FakeEngine::start(small_engine);
        let state = AppState::new();
        let file =
            std::env::temp_dir().join(format!("podbag-engine-test-{}.json", std::process::id()));
        state
            .switch(1, &first.client.endpoint.describe(), &file)
            .unwrap();
        let pending = state.session(2).unwrap();
        state
            .switch(2, &second.client.endpoint.describe(), &file)
            .unwrap();
        assert!(pending.retired.load(Ordering::SeqCst));
        assert!(state.session(2).is_err());
        assert!(state.switch(2, "podbag", &file).is_err());
        commands::remove(
            &state.session(3).unwrap().client,
            "volumes",
            "pgdata",
            false,
        )
        .unwrap();
        assert!(first.requests().is_empty());
        assert_eq!(second.requests(), ["DELETE /volumes/pgdata"]);
        std::fs::remove_file(file).unwrap();
    }
}
