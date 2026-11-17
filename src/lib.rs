//! openpod: a daemonless, rootless container engine.
//!
//! The command-line tool is a thin layer over this library.

#[doc(hidden)]
pub mod caps;
#[doc(hidden)]
pub mod cgroup;
#[doc(hidden)]
pub mod config;
pub(crate) mod container;
#[cfg(unix)]
mod containers;
mod credentials;
mod error;
#[cfg(unix)]
#[doc(hidden)]
pub mod helper;
#[doc(hidden)]
pub mod id;
#[doc(hidden)]
pub mod idmap;
#[doc(hidden)]
pub mod image;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod linux;
#[doc(hidden)]
pub mod netfiles;
#[doc(hidden)]
pub mod netlink;
pub mod network;
#[doc(hidden)]
pub mod paths;
#[doc(hidden)]
pub mod seccomp;
#[cfg(unix)]
#[doc(hidden)]
pub mod shim;
#[cfg(unix)]
#[doc(hidden)]
pub mod signals;
#[doc(hidden)]
pub mod spec;
#[doc(hidden)]
pub mod state;
#[doc(hidden)]
pub mod store;
pub mod volume;

pub use container::{Container, Output, Running};
#[cfg(unix)]
pub use containers::{
    ContainerHandle, ContainerInfo, ExecOptions, LogFollower, LogLine, MountInfo, Stream,
};
pub use error::{Error, ErrorKind, Result};
pub use image::{DiskUsage, Image, ImageConfig, PruneReport, disk_usage, prune};
pub use state::{State, Status};
pub use volume::{Access, MountRequest, Volume};
