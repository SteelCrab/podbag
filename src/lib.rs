//! podbag: a daemonless, rootless container engine.
//!
//! The command-line tool is a thin layer over this library.

#[cfg(unix)]
#[doc(hidden)]
pub mod api;
/// Files in and out of a container as tar archives (`docker cp`).
pub mod archive;
#[doc(hidden)]
pub mod base64;
/// Image builds from a Dockerfile.
pub mod build;
#[doc(hidden)]
pub mod caps;
#[doc(hidden)]
pub mod cgroup;
/// A client for the Docker API on podbag's socket (the TUI and the desktop app).
pub mod client;
#[doc(hidden)]
pub mod config;
pub(crate) mod container;
#[cfg(unix)]
mod containers;
mod credentials;
#[doc(hidden)]
pub mod diagnose;
#[doc(hidden)]
pub mod elf;
#[doc(hidden)]
pub mod envcheck;
mod error;
pub mod health;
#[cfg(unix)]
#[doc(hidden)]
pub mod helper;
#[doc(hidden)]
pub mod id;
#[doc(hidden)]
pub mod idmap;
#[doc(hidden)]
pub mod image;
#[cfg(unix)]
#[doc(hidden)]
pub mod keeper;
#[doc(hidden)]
pub mod libc_check;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod linux;
/// podbag on macOS: the VM host and the CLI that forwards to it.
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod macos;
#[doc(hidden)]
pub mod netfiles;
#[doc(hidden)]
pub mod netlink;
pub mod network;
#[doc(hidden)]
pub mod paths;
pub mod restart;
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
/// The macOS VM's shared parts (and, on Linux, the guest's init and agent).
#[doc(hidden)]
pub mod vm;
pub mod volume;
/// podbag on Windows: the CLI that runs commands in podbag's WSL2 distribution.
#[cfg(windows)]
#[doc(hidden)]
pub mod windows;
/// The WSL2 distribution's files, and Windows paths as WSL sees them.
#[doc(hidden)]
pub mod wsl;

pub use container::{Container, Output, Running};
#[cfg(unix)]
pub use containers::{
    ContainerHandle, ContainerInfo, CopyOut, ExecOptions, LogFollower, LogLine, MountInfo, Stream,
};
pub use error::{Error, ErrorKind, Result};
pub use image::{
    DiskUsage, Image, ImageConfig, PruneReport, VerifyReport, disk_usage, prune, verify_store,
};
pub use state::{State, Status};
pub use volume::{Access, MountRequest, Volume};
