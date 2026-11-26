//! Podbag Desktop on podbag-ui: the window (`app::Desktop`), its shell, pages and
//! dialogs, on `podbag-desktop-core`'s backend (the real engine, or the mock).

pub mod app;
pub mod dialogs;
pub mod fmt;
pub mod icons;
pub mod images;
mod jobs;
pub mod lists;
#[cfg(target_os = "macos")]
mod menu;
pub mod platform;
pub mod shell;
mod theme;
pub mod tr;
#[cfg(any(
    target_os = "macos",
    windows,
    all(target_os = "linux", feature = "linux-tray")
))]
pub mod tray;
pub mod views;

pub use app::{Desktop, Msg, Options, Page};
