//! macOS: the app menu (app, File, Edit, View, Window, Help) with Settings… (⌘,) after
//! About. Other systems have no app menu; the window takes Ctrl+, itself.

use tray_icon::menu::accelerator::Accelerator;
use tray_icon::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};

pub struct AppMenu {
    _menu: Menu,
    settings: MenuItem,
}

impl AppMenu {
    /// Make the menu bar's menus (main thread, once the event loop runs).
    /// `settings`: the text of Settings….
    pub fn install(settings: &str) -> Result<AppMenu, String> {
        let e = |e: tray_icon::menu::Error| e.to_string();
        let about = AboutMetadata {
            name: Some("podbag".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            ..AboutMetadata::default()
        };
        let accel: Accelerator = "CmdOrCtrl+,".parse().map_err(|e| format!("{e:?}"))?;
        let settings = MenuItem::with_id("app-settings", settings, true, Some(accel));
        let sep = PredefinedMenuItem::separator;
        let app = Submenu::with_items(
            "podbag",
            true,
            &[
                &PredefinedMenuItem::about(None, Some(about)),
                &sep(),
                &settings,
                &sep(),
                &PredefinedMenuItem::services(None),
                &sep(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &sep(),
                &PredefinedMenuItem::quit(None),
            ],
        )
        .map_err(e)?;
        let file = Submenu::with_items("File", true, &[&PredefinedMenuItem::close_window(None)])
            .map_err(e)?;
        let edit = Submenu::with_items(
            "Edit",
            true,
            &[
                &PredefinedMenuItem::undo(None),
                &PredefinedMenuItem::redo(None),
                &sep(),
                &PredefinedMenuItem::cut(None),
                &PredefinedMenuItem::copy(None),
                &PredefinedMenuItem::paste(None),
                &PredefinedMenuItem::select_all(None),
            ],
        )
        .map_err(e)?;
        let view = Submenu::with_items("View", true, &[&PredefinedMenuItem::fullscreen(None)])
            .map_err(e)?;
        let window = Submenu::with_items(
            "Window",
            true,
            &[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(None),
                &sep(),
                &PredefinedMenuItem::close_window(None),
            ],
        )
        .map_err(e)?;
        let help = Submenu::with_items("Help", true, &[]).map_err(e)?;
        let menu = Menu::with_items(&[&app, &file, &edit, &view, &window, &help]).map_err(e)?;
        menu.init_for_nsapp();
        window.set_as_windows_menu_for_nsapp();
        help.set_as_help_menu_for_nsapp();
        Ok(AppMenu {
            _menu: menu,
            settings,
        })
    }

    /// Follow the language (the tray's Settings… text).
    pub fn set_settings_text(&self, text: &str) {
        if self.settings.text() != text {
            self.settings.set_text(text);
        }
    }
}
