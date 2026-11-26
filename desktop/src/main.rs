//! podbag's desktop app: a window for containers, images, volumes and networks, and a
//! tray (menu bar) item. A client of the Docker API on podbag's socket, like
//! `podbag tui`. The page is static HTML, CSS and JavaScript in `ui/`. The engine and
//! system calls are in `podbag-desktop-core`; this is the Tauri side: the commands of
//! the page, its events, the window, the tray and the app menu.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod kubernetes;
mod migration;

use podbag::dashboard::lab;
use podbag_desktop_core::images::{self, LocationInfo, Search};
use podbag_desktop_core::updates::{self, Updates, Upgraded};
use podbag_desktop_core::vm::{VmResources, VmSettings};
use podbag_desktop_core::{
    Desktop, DockerCommand, EngineStatus, Event, Events, Features, Pruned, Settings,
    ThemePreference, parse_kind, terminal_cli, training,
};
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent, Wry};

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
    core: Desktop,
    /// Set when the app starts; events wait for it.
    app: Arc<OnceLock<AppHandle>>,
    tray: Mutex<Option<TrayItems>>,
    /// macOS: Settings… in the app menu.
    app_settings: Mutex<Option<MenuItem<Wry>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl AppState {
    fn new() -> AppState {
        let app: Arc<OnceLock<AppHandle>> = Arc::default();
        let handle = app.clone();
        let events =
            Events::callback(move |event| handle.get().is_none_or(|app| forward(app, event)));
        AppState {
            core: Desktop::new(env!("CARGO_PKG_VERSION"), events),
            app,
            tray: Mutex::new(None),
            app_settings: Mutex::new(None),
        }
    }
}

#[derive(Clone, Serialize)]
struct LogBatch {
    generation: u64,
    lines: Vec<String>,
}

/// An event of the engine session `session`: named with its token (`logs-3`), so a page
/// of an earlier engine does not get it.
fn emit_scoped(app: &AppHandle, name: &str, session: u64, payload: impl Serialize + Clone) -> bool {
    app.emit(&format!("{name}-{session}"), payload).is_ok()
}

/// The core's events, as the page's events.
fn forward(app: &AppHandle, event: Event) -> bool {
    match event {
        Event::Changed { session, kind } => emit_scoped(app, "changed", session, kind),
        Event::Live { session, on } => emit_scoped(app, "live", session, on),
        Event::Logs {
            session,
            generation,
            lines,
        } => emit_scoped(app, "logs", session, LogBatch { generation, lines }),
        Event::PullProgress { session, line } => emit_scoped(app, "pull-progress", session, line),
        Event::EngineChanged { session, action } => {
            emit_scoped(app, "engine-changed", session, action)
        }
        Event::VmStatus { session, state } => emit_scoped(app, "vm-status", session, state),
        Event::DockerSwitching(to) => app.emit("docker-switching", to).is_ok(),
        Event::DockerChanged(now) => app.emit("docker-changed", now).is_ok(),
        Event::StatusChanged => {
            update_tray(app);
            true
        }
        // The results of background work: the page waits for its commands instead.
        _ => true,
    }
}

/// The window's theme for the saved preference; None follows the system.
fn native_theme(theme: ThemePreference) -> Option<tauri::Theme> {
    match theme {
        ThemePreference::Light => Some(tauri::Theme::Light),
        ThemePreference::Dark => Some(tauri::Theme::Dark),
        ThemePreference::System => None,
    }
}

// The window's commands. They run off the main thread: requests may take a while.

#[tauri::command]
fn connection(state: State<'_, AppState>) -> (u64, String, bool) {
    let c = state.core.connection();
    (c.token, c.name, c.local)
}

#[tauri::command(async)]
fn contexts() -> Vec<String> {
    podbag::client::choice::docker_contexts()
}

#[tauri::command(async)]
fn engine(state: State<'_, AppState>, token: u64, start: bool) -> Result<EngineStatus, String> {
    Ok(state.core.session(token)?.check_engine(start))
}

