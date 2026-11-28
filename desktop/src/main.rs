//! podbag's desktop app: a window for containers, images, volumes and networks, and a
//! tray (menu bar) item. A client of the Docker API on podbag's socket, like
//! `podbag tui`. The page is static HTML, CSS and JavaScript in `ui/`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use podbag::dashboard::app_update::{self, Installer};
use podbag::dashboard::cli;
use podbag::dashboard::cluster_migration;
mod builders;
mod commands;
mod dockercmd;
mod extensions;
mod image_analysis;
mod images;
mod kubernetes;
use podbag::dashboard::lab;
mod migration;
mod model_runner;
mod run;
mod settings;
mod strings;
mod terminal_cli;

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
    builder_work: Mutex<()>,
    image_analysis: Mutex<()>,
    models: model_runner::ModelRunner,
    /// The engine this window uses, and its client.
    session: Mutex<Arc<Session>>,
    podbag: PathBuf,
    /// How this app updates itself; None when it did not come from a package.
    installer: Option<Installer>,
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
            builder_work: Mutex::new(()),
            image_analysis: Mutex::new(()),
            models: model_runner::ModelRunner::default(),
            session: Mutex::new(Arc::new(Session::new(1, target))),
            podbag,
            installer: std::env::current_exe()
                .and_then(|e| e.canonicalize())
                .ok()
                .and_then(|e| Installer::detect(&e)),
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

    /// A newer release, as the last check found it (a file; no network). A packaged app
    /// compares its own version: its engine comes with it.
    fn known_update(&self) -> Option<String> {
        let (_, latest) = cli::saved_check(&update_file()?)?;
        let current = if self.installer.is_some() {
            env!("CARGO_PKG_VERSION").to_string()
        } else {
            lock(&self.engine_version).clone()?
        };
        cli::is_newer(&latest, &current).then_some(latest)
    }

    /// A `podbag` for the terminal that this app's package does not update.
    fn terminal_podbag(&self) -> Option<PathBuf> {
        self.installer.as_ref()?;
        podbag::client::terminal_podbag(&self.podbag)
    }

    /// The `podbag` that Update upgrades with `podbag upgrade`.
    fn cli_to_upgrade(&self) -> Option<PathBuf> {
        cli::cli_to_upgrade(
            &self.podbag,
            self.installer.is_some(),
            self.terminal_podbag().as_deref(),
        )
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
fn builder_list(state: State<'_, AppState>, token: u64) -> Result<builders::Catalog, String> {
    let session = state.session(token)?;
    builders::list(&session.target)
}

#[tauri::command(async)]
fn builder_inspect(
    state: State<'_, AppState>,
    token: u64,
    scope: String,
    name: String,
) -> Result<builders::Inspection, String> {
    let session = state.session(token)?;
    builders::inspect(&session.target, &scope, &name)
}

#[tauri::command(async)]
fn builder_action(
    state: State<'_, AppState>,
    token: u64,
    scope: String,
    name: String,
    action: builders::Action,
) -> Result<(), String> {
    let _work = lock(&state.builder_work);
    let session = state.session(token)?;
    builders::act(&session.target, &scope, &name, action)
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
fn desktop_extensions(
    app: AppHandle,
    request: extensions::Action,
) -> Result<Vec<extensions::Installed>, extensions::Failure> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| extensions::Failure::detail("storage", error))?
        .join("extensions");
    extensions::apply(&directory, request)
}

