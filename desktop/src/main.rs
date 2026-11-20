//! podbag's desktop app: a window for containers, images, volumes and networks, and a
//! tray (menu bar) item. A client of the Docker API on podbag's socket, like
//! `podbag tui`. The page is static HTML, CSS and JavaScript in `ui/`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod commands;
mod settings;
mod strings;

use commands::EngineStatus;
use podbag::client::{Choice, Client, Closer, Endpoint, Features, Target, podbag_program};
use serde::Serialize;
use serde_json::Value;
use settings::Settings;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
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
    quit: MenuItem<Wry>,
}

struct AppState {
    /// The engine this window uses, and its client.
    target: Mutex<Target>,
    client: Mutex<Client>,
    podbag: PathBuf,
    /// The engine's version (`podbag --version`), once asked.
    engine_version: Mutex<Option<String>>,
    version_asked: AtomicBool,
    settings: Mutex<Settings>,
    settings_file: Mutex<Option<PathBuf>>,
    /// The events stream, while the window is open.
    watching: AtomicBool,
    watch_generation: AtomicU64,
    watch: Mutex<Option<Closer>>,
    /// The open log stream and its generation.
    logs: Mutex<Option<Closer>>,
    log_generation: AtomicU64,
    visible: AtomicBool,
    engine: Mutex<EngineState>,
    /// Running containers, as last seen.
    running: Mutex<Option<usize>>,
    tray: Mutex<Option<TrayItems>>,
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
            client: Mutex::new(Client::new(target.endpoint.clone())),
            target: Mutex::new(target),
            podbag,
            engine_version: Mutex::new(None),
            version_asked: AtomicBool::new(false),
            settings: Mutex::new(Settings::default()),
            settings_file: Mutex::new(None),
            watching: AtomicBool::new(false),
            watch_generation: AtomicU64::new(0),
            watch: Mutex::new(None),
            logs: Mutex::new(None),
            log_generation: AtomicU64::new(0),
            visible: AtomicBool::new(true),
            engine: Mutex::new(EngineState::Unknown),
            running: Mutex::new(None),
            tray: Mutex::new(None),
        }
    }

    fn client(&self) -> Client {
        lock(&self.client).clone()
    }

    fn target(&self) -> Target {
        lock(&self.target).clone()
    }

    fn is_podbag(&self) -> bool {
        lock(&self.target).choice == Choice::Podbag
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

#[tauri::command(async)]
fn engine(app: AppHandle, state: State<'_, AppState>, start: bool) -> EngineStatus {
    let status = commands::engine(&state.target(), &state.client(), &state.podbag, start);
    *lock(&state.engine) = match (status.vm.as_deref(), status.reachable) {
        (Some("running"), _) => EngineState::VmRunning,
        (Some(_), false) => EngineState::VmStopped,
        (_, true) => EngineState::Ready,
        (_, false) => EngineState::Down,
    };
    update_tray(&app);
    status
}

#[tauri::command(async)]
fn list(app: AppHandle, state: State<'_, AppState>, kind: String) -> Result<Value, String> {
    let v = commands::list(&state.client(), &kind)?;
    if kind == "containers" {
        *lock(&state.running) = Some(commands::running(&v));
        update_tray(&app);
    }
    Ok(v)
}

#[tauri::command(async)]
fn inspect(state: State<'_, AppState>, kind: String, id: String) -> Result<Value, String> {
    commands::inspect(&state.client(), &kind, &id)
}

#[tauri::command(async)]
fn act(state: State<'_, AppState>, id: String, action: String) -> Result<(), String> {
    commands::act(&state.client(), &id, &action)
}

#[tauri::command(async)]
fn remove(state: State<'_, AppState>, kind: String, id: String, force: bool) -> Result<(), String> {
    commands::remove(&state.client(), &kind, &id, force)
}

#[tauri::command(async)]
fn prune(state: State<'_, AppState>, kind: String) -> Result<podbag::client::Pruned, String> {
    commands::prune(&state.client(), &kind)
}

#[tauri::command(async)]
fn rename(state: State<'_, AppState>, id: String, name: String) -> Result<(), String> {
    commands::rename(&state.client(), &id, &name)
}

#[tauri::command(async)]
fn container_json(state: State<'_, AppState>, id: String, what: String) -> Result<Value, String> {
    commands::container_json(&state.client(), &id, &what)
}

#[tauri::command(async)]
fn features(state: State<'_, AppState>, sample: Option<String>) -> Features {
    state.client().features(sample.as_deref())
}

#[tauri::command(async)]
fn pull(app: AppHandle, state: State<'_, AppState>, image: String) -> Result<(), String> {
    state
        .client()
        .pull(&image, |line| {
            let _ = app.emit("pull-progress", line);
        })
        .map_err(|e| e.message)
}

#[derive(Clone, Serialize)]
struct LogBatch {
    generation: u64,
    lines: Vec<String>,
}

/// Follow a container's log: `logs` events with this generation, until `logs_stop`.
#[tauri::command(async)]
fn logs_start(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<u64, String> {
    if let Some(c) = lock(&state.logs).take() {
        c.close();
    }
    let generation = state.log_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let lines = state.client().logs(&id, 500, true).map_err(|e| e.message)?;
    *lock(&state.logs) = Some(lines.closer());
    let handle = app.clone();
    std::thread::spawn(move || {
        commands::follow_logs(lines, move |lines| {
            let current = handle
                .state::<AppState>()
                .log_generation
                .load(Ordering::SeqCst);
            current == generation && handle.emit("logs", LogBatch { generation, lines }).is_ok()
        });
    });
    Ok(generation)
}

#[tauri::command(async)]
fn logs_stop(state: State<'_, AppState>) {
    state.log_generation.fetch_add(1, Ordering::SeqCst);
    if let Some(c) = lock(&state.logs).take() {
        c.close();
    }
}

/// Watch the engine's events while `on`: a `changed` event for each change.
#[tauri::command(async)]
fn watch(app: AppHandle, state: State<'_, AppState>, on: bool) {
    if !on {
        state.stop_watch();
        return;
    }
    if state.watching.swap(true, Ordering::SeqCst) {
        return;
    }
    // A thread of an earlier watch that still sleeps sees another generation and ends.
    let generation = state.watch_generation.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let current = || {
            state.watching.load(Ordering::SeqCst)
                && state.watch_generation.load(Ordering::SeqCst) == generation
        };
        while current() {
            if let Ok(events) = state.client().events() {
                *lock(&state.watch) = Some(events.closer());
                // Stopped meanwhile: the stream is closed at once.
                if !current() {
                    events.closer().close();
                }
                let _ = app.emit("live", true);
                for e in events {
                    let Ok(e) = e else { break };
                    let quiet = e.action.starts_with("exec_")
                        || matches!(e.action.as_str(), "attach" | "detach" | "resize");
                    if !quiet {
                        let _ = app.emit("changed", e.kind);
                    }
                }
            }
            let _ = app.emit("live", false);
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

#[tauri::command(async)]
fn open_shell(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let docker = podbag::client::choice::docker_cli();
    let shell = commands::shell_command(&state.target(), &state.podbag, &docker, &id)?;
    commands::open_shell(&shell)
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
fn vm(app: AppHandle, action: String) -> Result<(), String> {
    run_vm(&app, &action)
}

fn run_vm(app: &AppHandle, action: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    if matches!(action, "stop" | "restart") {
        state.stop_streams();
    }
    let done = cli::vm_action(&state.podbag, action);
    refresh_tray(app, false);
    let _ = app.emit("engine-changed", action);
    done
}

/// `podbag vm status --json`: null when the engine is not podbag, or on Linux (no VM).
#[tauri::command(async)]
fn vm_status(state: State<'_, AppState>) -> Result<Value, String> {
    if !state.is_podbag() || commands::platform() == "linux" {
        return Ok(Value::Null);
    }
    cli::vm_status(&state.podbag)
}

/// `podbag vm set`: what it printed. With `restart`, the VM restarts now.
#[tauri::command(async)]
fn vm_set(app: AppHandle, settings: cli::VmSettings) -> Result<String, String> {
    let state = app.state::<AppState>();
    if settings.restart {
        state.stop_streams();
    }
    let done = cli::vm_set(&state.podbag, &settings);
    if settings.restart {
        refresh_tray(&app, false);
        let _ = app.emit("engine-changed", "restart");
    }
    done
}

/// Switch this window to `podbag` or `docker`; saved for the TUI too.
#[tauri::command(async)]
fn set_engine(app: AppHandle, state: State<'_, AppState>, engine: String) -> Result<(), String> {
    let file = podbag::client::ui_dir()
        .ok_or("cannot find the folder for ui.json")?
        .join("ui.json");
    let target = commands::choose(&engine, &file, &state.podbag)?;
    state.stop_streams();
    *lock(&state.client) = Client::new(target.endpoint.clone());
    *lock(&state.target) = target;
    *lock(&state.engine) = EngineState::Unknown;
    *lock(&state.running) = None;
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
fn get_settings(state: State<'_, AppState>) -> Settings {
    lock(&state.settings).clone()
}

#[tauri::command]
fn set_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    let settings = settings.checked();
    *lock(&state.settings) = settings.clone();
    update_tray(&app);
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

fn update_tray(app: &AppHandle) {
    let state = app.state::<AppState>();
    let lang = state.language();
    let engine = *lock(&state.engine);
    let running = *lock(&state.running);
    let target = state.target();
    let update = state.known_update();
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
    // Once: `podbag --version` needs no engine and wakes nothing.
    let _ = state.engine_version();
    let own =
        state.is_podbag() && matches!(state.client().endpoint, Endpoint::Socket { own: true, .. });
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
            if state.client().ping().is_ok() {
                EngineState::Ready
            } else {
                EngineState::Down
            }
        }
        None => *lock(&state.engine),
    };
    *lock(&state.engine) = engine;
    if ask_api
        && matches!(engine, EngineState::Ready | EngineState::VmRunning)
        && let Ok(list) = state.client().containers()
    {
        *lock(&state.running) = Some(list.iter().filter(|c| c.state == "running").count());
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
    let quit = item("quit", true)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let mut entries: Vec<&dyn tauri::menu::IsMenuItem<Wry>> =
        vec![&engine, &status, &running, &separator, &open];
    if let Some(vm) = &vm {
        entries.push(vm);
    }
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
        quit,
    };
    let icon = if cfg!(target_os = "macos") {
        tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template-44.png"))?
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
            "quit" => {
                app.state::<AppState>().stop_streams();
                app.exit(0);
            }
            "vm" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    let running = *lock(&app.state::<AppState>().engine) == EngineState::VmRunning;
                    let _ = run_vm(&app, if running { "stop" } else { "start" });
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

fn main() {
    let app = tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            engine,
            list,
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
            get_settings,
            set_settings,
        ])
        .setup(|app| {
            let state = app.state::<AppState>();
            if let Ok(dir) = app.path().app_config_dir() {
                let file = dir.join("settings.json");
                *lock(&state.settings) = Settings::load(&file);
                *lock(&state.settings_file) = Some(file);
            }
            build_tray(app)?;
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
            // Closing the window hides it; the tray item stays, and Quit ends the app.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                let state = window.state::<AppState>();
                state.visible.store(false, Ordering::SeqCst);
                state.stop_streams();
            }
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("podbag-desktop: {e}");
            std::process::exit(1);
        });
    app.run(|app, event| {
        // macOS: a click on the Dock icon brings the window back.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            show_window(app);
        }
        let _ = (app, event);
    });
}