#[tauri::command(async)]
fn list(state: State<'_, AppState>, token: u64, kind: String) -> Result<Value, String> {
    let session = state.core.session(token)?;
    let list = session.list(parse_kind(&kind)?)?;
    serde_json::to_value(list).map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn inspect(
    state: State<'_, AppState>,
    token: u64,
    kind: String,
    id: String,
) -> Result<Value, String> {
    let session = state.core.session(token)?;
    session.inspect(parse_kind(&kind)?, &id)
}

#[tauri::command(async)]
fn act(state: State<'_, AppState>, token: u64, id: String, action: String) -> Result<(), String> {
    state.core.session(token)?.act(&id, &action)
}

#[tauri::command(async)]
fn remove(
    state: State<'_, AppState>,
    token: u64,
    kind: String,
    id: String,
    force: bool,
) -> Result<(), String> {
    let session = state.core.session(token)?;
    session.remove(parse_kind(&kind)?, &id, force)
}

#[tauri::command(async)]
fn create_volume(state: State<'_, AppState>, token: u64, name: String) -> Result<(), String> {
    state.core.session(token)?.create_volume(&name)
}

#[tauri::command(async)]
fn prune(state: State<'_, AppState>, token: u64, kind: String) -> Result<Pruned, String> {
    let session = state.core.session(token)?;
    session.prune(parse_kind(&kind)?)
}

#[tauri::command(async)]
fn rename(state: State<'_, AppState>, token: u64, id: String, name: String) -> Result<(), String> {
    state.core.session(token)?.rename(&id, &name)
}

#[tauri::command(async)]
fn container_json(
    state: State<'_, AppState>,
    token: u64,
    id: String,
    what: String,
) -> Result<Value, String> {
    state.core.session(token)?.container_json(&id, &what)
}

#[tauri::command(async)]
fn features(
    state: State<'_, AppState>,
    token: u64,
    sample: Option<String>,
) -> Result<Features, String> {
    Ok(state.core.session(token)?.features(sample.as_deref()))
}

#[tauri::command(async)]
/// Pull `image` from `location` (with `host` for ECR, GAR, ACR or another registry), with the
/// login for its registry when the engine is local. Gives the reference pulled.
fn pull(
    state: State<'_, AppState>,
    token: u64,
    image: String,
    location: Option<String>,
    host: Option<String>,
) -> Result<String, String> {
    let session = state.core.session(token)?;
    session.pull(&image, location.as_deref(), host.as_deref())
}

/// Search Docker Hub through the engine.
#[tauri::command(async)]
fn search(state: State<'_, AppState>, token: u64, term: String) -> Result<Search, String> {
    Ok(state.core.session(token)?.search(&term))
}

/// The pull locations and who is logged in there (account names only).
#[tauri::command(async)]
fn registries(hosts: Vec<(String, String)>) -> Vec<LocationInfo> {
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
    state
        .core
        .registry_login(&location, &host, &user, &password)
}

/// Where the `docker` command goes. It reads files only.
#[tauri::command(async)]
fn docker_command(state: State<'_, AppState>) -> DockerCommand {
    state.core.docker_command()
}

/// Point the `docker` command at podbag (`use`), or back where it was (`back`).
#[tauri::command(async)]
fn docker_switch(state: State<'_, AppState>, to: String) -> Result<DockerCommand, String> {
    state.core.docker_switch(&to)
}

/// Docker's images, to copy to podbag. Only read; Docker is never started.
#[tauri::command(async)]
fn docker_images(
    state: State<'_, AppState>,
    hosts: Option<Vec<(String, String)>>,
) -> Result<Vec<images::ImportImage>, String> {
    state.core.docker_images(&hosts.unwrap_or_default())
}

/// Copy images from Docker to podbag (this window must use podbag).
#[tauri::command(async)]
fn import_images(state: State<'_, AppState>, token: u64, names: Vec<String>) -> Result<(), String> {
    state.core.session(token)?.import_images(&names)
}

/// Follow a container's log: `logs` events with this generation, until `logs_stop`.
#[tauri::command(async)]
fn logs_start(state: State<'_, AppState>, token: u64, id: String) -> Result<u64, String> {
    state.core.session(token)?.logs_start(&id)
}

#[tauri::command(async)]
fn logs_stop(state: State<'_, AppState>, token: u64) -> Result<(), String> {
    state.core.session(token)?.logs_stop();
    Ok(())
}

/// Watch the engine's events while `on`: a `changed` event for each change.
#[tauri::command(async)]
fn watch(state: State<'_, AppState>, token: u64, on: bool) -> Result<(), String> {
    state.core.session(token)?.watch(on);
    Ok(())
}

#[tauri::command(async)]
fn open_shell(state: State<'_, AppState>, token: u64, id: String) -> Result<(), String> {
    state.core.session(token)?.open_shell(&id)
}

/// Query the companion CLI so preferences and boot always use the same VM home.
#[tauri::command(async)]
fn vm_resources(
    state: State<'_, AppState>,
    token: u64,
    values: Option<VmResources>,
) -> Result<Value, String> {
    state.core.session(token)?.vm_resources(values)
}

/// A terminal window: `plain`, or `upgrade` (`sudo podbag upgrade`, where the user types
/// the password). The error is the command to run by hand.
#[tauri::command(async)]
fn open_terminal(state: State<'_, AppState>, what: String) -> Result<(), String> {
    state.core.open_terminal(&what)
}

/// `podbag vm start`, `stop` or `restart` (macOS and Windows).
#[tauri::command(async)]
fn vm(state: State<'_, AppState>, token: u64, action: String) -> Result<(), String> {
    let session = state.core.session(token)?;
    state.core.vm(&session, &action)
}

/// `podbag vm status --json`: null when the engine is not podbag, or on Linux (no VM).
#[tauri::command(async)]
fn vm_status(state: State<'_, AppState>, token: u64) -> Result<Value, String> {
    state.core.session(token)?.vm_status_json()
}

/// `podbag vm set`: what it printed. With `restart`, the VM restarts now.
#[tauri::command(async)]
fn vm_set(state: State<'_, AppState>, token: u64, settings: VmSettings) -> Result<String, String> {
    let session = state.core.session(token)?;
    state.core.vm_set(&session, &settings)
}

/// Switch this window to `podbag` or `docker`; saved for the TUI too.
#[tauri::command(async)]
fn set_engine(state: State<'_, AppState>, token: u64, engine: String) -> Result<(), String> {
    state.core.set_engine(token, &engine)
}

/// The versions and the last update check. `check`: `never`, `due` (when the window
/// opens: at most once a day) or `now`.
#[tauri::command(async)]
fn updates(state: State<'_, AppState>, check: String) -> Updates {
    state.core.updates(updates::parse_check(&check))
}

/// `podbag upgrade --yes [--version V]` on the `podbag` that Update upgrades: in a
/// packaged app, a separate one for the terminal (the app's own comes with the app).
#[tauri::command(async)]
fn upgrade(state: State<'_, AppState>, version: Option<String>) -> Upgraded {
    state.core.upgrade(version.as_deref())
}

/// Install the app's package of release `version` (None: the latest) and start the new
/// app. Its own `podbag` comes with it.
#[tauri::command(async)]
fn update_app(
    app: AppHandle,
    state: State<'_, AppState>,
    version: Option<String>,
) -> Result<(), String> {
    state.core.update_app(version.as_deref())?;
    // Answer the window first; then end, so the new app (or the installer) can start.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        app.exit(0);
    });
    Ok(())
}

