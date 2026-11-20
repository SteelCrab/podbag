//! podbag's desktop app: a window for containers, images, volumes and networks, and a
//! tray (menu bar) item. A client of the Docker API on podbag's socket, like
//! `podbag tui`. The page is static HTML, CSS and JavaScript in `ui/`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod kubernetes;
mod settings;
mod strings;

use commands::EngineStatus;
use podbag::client::{Client, Closer, Endpoint, Features, podbag_program};
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
    status: MenuItem<Wry>,
    running: MenuItem<Wry>,
    open: MenuItem<Wry>,
    vm: Option<MenuItem<Wry>>,
    quit: MenuItem<Wry>,
}

struct AppState {
    client: Client,
    podbag: PathBuf,
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
        let endpoint = Endpoint::find(&podbag).unwrap_or_else(|_| Endpoint::Socket {
            path: PathBuf::from("docker.sock"),
            own: false,
        });
        AppState {
            client: Client::new(endpoint),
            podbag,
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
    let status = commands::engine(&state.client, &state.podbag, start);
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
    let v = commands::list(&state.client, &kind)?;
    if kind == "containers" {
        *lock(&state.running) = Some(commands::running(&v));
        update_tray(&app);
    }
    Ok(v)
}

#[tauri::command(async)]
fn inspect(state: State<'_, AppState>, kind: String, id: String) -> Result<Value, String> {
    commands::inspect(&state.client, &kind, &id)
}

#[tauri::command(async)]
fn act(state: State<'_, AppState>, id: String, action: String) -> Result<(), String> {
    commands::act(&state.client, &id, &action)
}

#[tauri::command(async)]
fn remove(state: State<'_, AppState>, kind: String, id: String, force: bool) -> Result<(), String> {
    commands::remove(&state.client, &kind, &id, force)
}

#[tauri::command(async)]
fn prune(state: State<'_, AppState>, kind: String) -> Result<podbag::client::Pruned, String> {
    commands::prune(&state.client, &kind)
}

#[tauri::command(async)]
fn rename(state: State<'_, AppState>, id: String, name: String) -> Result<(), String> {
    commands::rename(&state.client, &id, &name)
}

#[tauri::command(async)]
fn container_json(state: State<'_, AppState>, id: String, what: String) -> Result<Value, String> {
    commands::container_json(&state.client, &id, &what)
}

#[tauri::command(async)]
fn features(state: State<'_, AppState>, sample: Option<String>) -> Features {
    state.client.features(sample.as_deref())
}

#[tauri::command(async)]
fn pull(app: AppHandle, state: State<'_, AppState>, image: String) -> Result<(), String> {
    state
        .client
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
    let lines = state.client.logs(&id, 500, true).map_err(|e| e.message)?;
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
            if let Ok(events) = state.client.events() {
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
    commands::open_shell(&state.podbag, &id)
}

/// Query the companion CLI so preferences and boot always use the same VM home.
#[tauri::command(async)]
fn vm_resources(state: State<'_, AppState>, values: Option<VmResources>) -> Result<Value, String> {
    if !cfg!(target_os = "macos")
        || !matches!(state.client.endpoint, Endpoint::Socket { own: true, .. })
    {
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

/// `podbag vm start` or `podbag vm stop` (macOS and Windows).
#[tauri::command(async)]
fn vm(app: AppHandle, action: String) -> Result<(), String> {
    run_vm(&app, &action)
}

fn run_vm(app: &AppHandle, action: &str) -> Result<(), String> {
    if !matches!(action, "start" | "stop") {
        return Err(format!("unknown action {action}"));
    }
    let state = app.state::<AppState>();
    if action == "stop" {
        state.stop_streams();
    }
    let mut cmd = std::process::Command::new(&state.podbag);
    cmd.args(["vm", action]).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("{}: {e}", state.podbag.display()))?;
    refresh_tray(app, false);
    let _ = app.emit("engine-changed", action);
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .trim_start_matches("podbag: ")
            .to_string())
    }
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
    let tray = lock(&state.tray);
    let Some(items) = tray.as_ref() else {
        return;
    };
    let t = |key: &str| strings::text(lang, key, None);
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
        let _ = vm.set_enabled(engine != EngineState::Unknown);
    }
}

/// Read the engine's state for the tray. The VM's state costs nothing (macOS reads
/// files); the API is asked only while the window is open, or when `asked` (a click on
/// the tray), so a hidden app keeps no VM or service awake.
fn refresh_tray(app: &AppHandle, asked: bool) {
    let state = app.state::<AppState>();
    let own = matches!(state.client.endpoint, Endpoint::Socket { own: true, .. });
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
            if state.client.ping().is_ok() {
                EngineState::Ready
            } else {
                EngineState::Down
            }
        }
        None => *lock(&state.engine),
    };
    let previous = std::mem::replace(&mut *lock(&state.engine), engine);
    if previous != engine
        && let Some(vm) = vm
    {
        let _ = app.emit("vm-status", vm);
    }
    if ask_api
        && matches!(engine, EngineState::Ready | EngineState::VmRunning)
        && let Ok(list) = state.client.containers()
    {
        *lock(&state.running) = Some(list.iter().filter(|c| c.state == "running").count());
    }
    update_tray(app);
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let item = |id: &str, enabled: bool| MenuItem::with_id(app, id, id, enabled, None::<&str>);
    let items = TrayItems {
        status: item("status", false)?,
        running: item("running", false)?,
        open: item("open", true)?,
        vm: if cfg!(target_os = "macos") {
            Some(item("vm", false)?)
        } else {
            None
        },
        quit: item("quit", true)?,
    };
    let separator = PredefinedMenuItem::separator(app)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let mut entries: Vec<&dyn tauri::menu::IsMenuItem<Wry>> =
        vec![&items.status, &items.running, &separator, &items.open];
    if let Some(vm) = &items.vm {
        entries.push(vm);
    }
    entries.push(&separator2);
    entries.push(&items.quit);
    let menu = Menu::with_items(app, &entries)?;
    let icon = if cfg!(target_os = "macos") {
        tauri::image::Image::from_bytes(include_bytes!("../icons/tray-44.png"))?
    } else {
        tauri::image::Image::from_bytes(include_bytes!("../icons/64x64.png"))?
    };
    TrayIconBuilder::with_id("podbag")
        .icon(icon)
        .tooltip("podbag")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
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
    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.append_invoke_initialization_script("window.podbagWindowToolbar = true;");
    let app = builder
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            engine,
            list,
            kubernetes::kube_list,
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
            vm,
            vm_resources,
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
