//! The tray (menu bar) item: its items, texts (the core's `TrayModel`), check states
//! and actions, on the tray-icon and muda crates (MIT/Apache-2.0,
//! https://github.com/tauri-apps/tray-icon).
//!
//! macOS and Windows: the item lives on the main thread, whose winit loop also runs
//! its events. Linux (feature `linux-tray`): GTK runs it on a thread of its own, and
//! updates go there through a channel.

use podbag_desktop_core::TrayModel;

use crate::platform::{Action, TrayView};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Send menu clicks to `on_action`, and a pointer entering or clicking the tray item
/// to `on_tray` (the window reads the engine's state again then). Both run on any
/// thread.
pub fn forward_events(
    on_action: impl Fn(Action) + Send + Sync + 'static,
    on_tray: impl Fn() + Send + Sync + 'static,
) {
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some(a) = Action::from_id(e.id.as_ref()) {
            on_action(a);
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if matches!(e, TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. }) {
            on_tray();
        }
    }));
}

fn icon() -> Result<Icon, String> {
    let png = if cfg!(target_os = "macos") {
        crate::images::TRAY_TEMPLATE
    } else {
        crate::images::TRAY_ICON
    };
    let img = podbag_ui::ImageData::from_png(png)?;
    Icon::from_rgba(img.rgba, img.width, img.height).map_err(|e| e.to_string())
}

/// The tray item and its menu.
pub struct Tray {
    // Kept alive; read only to open the menu (macOS and Windows).
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    icon: TrayIcon,
    menu: Menu,
    engine: MenuItem,
    /// "Update available": in the menu only while one is known.
    update: MenuItem,
    update_shown: bool,
    status: MenuItem,
    running: MenuItem,
    open: MenuItem,
    /// macOS: start or stop the VM.
    vm: Option<MenuItem>,
    docker: Submenu,
    docker_podbag: CheckMenuItem,
    docker_other: CheckMenuItem,
    settings: MenuItem,
    quit: MenuItem,
    shown: Option<TrayModel>,
}

impl Tray {
    /// Make the item (on the thread that runs its events) and show `model`.
    pub fn new(model: &TrayModel) -> Result<Tray, String> {
        let item = |id: &str, enabled: bool| MenuItem::with_id(id, id, enabled, None);
        let engine = item("engine", false);
        let update = item("update", true);
        let status = item("status", false);
        let running = item("running", false);
        let open = item("open", true);
        let vm = cfg!(target_os = "macos").then(|| item("vm", false));
        let settings = item("settings", true);
        let quit = item("quit", true);
        let check = |id: &str| CheckMenuItem::with_id(id, id, true, false, None);
        let docker_podbag = check("docker-podbag");
        let docker_other = check("docker-other");
        let docker =
            Submenu::with_id_and_items("docker", "docker", false, &[&docker_podbag, &docker_other])
                .map_err(|e| e.to_string())?;
        docker_podbag.set_text("podbag");
        let (separator, separator2) = (
            PredefinedMenuItem::separator(),
            PredefinedMenuItem::separator(),
        );
        let menu = Menu::new();
        let mut entries: Vec<&dyn tray_icon::menu::IsMenuItem> =
            vec![&engine, &status, &running, &separator, &open, &settings];
        if let Some(vm) = &vm {
            entries.push(vm);
        }
        entries.extend([
            &docker as &dyn tray_icon::menu::IsMenuItem,
            &separator2,
            &quit,
        ]);
        menu.append_items(&entries).map_err(|e| e.to_string())?;
        let tray_icon = TrayIconBuilder::new()
            .with_id("podbag")
            .with_icon(icon()?)
            .with_icon_as_template(cfg!(target_os = "macos"))
            .with_tooltip("podbag")
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(true)
            .build()
            .map_err(|e| e.to_string())?;
        let mut tray = Tray {
            icon: tray_icon,
            menu,
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
            shown: None,
        };
        tray.update(model);
        Ok(tray)
    }

