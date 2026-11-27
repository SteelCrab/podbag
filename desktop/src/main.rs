//! Podbag Desktop on podbag-ui.
//!
//! `--mock` or `PODBAG_DESKTOP_MOCK=1`: sample data instead of the engine, with the
//! core's `PODBAG_MOCK_*` options (scenario, rows, features, language).
//!
//! `PODBAG_DESKTOP_SHOT=1` (debug builds): a window that floats without taking the
//! focus, and preview commands on stdin, for screenshots.
//!
//! The window runs inside `platform::Native`, which adds the tray (menu bar) item, the
//! macOS app menu, and the close, Dock and quit behavior.

// Release builds on Windows open no console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use podbag_desktop::platform::{Native, NativeMsg, Sender};
use podbag_desktop::{Desktop, Msg, Options, icons, images};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock;
use podbag_ui::ui::UiView;
use podbag_ui::{App, Point, Synthetic, WindowOptions};

const ID: &str = "io.github.steelcrab.podbag";

/// The app's config folder, where Podbag Desktop keeps `settings.json`.
fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", windows)))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.map(|b| b.join(ID))
}

/// The app's data folder, where the local models are kept (in `models`). The same as
/// the earlier, WebView-based app's, so its models stay.
fn data_dir() -> Option<PathBuf> {
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .map(|d| d.join(ID))
    }
    #[cfg(any(target_os = "macos", windows))]
    config_dir()
}

/// Where the earlier, WebView-based app (up to 0.3) kept its page's data. It marks an
/// update from that app, which does not get the first-launch star prompt.
fn webview_data() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let dir = std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join("Library/WebKit")
            .join(ID)
            .join("WebsiteData")
    });
    #[cfg(windows)]
    let dir = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(ID));
    #[cfg(not(any(target_os = "macos", windows)))]
    let dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join(ID));
    dir
}

fn main() {
    let mock = std::env::args().any(|a| a == "--mock")
        || std::env::var("PODBAG_DESKTOP_MOCK").is_ok_and(|v| !v.is_empty() && v != "0");
    // Debug builds: a window that floats without taking the focus, and preview commands
    // on stdin (`nav images`, `theme dark`, `close`, `tray-menu`, …), for screenshots.
    let shot = cfg!(debug_assertions) && std::env::var_os("PODBAG_DESKTOP_SHOT").is_some();
    let app = App::<NativeMsg>::new();
    icons::install();
    images::install();
    let proxy = app.proxy();
    let send: Sender = Arc::new(move |m| {
        proxy.send(m);
    });
    let waker = {
        let send = send.clone();
        move || send(NativeMsg::App(Msg::Wake))
    };
    let (backend, events) = if mock {
        Backend::mock(mock::Options::from_env(), waker.clone())
    } else {
        Backend::real(
            env!("CARGO_PKG_VERSION"),
            config_dir().as_deref(),
            data_dir().as_deref(),
            webview_data().as_deref(),
            waker.clone(),
        )
    };
    if shot {
        let proxy = app.proxy();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                // `click x y`: a click at a point of the window (logical pixels).
                let sent = match click_at(&line) {
                    Some(at) => proxy.synthetic(Synthetic::Click(at)),
                    None => proxy.send(NativeMsg::App(Msg::Command(line))),
                };
                if !sent {
                    break;
                }
            }
        });
    }
    #[cfg(any(
        target_os = "macos",
        windows,
        all(target_os = "linux", feature = "linux-tray")
    ))]
    {
        let (on_action, on_tray) = (send.clone(), send.clone());
        podbag_desktop::tray::forward_events(
            move |a| on_action(NativeMsg::Action(a)),
            move || on_tray(NativeMsg::TrayHover),
        );
    }
    let options = Options {
        inline: false,
        monitor: !mock,
        waker: Arc::new(waker),
    };
    let window = WindowOptions {
        title: "Podbag Desktop".into(),
        width: 1200.,
        height: 800.,
        min_size: Some((560., 480.)),
        activate: !shot,
        always_on_top: shot,
        debug: std::env::var_os("PODBAG_DESKTOP_LOG").is_some(),
        icon: Some(images::window_icon()),
        // desktop/linux/podbag-desktop.desktop
        app_id: Some("podbag-desktop".into()),
        ..WindowOptions::default()
    };
    // Runs on the main thread once the event loop runs, as the tray and menus need.
    app.run(window, move || {
        let tray = start_tray(&backend);
        let desktop = Desktop::new(backend, events, options);
        let mut native = Native::new(desktop, send, tray.is_some());
        if let Some(tray) = tray {
            native.set_tray(tray);
        }
        UiView::new(native)
    });
}

fn click_at(line: &str) -> Option<Point> {
    let mut words = line.split_whitespace();
    if words.next() != Some("click") {
        return None;
    }
    let x = words.next()?.parse().ok()?;
    let y = words.next()?.parse().ok()?;
    Some(Point::new(x, y))
}

/// The tray item (and on macOS the app menu); None where there is none.
fn start_tray(backend: &Backend) -> Option<Box<dyn podbag_desktop::platform::TrayView>> {
    #[cfg(any(
        target_os = "macos",
        windows,
        all(target_os = "linux", feature = "linux-tray")
    ))]
    match podbag_desktop::tray::TrayHandle::start(&backend.tray()) {
        Ok(tray) => return Some(Box::new(tray)),
        Err(e) => eprintln!("podbag-desktop: no tray item: {e}"),
    }
    let _ = backend;
    None
}
