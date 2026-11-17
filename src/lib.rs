//! openpod: a daemonless, rootless container engine.
//!
//! The command-line tool is a thin layer over this library.

mod container;
mod error;
#[cfg(unix)]
#[doc(hidden)]
pub mod helper;
#[doc(hidden)]
pub mod id;
#[doc(hidden)]
pub mod idmap;
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

pub use container::{Container, Output, Running};
pub use error::{Error, ErrorKind, Result};