    /// Show what the core reports (desktop/src/main.rs `update_tray`).
    pub fn update(&mut self, model: &TrayModel) {
        if self.shown.as_ref() == Some(model) {
            return;
        }
        self.shown = Some(model.clone());
        self.engine.set_text(&model.engine);
        match &model.update {
            Some(text) => {
                self.update.set_text(text);
                if !self.update_shown {
                    // After the separator, above Open podbag.
                    self.update_shown = self.menu.insert(&self.update, 4).is_ok();
                }
            }
            None if self.update_shown => {
                self.update_shown = self.menu.remove(&self.update).is_err();
            }
            None => {}
        }
        self.status.set_text(&model.status);
        self.running.set_text(&model.running);
        self.open.set_text(&model.open);
        self.settings.set_text(&model.settings);
        self.docker.set_text(&model.docker);
        match &model.docker_switch {
            Some(d) => {
                self.docker.set_enabled(true);
                self.docker_podbag.set_checked(d.is_podbag);
                self.docker_podbag.set_enabled(!d.is_podbag);
                self.docker_other.set_text(&d.other);
                self.docker_other.set_checked(!d.is_podbag);
                self.docker_other.set_enabled(d.is_podbag);
            }
            None => self.docker.set_enabled(false),
        }
        self.quit.set_text(&model.quit);
        if let Some(vm) = &self.vm {
            vm.set_text(&model.vm);
            vm.set_enabled(model.vm_enabled);
        }
    }
}

/// The tray of the running app, wherever its thread is, and on macOS the app menu.
pub struct TrayHandle {
    #[cfg(not(target_os = "linux"))]
    tray: Tray,
    #[cfg(target_os = "linux")]
    to_tray: std::sync::mpsc::Sender<TrayModel>,
    #[cfg(target_os = "macos")]
    menu: crate::menu::AppMenu,
}

impl TrayHandle {
    /// Make the tray item. macOS and Windows: call it on the main thread once the
    /// event loop runs.
    #[cfg(not(target_os = "linux"))]
    pub fn start(model: &TrayModel) -> Result<TrayHandle, String> {
        Ok(TrayHandle {
            tray: Tray::new(model)?,
            #[cfg(target_os = "macos")]
            menu: crate::menu::AppMenu::install(&model.settings)?,
        })
    }

    /// Make the tray item on a GTK thread.
    #[cfg(target_os = "linux")]
    pub fn start(model: &TrayModel) -> Result<TrayHandle, String> {
        use std::sync::mpsc;
        use std::time::Duration;
        let (to_tray, rx) = mpsc::channel::<TrayModel>();
        let (ready_tx, ready) = mpsc::channel::<Result<(), String>>();
        let first = model.clone();
        std::thread::spawn(move || {
            if let Err(e) = gtk::init() {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
            let mut tray = match Tray::new(&first) {
                Ok(t) => t,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            gtk::glib::timeout_add_local(Duration::from_millis(250), move || {
                loop {
                    match rx.try_recv() {
                        Ok(model) => tray.update(&model),
                        Err(mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            gtk::main_quit();
                            return gtk::glib::ControlFlow::Break;
                        }
                    }
                }
            });
            gtk::main();
        });
        ready
            .recv()
            .map_err(|_| "the tray thread ended".to_string())??;
        Ok(TrayHandle { to_tray })
    }
}

impl TrayView for TrayHandle {
    fn show(&mut self, model: &TrayModel) {
        #[cfg(target_os = "macos")]
        self.menu.set_settings_text(&model.settings);
        #[cfg(not(target_os = "linux"))]
        self.tray.update(model);
        #[cfg(target_os = "linux")]
        let _ = self.to_tray.send(model.clone());
    }

    #[cfg(not(target_os = "linux"))]
    fn open_menu(&self) {
        if let Some(r) = self.tray.icon.rect() {
            eprintln!(
                "tray item at {},{} {}x{}",
                r.position.x, r.position.y, r.size.width, r.size.height
            );
        }
        self.tray.icon.show_menu();
    }
}
