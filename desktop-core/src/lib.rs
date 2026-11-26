//! Engine and system calls of Podbag Desktop, without a UI toolkit.
//!
//! [`Desktop`] holds the app's state: the engine session, the settings, the `docker`
//! command, updates and what the tray shows. A UI talks to it through [`Api`] and
//! [`Backend`]: quick calls return at once, long work and streams report [`Event`]s on a
//! channel (or to a callback, see [`Events`]). With the `mock` feature, [`Backend::mock`]
//! gives the same API on sample data.

pub mod autostart;
pub mod backend;
pub mod desktop;
pub mod dockercmd;
pub mod engine;
pub mod events;
pub mod images;
pub mod kubernetes;
pub mod migration;
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
mod mock_training;
pub mod session;
pub mod settings;
pub mod shell;
pub mod strings;
pub mod terminal_cli;
pub mod training;
pub mod tray;
pub mod updates;
pub mod vm;

pub use backend::{Api, Backend, Monitor};
pub use desktop::{Connection, Desktop};
pub use dockercmd::DockerCommand;
pub use engine::{EngineStatus, List, parse_kind};
pub use events::{Event, Events, Waker};
pub use session::Session;
pub use settings::{Settings, ThemePreference};
pub use tray::{EngineState, TrayModel};

/// The engine's types, as the API gives them.
pub use podbag::client::{
    ContainerSummary, Features, ImageSummary, Kind, NetworkSummary, Port, Pruned, SearchResult,
    VolumeSummary,
};
pub use podbag::dashboard::lab;

use std::sync::{Mutex, MutexGuard};

/// A lock that a panic elsewhere does not poison.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
