//! Paths shared at their macOS location in the Linux VM.

use std::path::{Component, Path, PathBuf};

pub(crate) const DEFAULT: &str = "/Users";
pub(crate) const TAG: &str = "podbag-shares";
pub(crate) const CONFIG: &str = "config";
pub(crate) const MANIFEST: &str = "paths.json";
#[cfg(target_os = "linux")]
pub(crate) const ACTIVE: &str = "/run/podbag-vm-shares.json";

/// None means native Linux. A VM without usable share metadata permits no host paths.
#[cfg(target_os = "linux")]
pub(crate) fn active() -> Option<Vec<PathBuf>> {
    match std::fs::read(ACTIVE) {
        Ok(json) => Some(serde_json::from_slice(&json).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(Vec::new()),
    }
}

pub(crate) fn defaults() -> Vec<PathBuf> {
    vec![PathBuf::from(DEFAULT)]
}

pub(crate) fn entry(index: usize) -> String {
    format!("share-{index}")
}

/// A share target may only occupy a path outside the guest's system directories.
pub(crate) fn valid_target(path: &Path) -> bool {
    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir) {
        return false;
    }
    let Some(Component::Normal(top)) = parts.next() else {
        return false;
    };
    ![
        "boot", "bin", "dev", "etc", "home", "init", "lib", "lib32", "lib64", "proc", "root",
        "run", "sbin", "sys", "tmp", "usr", "var",
    ]
    .iter()
    .any(|part| top == *part)
        && parts.all(|part| matches!(part, Component::Normal(_)))
}

/// Resolve a path as far as it exists. Preserve any remaining components so -v may
/// still create a new directory below a share.
pub(crate) fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(real) => Ok(real),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A dangling link can point outside the share even though canonicalize
            // reports that its destination is missing. Never treat it as a new path.
            if path.symlink_metadata().is_ok() {
                return Err(e);
            }
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(e);
            };
            Ok(resolve(parent)?.join(name))
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn contains(roots: &[PathBuf], path: &Path) -> bool {
    path.is_absolute()
        && resolve(path).is_ok_and(|real| roots.iter().any(|root| real.starts_with(root)))
}
