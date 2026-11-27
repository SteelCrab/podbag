//! Dockerfile builds, container commits, and container filesystem changes.

pub mod cache;
pub mod commit;
pub mod dockerfile;
#[cfg(target_os = "linux")]
pub mod engine;
pub mod ignore;
#[cfg(target_os = "linux")]
pub mod layer;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod managed_worker;
pub mod plan;
pub mod shell;
pub mod steps;

pub use commit::CommitOptions;
#[cfg(target_os = "linux")]
pub use engine::{BuildOptions, build};

/// One line of `docker diff`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Change {
    /// The container path, starting with `/`.
    pub path: String,
    /// 0 changed, 1 added, 2 deleted (Docker's `Kind`).
    pub kind: u8,
}

/// What a build or a commit made.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Built {
    /// `sha256:…`
    pub id: String,
    /// The full names it got.
    pub names: Vec<String>,
}