#[tauri::command(async)]
fn models(
    app: AppHandle,
    state: State<'_, AppState>,
    request: model_runner::Action,
) -> Result<model_runner::Reply, model_runner::Failure> {
    let store = app
        .path()
        .app_data_dir()
        .map_err(|error| model_runner::Failure::with_detail("start", error))?
        .join("models");
    state.models.action(request, &store)
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
fn analyze_image(
    app: AppHandle,
    state: State<'_, AppState>,
    token: u64,
    id: String,
) -> Result<image_analysis::Report, image_analysis::Failure> {
    use image_analysis::Failure;
    let session = state.session(token).map_err(|_| Failure::EngineChanged)?;
    let _running = state.image_analysis.try_lock().map_err(|_| Failure::Busy)?;
    let cache = app.path().app_cache_dir().map_err(|_| Failure::Workspace)?;
    let result = image_analysis::analyze(&session.client, &id, &cache.join("trivy"));
    state.session(token).map_err(|_| Failure::EngineChanged)?;
    result
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
    session.client.create_volume(&name).map_err(|e| e.message)
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

/// Create and start a container from the form. A missing image is pulled first; its lines
/// go out as pull progress. Gives the container's id.
#[tauri::command(async)]
fn run_container(
    app: AppHandle,
    state: State<'_, AppState>,
    token: u64,
    spec: podbag::client::RunSpec,
) -> Result<String, String> {
    let session = state.session(token)?;
    run::run(
        &session.client,
        &session.target,
        &spec,
        podbag::client::registry_auth,
        |line| {
            let _ = session.emit(&app, "pull-progress", line);
        },
    )
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

/// Read or edit the local macOS VM disk through the companion CLI.
#[tauri::command(async)]
fn vm_storage(
    state: State<'_, AppState>,
    token: u64,
    edit: Option<cli::VmStorageEdit>,
) -> Result<Value, String> {
    let session = state.session(token)?;
    if !cfg!(target_os = "macos") || session.target.choice != Choice::Podbag {
        return Err("VM storage requires the local macOS engine".into());
    }
    cli::vm_storage(&state.podbag, edit)
}

/// `podbag system compression [idle|off] --json`: `{"mode", "after"}`. With no mode it
/// only reads. On macOS the command runs in the VM (it starts the VM when none runs).
#[tauri::command(async)]
fn compression(
    state: State<'_, AppState>,
    token: u64,
    mode: Option<String>,
) -> Result<Value, String> {
    if state.session(token)?.target.choice != Choice::Podbag {
        return Err("Image compression requires the podbag engine".into());
    }
    cli::compression(&state.podbag, mode.as_deref())
}

#[tauri::command(async)]
fn network_settings(
    state: State<'_, AppState>,
    token: u64,
    values: Option<podbag::network::NetworkPreferences>,
    expected_revision: Option<String>,
) -> Result<Value, String> {
    if state.session(token)?.target.choice != Choice::Podbag {
        return Err("Network settings require the local podbag engine".into());
    }
    cli::network_settings(&state.podbag, values, expected_revision.as_deref())
}

#[tauri::command(async)]
fn registry_proxy(
    state: State<'_, AppState>,
    token: u64,
    values: Option<podbag::image::proxy::Settings>,
) -> Result<podbag::image::proxy::Settings, String> {
    if !cfg!(any(target_os = "linux", target_os = "macos"))
        || state.session(token)?.target.choice != Choice::Podbag
    {
        return Err("Registry proxy settings require the local Podbag engine".into());
    }
    cli::registry_proxy(&state.podbag, values)
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
            let podbag = state.cli_to_upgrade().ok_or("no podbag to update")?;
            let argv = [
                "sudo".to_string(),
                podbag.display().to_string(),
                "upgrade".to_string(),
            ];
            commands::open_terminal(Some(&argv))
                .then_some(())
                .ok_or_else(|| cli::admin_command(&podbag))
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

/// List or change synchronized file shares in the macOS VM.
#[tauri::command(async)]
fn vm_sync(
    state: State<'_, AppState>,
    token: u64,
    action: String,
    name: Option<String>,
    path: Option<String>,
) -> Result<Value, String> {
    if state.session(token)?.target.choice != Choice::Podbag || commands::platform() != "macos" {
        return Err("Synchronized shares require Podbag on macOS".into());
    }
    cli::vm_sync(&state.podbag, &action, name.as_deref(), path.as_deref())
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
    {
        let _work = lock(&state.builder_work);
        state.switch(token, &engine, &file)?;
    }
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
    let mut u = cli::updates(&state.podbag, file.as_deref(), check, unix_now());
    cli::plan(
        &mut u,
        state.installer.as_ref().map(Installer::name),
        state.terminal_podbag().as_deref(),
    );
    *lock(&state.engine_version) = u.engine.clone();
    state.version_asked.store(true, Ordering::SeqCst);
    update_tray(&app);
    u
}

/// `podbag upgrade --yes [--version V]` on the `podbag` that Update upgrades: in a
/// packaged app, a separate one for the terminal (the app's own comes with the app).
#[tauri::command(async)]
fn upgrade(app: AppHandle, state: State<'_, AppState>, version: Option<String>) -> cli::Upgraded {
    let Some(podbag) = state.cli_to_upgrade() else {
        return cli::Upgraded {
            ok: true,
            ..cli::Upgraded::default()
        };
    };
    let done = cli::upgrade_to(&podbag, version.as_deref());
    if done.ok && podbag == state.podbag {
        *lock(&state.engine_version) = cli::engine_version(&state.podbag);
        update_tray(&app);
    }
    done
}

/// Install the app's package of release `version` (None: the latest) and start the new
/// app. Its own `podbag` comes with it.
#[tauri::command(async)]
fn update_app(
    app: AppHandle,
    state: State<'_, AppState>,
    version: Option<String>,
) -> Result<(), String> {
    let installer = state
        .installer
        .clone()
        .ok_or("This app was not installed from a package, so it cannot update itself.")?;
    let wanted = match version.as_deref() {
        Some(v) => Some(
            podbag::upgrade::Version::parse(v).ok_or_else(|| format!("{v:?} is not a version"))?,
        ),
        None => None,
    };
    let current = podbag::upgrade::Version::parse(env!("CARGO_PKG_VERSION"))
        .ok_or("this app has no version")?;
    let restart = app_update::install(&installer, wanted.as_ref(), &current)?;
    state.current().stop_streams();
    restart.spawn()?;
    // Answer the window first; then end, so the new app (or the installer) can start.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        app.exit(0);
    });
    Ok(())
}

#[tauri::command(async)]
fn open_releases() -> Result<(), String> {
    let url = format!("{}/releases/latest", env!("CARGO_PKG_REPOSITORY"));
    open::that_detached(url).map_err(|e| e.to_string())
}

/// What a terminal runs for `podbag`. None: this app is a build of its own, with no podbag
/// to offer.
#[tauri::command(async)]
fn terminal_command(state: State<'_, AppState>) -> Option<terminal_cli::Status> {
    state.installer.as_ref()?;
    Some(terminal_cli::current(&state.podbag))
}

/// Make the `podbag` command from the app's own podbag. Ok(None): the user did not give
/// the administrator password.
#[tauri::command(async)]
fn install_terminal_command(
    state: State<'_, AppState>,
) -> Result<Option<terminal_cli::Status>, String> {
    if state.installer.is_none() {
        return Err("this app has no podbag of its own to install".into());
    }
    let prompt = strings::text(state.language(), "cli.prompt", None);
    terminal_cli::install(&state.podbag, &prompt)
}

#[tauri::command]
fn take_star_prompt(state: State<'_, AppState>) -> bool {
    state.star_prompt.swap(false, Ordering::SeqCst)
}

#[tauri::command(async)]
fn open_repository() -> Result<(), String> {
    open::that_detached(env!("CARGO_PKG_REPOSITORY")).map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn open_training_guide(id: &str, lang: &str) -> Result<(), String> {
    use podbag::volume_lesson as lesson;
    let module = lesson::Module::parse(id).map_err(|e| e.to_string())?;
    let dir = lesson::directory().map_err(|e| e.to_string())?;
    let url = match lesson::read(&dir, &module).map_err(|e| e.to_string())? {
        Some(record) => record.guide_url(lang),
        None => module.guide_url(lang),
    }
    .ok_or("The module has no web guide.")?;
    open::that_detached(url).map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn engine_config(
    state: State<'_, AppState>,
    token: u64,
    request: cli::EngineConfigRequest,
) -> Result<Option<podbag::config::Document>, String> {
    if state.session(token)?.target.choice != Choice::Podbag {
        return Err("Engine configuration requires the local Podbag engine".into());
    }
    cli::engine_config(&state.podbag, request)
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
    let mut active = lock(&state.settings);
    let theme_changed = settings.theme != active.theme;
    if theme_changed {
        window
            .set_theme(settings.theme.native())
            .map_err(|e| e.to_string())?;
    }
    if let Some(file) = lock(&state.settings_file).as_ref()
        && let Err(error) = settings.save(file)
    {
        if theme_changed {
            let _ = window.set_theme(active.theme.native());
        }
        return Err(error);
    }
    *active = settings;
    drop(active);
    update_tray(window.app_handle());
    Ok(())
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
    containers: Option<bool>,
    course: Option<bool>,
) -> Result<serde_json::Value, String> {
    use podbag::volume_lesson as lesson;
    let dir = lesson::directory().map_err(|e| e.to_string())?;
    let containers = containers.unwrap_or(false);
    let course = course.unwrap_or(false);
    let request = |action: &str| -> Result<serde_json::Value, String> {
        #[cfg(unix)]
        {
            let target = name.as_deref().unwrap_or("");
            let result = if course {
                lesson::course::view(&dir, action, session.as_deref(), target)
                    .map(|view| serde_json::to_value(view))
            } else if containers {
                lesson::containers::view(&dir, action, session.as_deref(), target)
                    .map(|view| serde_json::to_value(view))
            } else {
                lesson::gui::view(&dir, action, session.as_deref(), target)
                    .map(|view| serde_json::to_value(view))
            };
            result
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let active = if course {
                lesson::course::ACTIVE
            } else if containers {
                lesson::containers::ACTIVE
            } else {
                lesson::gui::ACTIVE
            };
            if !dir.join(active).exists() && action == "status" {
                return Ok(serde_json::Value::Null);
            }
            let path = podbag::wsl::wsl_path(&dir.to_string_lossy())?;
            let mut command = std::process::Command::new("wsl.exe");
            command.args([
                "-d",
                podbag::wsl::DISTRO,
                "--exec",
                podbag::wsl::ENGINE,
                "lab",
                "desktop-view",
            ]);
            if course {
                command.arg("--course");
            } else if containers {
                command.arg("--containers");
            }
            let out = command
                .args([&path, action])
                .args(session.as_deref())
                .args(name.as_deref())
                .creation_flags(0x08000000)
                .output()
                .map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
            }
            serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
        }
    };
    if course && action == "open-web" {
        let view = request("show-web")?;
        let port = view["web"]["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .ok_or("The practice port is unavailable.")?;
        open::that_detached(format!("http://127.0.0.1:{port}/")).map_err(|e| e.to_string())?;
    }
    request(&action)
}

#[tauri::command(async)]
fn volume_tools_lab(
    action: &str,
    package: Option<String>,
    id: Option<String>,
) -> Result<Vec<LessonRecord>, String> {
    use podbag::volume_lesson as lesson;
    let dir = lesson::directory().map_err(|e| e.to_string())?;
    let module = || lesson::selected(&dir, id.as_deref()).map_err(|e| e.to_string());
    match action {
        "status" | "summary" => {}
        "install" => {
            lesson::install(&dir, package.as_deref().ok_or("Choose a module file.")?)
                .map_err(|e| e.to_string())?;
        }
        "remove" => lesson::remove(&dir, &module()?).map_err(|e| e.to_string())?,
        // The Training catalog on podbag.dev: new and changed modules, no app update.
        "sync" => {
            lesson::catalog::sync(&dir)?;
        }
        _ => return Err("Unknown lesson action.".into()),
    }
    let mut records = Vec::new();
    for mut record in lesson::list(&dir).map_err(|e| e.to_string())? {
        let key = lesson::Module::of(&record.package)
            .map_err(|e| e.to_string())?
            .id()
            .to_owned();
        if id
            .as_deref()
            .is_some_and(|id| action == "summary" && id != key)
        {
            continue;
        }
        record.package.animation = None;
        // An old course format has no steps until a sync replaces it.
        let steps = record
            .package
            .course()
            .map(|course| lesson::course::instructions(&course))
            .unwrap_or_default();
        records.push(LessonRecord {
            id: key,
            record,
            steps,
        });
    }
    Ok(records)
}

#[derive(serde::Serialize)]
struct LessonRecord {
    id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    steps: Vec<podbag::volume_lesson::course::Instruction>,
    #[serde(flatten)]
    record: podbag::volume_lesson::Installed,
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
            desktop_extensions,
            models,
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
            kubernetes::local_kube_status,
            kubernetes::local_kube_create,
            kubernetes::local_kube_reset,
            kubernetes::local_kube_remove,
            kubernetes::kube_openshift,
            kubernetes::kube_open_route,
            connection,
            builder_list,
            builder_inspect,
            builder_action,
            contexts,
            create_volume,
            inspect,
            analyze_image,
            act,
            remove,
            prune,
            rename,
            container_json,
            features,
            pull,
            run_container,
            logs_start,
            logs_stop,
            watch,
            open_shell,
            open_terminal,
            vm,
            vm_status,
            vm_set,
            vm_sync,
            set_engine,
            updates,
            upgrade,
            update_app,
            open_releases,
            search,
            registries,
            registry_login,
            docker_command,
            docker_switch,
            docker_images,
            import_images,
            vm_resources,
            vm_storage,
            compression,
            registry_proxy,
            network_settings,
            take_star_prompt,
            terminal_command,
            install_terminal_command,
            open_repository,
            open_training_guide,
            engine_config,
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
            app.state::<AppState>().models.stop();
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
