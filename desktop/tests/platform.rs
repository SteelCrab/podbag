//! The app around the window, on the mock backend: the mouse's side buttons, closing
//! to the tray and reopening, the tray's actions and texts, launch at login.

use std::sync::{Arc, Mutex};

use podbag_desktop::platform::{Action, Native, NativeMsg, TrayView};
use podbag_desktop::views::settings;
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::mock::Options as MockOptions;
use podbag_desktop_core::{Backend, TrayModel};
use podbag_ui::NavButton;
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;

type H = Harness<UiView<Native>>;

/// The tray's texts as last shown.
#[derive(Clone, Default)]
struct FakeTray(Arc<Mutex<Vec<TrayModel>>>);

impl TrayView for FakeTray {
    fn show(&mut self, model: &TrayModel) {
        self.0.lock().unwrap().push(model.clone());
    }
}

impl FakeTray {
    fn last(&self) -> TrayModel {
        self.0.lock().unwrap().last().cloned().expect("shown")
    }
}

fn window(hide_on_close: bool) -> (H, FakeTray) {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        all_features: true,
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let desktop = Desktop::new(backend, events, Options::inline());
    let mut native = Native::new(desktop, Arc::new(|_| {}), hide_on_close);
    let tray = FakeTray::default();
    native.set_tray(Box::new(tray.clone()));
    (Harness::new(UiView::new(native), 1200., 800.), tray)
}

fn app(h: &H) -> &Desktop {
    &h.view().app.desktop
}

#[test]
fn side_buttons_go_back_and_forward() {
    let (mut h, _) = window(true);
    let start = app(&h).page;
    for page in [Page::Images, Page::Volumes] {
        h.click_id(shell::nav_id(page));
    }
    h.nav(NavButton::Back);
    assert_eq!(app(&h).page, Page::Images);
    h.nav(NavButton::Back);
    assert_eq!(app(&h).page, start);
    // Nothing further back.
    h.nav(NavButton::Back);
    assert_eq!(app(&h).page, start);
    h.nav(NavButton::Forward);
    h.nav(NavButton::Forward);
    assert_eq!(app(&h).page, Page::Volumes);
}

#[test]
fn side_buttons_leave_an_open_dialog_alone() {
    let (mut h, _) = window(true);
    h.click_id(shell::nav_id(Page::Images));
    h.view_mut().app.desktop.open_pull();
    h.repaint();
    h.nav(NavButton::Back);
    assert_eq!(app(&h).page, Page::Images);
}

#[test]
fn closing_hides_to_the_tray_and_the_dock_brings_it_back() {
    let (mut h, _) = window(true);
    h.close_window();
    assert!(!h.quit_requested());
    assert!(!h.window_visible());
    assert!(app(&h).hidden);
    h.reopen();
    assert!(h.window_visible());
    assert!(!app(&h).hidden);
}

#[test]
fn closing_quits_without_a_tray() {
    let (mut h, _) = window(false);
    h.close_window();
    assert!(h.quit_requested());
}

#[test]
fn tray_items_open_the_window_where_they_say() {
    let (mut h, _) = window(true);
    h.close_window();
    h.message(NativeMsg::Action(Action::Settings));
    assert!(h.window_visible());
    assert_eq!(app(&h).page, Page::Settings);

    h.close_window();
    h.message(NativeMsg::Action(Action::Update));
    assert!(h.window_visible());
    assert_eq!(app(&h).page, Page::Settings);
    assert!(app(&h).open_sections.contains("updates"));

    h.click_id(shell::nav_id(Page::Images));
    h.close_window();
    h.message(NativeMsg::Action(Action::Open));
    assert!(h.window_visible());
    assert_eq!(app(&h).page, Page::Images);

    h.message(NativeMsg::Action(Action::Quit));
    assert!(h.quit_requested());
}

#[test]
fn the_tray_shows_the_cores_texts_and_follows_changes() {
    let (mut h, tray) = window(true);
    let shown = tray.last();
    assert_eq!(shown, app(&h).api.tray());
    assert!(shown.engine.contains("podbag"), "{shown:?}");
    // A change the core reports (here: another language) reaches the tray.
    h.view_mut().app.desktop.set_language("ko");
    h.message(NativeMsg::App(Msg::Wake));
    let after = tray.last();
    assert_ne!(after.quit, shown.quit);
    assert_eq!(after, app(&h).api.tray());
}

#[test]
fn the_window_stops_polling_while_hidden() {
    let (mut h, _) = window(true);
    h.close_window();
    // Hidden, the timers do nothing and ask for no wake-up soon.
    h.timer();
    assert!(app(&h).hidden);
    h.message(NativeMsg::Action(Action::Open));
    assert!(!app(&h).hidden);
}

#[test]
fn quit_soon_ends_the_app_after_the_delay() {
    let (mut h, _) = window(true);
    h.view_mut()
        .app
        .desktop
        .quit_soon(std::time::Duration::from_millis(0));
    h.timer();
    assert!(h.quit_requested());
}

#[test]
fn launch_at_login_checkbox_switches_the_entry() {
    let (mut h, _) = window(true);
    h.click_id(shell::nav_id(Page::Settings));
    assert_eq!(app(&h).api.launch_at_login(), Ok(false));
    h.click_id(settings::LAUNCH);
    assert_eq!(app(&h).api.launch_at_login(), Ok(true));
    h.click_id(settings::LAUNCH);
    assert_eq!(app(&h).api.launch_at_login(), Ok(false));
}
