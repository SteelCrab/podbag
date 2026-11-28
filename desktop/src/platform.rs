//! The app as the system sees it, around `Desktop`: the window's close button and the
//! Dock icon, the tray (menu bar) item and the macOS app menu:
//!
//! - Closing the window hides it while there is a tray item to bring it back; the
//!   tray's Quit ends the app. Without a tray item (Linux without the `linux-tray`
//!   feature) closing quits.
//! - A click on the Dock icon (macOS) or the tray's Open shows the window again,
//!   which checks the engine and the updates.
//! - When the app ends, streams stop and the tutorial lab is cleaned up.

use std::sync::Arc;
use std::time::Duration;

use podbag_desktop_core::Backend;
use podbag_ui::Cx;
use podbag_ui::ui::{Theme, Ui, UiApp};

use crate::app::{Desktop, Msg, Page};

/// Whether this build has a tray item.
pub const HAS_TRAY: bool = cfg!(any(
    target_os = "macos",
    windows,
    all(target_os = "linux", feature = "linux-tray")
));

/// What a menu item asks for (tray or app menu).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    Update,
    Settings,
    /// Point the `docker` command at podbag.
    DockerPodbag,
    /// Point it back where it was.
    DockerOther,
    /// Start or stop the VM.
    Vm,
    Quit,
}

impl Action {
    /// The action of a menu item id.
    pub fn from_id(id: &str) -> Option<Action> {
        Some(match id {
            "open" => Action::Open,
            "update" => Action::Update,
            "settings" | "app-settings" => Action::Settings,
            "docker-podbag" => Action::DockerPodbag,
            "docker-other" => Action::DockerOther,
            "vm" => Action::Vm,
            "quit" => Action::Quit,
            _ => return None,
        })
    }
}

pub enum NativeMsg {
    App(Msg),
    /// A menu item was chosen.
    Action(Action),
    /// The pointer entered or clicked the tray item: read the engine's state now.
    TrayHover,
    /// Background work for the tray ended: show the core's state again.
    TrayRefresh,
}

/// Where the tray's texts go (the tray item, the app menu); a fake in tests.
pub trait TrayView {
    fn show(&mut self, model: &podbag_desktop_core::TrayModel);
    /// Open the tray's menu (screenshots).
    fn open_menu(&self) {}
}

pub type Sender = Arc<dyn Fn(NativeMsg) + Send + Sync>;

pub struct Native {
    pub desktop: Desktop,
    api: Backend,
    send: Sender,
    tray: Option<Box<dyn TrayView>>,
    /// Closing hides the window.
    hide_on_close: bool,
}

impl Native {
    /// `send` delivers messages to this view from any thread. `hide_on_close`: there is
    /// a tray item to bring the window back.
    pub fn new(desktop: Desktop, send: Sender, hide_on_close: bool) -> Native {
        Native {
            api: desktop.api.clone(),
            desktop,
            send,
            tray: None,
            hide_on_close,
        }
    }

    /// Show the core's tray state in `tray` from now on.
    pub fn set_tray(&mut self, mut tray: Box<dyn TrayView>) {
        tray.show(&self.api.tray());
        self.tray = Some(tray);
    }

    fn refresh_tray(&mut self) {
        if let Some(tray) = &mut self.tray {
            tray.show(&self.api.tray());
        }
    }

    /// Update the tray when the core said its state changed.
    fn after(&mut self) {
        if std::mem::take(&mut self.desktop.status_changed) {
            self.refresh_tray();
        }
    }

    fn background(&self, job: impl FnOnce(&Backend) + Send + 'static) {
        let (api, send) = (self.api.clone(), self.send.clone());
        std::thread::spawn(move || {
            job(&api);
            send(NativeMsg::TrayRefresh);
        });
    }

    fn show_window(&mut self, cx: &mut Cx) {
        cx.show_window();
        self.desktop.window_shown();
    }

    pub fn act(&mut self, action: Action, cx: &mut Cx) {
        match action {
            Action::Open => self.show_window(cx),
            Action::Update => {
                self.show_window(cx);
                self.desktop.open_section("updates");
            }
            Action::Settings => {
                self.show_window(cx);
                self.desktop.go(Page::Settings);
            }
            Action::DockerPodbag | Action::DockerOther => {
                let to = if action == Action::DockerPodbag {
                    "use"
                } else {
                    "back"
                };
                self.background(move |api| {
                    if let Err(e) = api.docker_switch(to) {
                        eprintln!("podbag-desktop: {e}");
                    }
                });
            }
            Action::Vm => self.background(|api| {
                let _ = api.toggle_vm();
            }),
            Action::Quit => cx.quit(),
        }
        cx.notify();
    }

    /// Preview commands of the platform side: `close`, `reopen`, `tray-menu`.
    fn command(&mut self, line: &str, cx: &mut Cx) -> bool {
        match line.trim() {
            "close" => self.close_requested(cx),
            "reopen" => self.reopen(cx),
            // Quit now, like the tray item, without waiting for a frame: a hidden or
            // occluded window may not get one.
            "quit" => self.act(Action::Quit, cx),
            line if line.starts_with("capture ") => {
                cx.capture(line["capture ".len()..].trim().into());
            }
            "tray-menu" => {
                if let Some(tray) = &self.tray {
                    tray.open_menu();
                }
            }
            _ => return false,
        }
        true
    }
}

impl UiApp for Native {
    type Message = NativeMsg;

    fn ui(&mut self, ui: &mut Ui) {
        self.desktop.ui(ui);
        self.after();
    }

    fn message(&mut self, msg: NativeMsg, cx: &mut Cx) {
        match msg {
            NativeMsg::App(Msg::Command(line)) if self.command(&line, cx) => {}
            NativeMsg::App(m) => self.desktop.message(m, cx),
            NativeMsg::Action(a) => self.act(a, cx),
            NativeMsg::TrayHover => {
                let api = self.api.clone();
                std::thread::spawn(move || api.refresh(true));
            }
            NativeMsg::TrayRefresh => self.refresh_tray(),
        }
        self.after();
    }

    fn tick(&mut self, cx: &mut Cx) -> bool {
        let paint = self.desktop.tick(cx);
        self.after();
        paint
    }

    fn theme(&self, system_dark: bool) -> Theme {
        self.desktop.theme(system_dark)
    }

    fn close_requested(&mut self, cx: &mut Cx) {
        if self.hide_on_close {
            cx.hide_window();
            self.desktop.window_hidden();
        } else {
            cx.quit();
        }
    }

    fn reopen(&mut self, cx: &mut Cx) {
        self.show_window(cx);
    }

    fn exiting(&mut self) {
        self.api.shutdown();
    }
}

/// How long the window stays after `update_app` before the app ends, so the new app
/// or its installer can start.
pub const QUIT_AFTER_UPDATE: Duration = Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_map_to_their_actions() {
        for (id, action) in [
            ("open", Action::Open),
            ("update", Action::Update),
            ("settings", Action::Settings),
            ("app-settings", Action::Settings),
            ("docker-podbag", Action::DockerPodbag),
            ("docker-other", Action::DockerOther),
            ("vm", Action::Vm),
            ("quit", Action::Quit),
        ] {
            assert_eq!(Action::from_id(id), Some(action));
        }
        for id in ["engine", "status", "running", "docker", ""] {
            assert_eq!(Action::from_id(id), None);
        }
    }
}