#[tauri::command(async)]
fn open_releases() -> Result<(), String> {
    updates::open_releases()
}

/// What a terminal runs for `podbag`. None: this app is a build of its own, with no podbag
/// to offer.
#[tauri::command(async)]
fn terminal_command(state: State<'_, AppState>) -> Option<terminal_cli::Status> {
    state.core.terminal_command()
}

/// Make the `podbag` command from the app's own podbag. Ok(None): the user did not give
/// the administrator password.
#[tauri::command(async)]
fn install_terminal_command(
    state: State<'_, AppState>,
) -> Result<Option<terminal_cli::Status>, String> {
    state.core.install_terminal_command()
}

#[tauri::command]
fn take_star_prompt(state: State<'_, AppState>) -> bool {
    state.core.take_star_prompt()
}

#[tauri::command(async)]
fn open_repository() -> Result<(), String> {
    updates::open_repository()
}

#[tauri::command(async)]
fn open_training_guide(id: &str, lang: &str) -> Result<(), String> {
    training::open_training_guide(id, lang)
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.core.settings()
}

#[tauri::command]
fn set_settings(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    let settings = settings.checked();
    if settings.theme != state.core.settings().theme {
        window
            .set_theme(native_theme(settings.theme))
            .map_err(|e| e.to_string())?;
    }
    state.core.set_settings(settings)
}

#[tauri::command(async)]
fn practice_volumes(
    action: String,
    session: Option<String>,
    name: Option<String>,
    containers: Option<bool>,
    course: Option<bool>,
) -> Result<Value, String> {
    training::practice_volumes(
        &action,
        session.as_deref(),
        name.as_deref(),
        containers.unwrap_or(false),
        course.unwrap_or(false),
    )
}

#[tauri::command(async)]
fn volume_tools_lab(
    action: &str,
    package: Option<String>,
    id: Option<String>,
) -> Result<Vec<training::LessonRecord>, String> {
    training::volume_tools_lab(action, package.as_deref(), id.as_deref())
}

#[tauri::command]
fn tutorial_lab(
    state: State<'_, AppState>,
    action: lab::Action,
    value: Option<String>,
) -> Result<lab::Snapshot, String> {
    state.core.tutorial_lab(action, value.as_deref())
}

