//! Engine and system calls of Podbag Desktop, without a UI toolkit.
//!
//! [`Desktop`] holds the app's state: the engine session, the settings, the `docker`
//! command, updates and what the tray shows. A UI talks to it through [`Api`] and
//! [`Backend`]: quick calls return at once, long work and streams report [`Event`]s on a
//! channel (or to a callback, see [`Events`]). With the `mock` feature, [`Backend::mock`]
//! gives the same API on sample data.

pub mod autostart;
pub mod backend;
pub mod compression;
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
pub mod model_runner;
pub mod network_settings;
pub mod run;
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
pub use compression::Compression;
pub use desktop::{Connection, Desktop};
pub use dockercmd::DockerCommand;
pub use engine::{EngineStatus, List, parse_kind};
pub use events::{Event, Events, Waker};
pub use session::Session;
pub use settings::{Settings, ThemePreference};
pub use tray::{EngineState, TrayModel};

pub use network_settings::NetworkSettings;
/// The engine's types, as the API gives them.
pub use podbag::client::{
    CompressionSizes, ContainerSummary, Features, ImageSummary, Kind, NetworkSummary, Port, Pruned,
    PublishedPort, RESTART_POLICIES, RunSpec, SearchResult, VolumeMount, VolumeSummary,
};
pub use podbag::config::{DnsFilter, Document as ConfigDocument, Edit as ConfigEdit};
pub use podbag::dashboard::cli::{EngineConfigRequest, VmStorageEdit};
pub use podbag::dashboard::lab;
pub use podbag::image::proxy::{Mode as RegistryProxyMode, Settings as RegistryProxySettings};
pub use podbag::network::NetworkPreferences;
pub use podbag::vm::DiskStorage;

use std::sync::{Mutex, MutexGuard};

/// A lock that a panic elsewhere does not poison.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
