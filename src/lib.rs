//! openpod: a daemonless, rootless container engine.
//!
//! The command-line tool is a thin layer over this library.

#[doc(hidden)]
pub mod config;
mod container;
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
mod linux;
#[doc(hidden)]
pub mod paths;
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

pub use container::{Container, Output, Running};
pub use error::{Error, ErrorKind, Result};
pub use image::{DiskUsage, Image, ImageConfig, PruneReport, disk_usage, prune};