// The tray.

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    app.state::<AppState>().core.set_visible(true);
    let _ = app.emit("shown", ());
}

/// Open the window (or bring it forward) on the Settings page.
fn open_settings(app: &AppHandle) {
    show_window(app);
    let _ = app.emit("open-settings", ());
}

/// Show in the tray what the core reports.
fn update_tray(app: &AppHandle) {
    let state = app.state::<AppState>();
    let model = state.core.tray();
    if let Some(item) = lock(&state.app_settings).as_ref() {
        let _ = item.set_text(&model.settings);
    }
    let mut tray = lock(&state.tray);
    let Some(items) = tray.as_mut() else {
        return;
    };
    let _ = items.engine.set_text(&model.engine);
    match &model.update {
        Some(text) => {
            let _ = items.update.set_text(text);
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
    let _ = items.status.set_text(&model.status);
    let _ = items.running.set_text(&model.running);
    let _ = items.open.set_text(&model.open);
    let _ = items.settings.set_text(&model.settings);
    let _ = items.docker.set_text(&model.docker);
    match &model.docker_switch {
        Some(d) => {
            let _ = items.docker.set_enabled(true);
            let _ = items.docker_podbag.set_checked(d.is_podbag);
            let _ = items.docker_podbag.set_enabled(!d.is_podbag);
            let _ = items.docker_other.set_text(&d.other);
            let _ = items.docker_other.set_checked(!d.is_podbag);
            let _ = items.docker_other.set_enabled(d.is_podbag);
        }
        None => {
            let _ = items.docker.set_enabled(false);
        }
    }
    let _ = items.quit.set_text(&model.quit);
    if let Some(vm) = &items.vm {
        let _ = vm.set_text(&model.vm);
        let _ = vm.set_enabled(model.vm_enabled);
    }
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
                    if let Err(e) = app.state::<AppState>().core.docker_switch(to) {
                        eprintln!("podbag-desktop: {e}");
                    }
                    update_tray(&app);
                });
            }
            "quit" => {
                app.state::<AppState>().core.current().stop_streams();
                app.exit(0);
            }
            "vm" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    let _ = app.state::<AppState>().core.toggle_vm();
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
                std::thread::spawn(move || app.state::<AppState>().core.refresh(true));
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
            let lang = app.state::<AppState>().core.language();
            let label = podbag_desktop_core::strings::text(lang, "tray.settings", None);
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

fn main() {
    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.append_invoke_initialization_script("window.podbagWindowToolbar = true;");
    let app = builder
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .manage(AppState::new())
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
            take_star_prompt,
            terminal_command,
            install_terminal_command,
            open_repository,
            open_training_guide,
            get_settings,
            set_settings,
        ])
        .setup(|app| {
            let state = app.state::<AppState>();
            let _ = state.app.set(app.handle().clone());
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
                state.core.load_config(&dir, profile.ok().as_deref());
            }
            // Create the window with the saved appearance before the webview paints.
            let config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .ok_or("missing main window configuration")?;
            let theme = native_theme(state.core.settings().theme);
            tauri::WebviewWindowBuilder::from_config(app, config)?
                .theme(theme)
                .build()?;
            build_tray(app)?;
            build_app_menu(app)?;
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                loop {
                    handle.state::<AppState>().core.refresh(false);
                    std::thread::sleep(Duration::from_secs(5));
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // GTK applies portal theme changes to every window, including pinned themes.
            #[cfg(target_os = "linux")]
            if let WindowEvent::ThemeChanged(actual) = event {
                let preferred = native_theme(window.state::<AppState>().core.settings().theme);
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
                window.state::<AppState>().core.hidden();
            }
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("podbag-desktop: {e}");
            std::process::exit(1);
        });
    app.run(|app, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            app.state::<AppState>().core.end_lab();
        }
        // macOS: a click on the Dock icon brings the window back.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            show_window(app);
        }
        let _ = (app, event);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_theme_matches_saved_preference() {
        for (name, native) in [
            ("system", None),
            ("light", Some(tauri::Theme::Light)),
            ("dark", Some(tauri::Theme::Dark)),
        ] {
            let json = serde_json::json!({ "theme": name });
            let settings: Settings = serde_json::from_value(json).unwrap();
            assert_eq!(native_theme(settings.theme), native);
        }
    }

    #[test]
    fn every_pull_location_has_an_icon() {
        let sprite = include_str!("../ui/registry-icons.svg");
        for location in images::locations(&[], &[]) {
            assert!(
                sprite.contains(&format!("<symbol id=\"{}\"", location.id)),
                "missing icon for {}",
                location.id
            );
        }
    }
}
