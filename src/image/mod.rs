//! Container images: names, registries and the public image API.

pub mod manifest;
pub mod reference;
pub mod registry;

use crate::paths;
use crate::store::Store;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

/// A pulled image.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// The full name, like `docker.io/library/alpine:latest`.
    pub name: String,
    /// The manifest digest, `sha256:…`.
    pub digest: String,
    /// Bytes of file content in the image.
    pub size: u64,
    pub config: ImageConfig,
}

impl Image {
    /// Pull an image from its registry, or reuse it if it is already stored.
    pub fn pull(name: &str) -> Result<Image> {
        pull(name, false)
    }

    /// Like [`Image::pull`], printing progress to stderr.
    #[doc(hidden)]
    pub fn pull_with_progress(name: &str) -> Result<Image> {
        pull(name, true)
    }

    /// A stored image, by name or digest prefix. NotFound if it has not been pulled.
    pub fn get(name: &str) -> Result<Image> {
        let dirs = paths::dirs()?;
        let store = Store::open_existing(&dirs).ok_or_else(|| not_pulled(name))?;
        let (full, record) = store.find(name)?;
        Ok(Image {
            name: full,
            digest: record.digest,
            size: record.size,
            config: record.config,
        })
    }

    /// Every stored image name.
    pub fn list() -> Result<Vec<Image>> {
        let dirs = paths::dirs()?;
        let Some(store) = Store::open_existing(&dirs) else {
            return Ok(Vec::new());
        };
        Ok(store
            .list()?
            .into_iter()
            .map(|(name, r, _)| Image {
                name,
                digest: r.digest,
                size: r.size,
                config: r.config,
            })
            .collect())
    }

    /// Remove an image name; the image goes when no name is left. Returns the names removed.
    pub fn remove(name: &str) -> Result<Vec<String>> {
        let out = store_op(
            &crate::store::ops::StoreOp::RemoveImage {
                name: name.to_string(),
            },
            false,
        )?;
        from_output(out)
    }
}

/// Disk use of the image store and containers.
pub fn disk_usage() -> Result<DiskUsage> {
    let dirs = paths::dirs()?;
    match Store::open_existing(&dirs) {
        Some(store) => store.disk_usage(&dirs),
        None => Ok(DiskUsage::default()),
    }
}

/// Remove every image that no container uses.
pub fn prune() -> Result<PruneReport> {
    from_output(store_op(&crate::store::ops::StoreOp::Prune, false)?)
}

fn pull(name: &str, progress: bool) -> Result<Image> {
    let out = store_op(
        &crate::store::ops::StoreOp::Pull {
            name: name.to_string(),
            progress,
        },
        progress,
    )?;
    from_output(out)
}

#[cfg(unix)]
fn store_op(op: &crate::store::ops::StoreOp, progress: bool) -> Result<serde_json::Value> {
    crate::helper::run_store_op(op, progress)
}

#[cfg(not(unix))]
fn store_op(_op: &crate::store::ops::StoreOp, _progress: bool) -> Result<serde_json::Value> {
    Err(Error::unsupported_os())
}

fn from_output<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "the store helper sent an unreadable result",
            e.to_string(),
            "report this bug",
        )
    })
}

fn not_pulled(name: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("cannot find image {name}"),
        "it has not been pulled",
        format!("run `openpod pull {name}`"),
    )
}

/// Disk use of the image store.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct DiskUsage {
    pub images: u64,
    pub objects: u64,
    pub object_bytes: u64,
    pub containers: u64,
}

/// What `prune` removed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct PruneReport {
    pub removed_images: Vec<String>,
    pub freed_bytes: u64,
}

/// A size the way Docker prints it: powers of 1000, three significant digits.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes}B");
    }
    let digits = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let text = format!("{value:.digits$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    };
    format!("{text}{}", UNITS[unit])
}

/// The run settings an image carries: what to start and how.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct ImageConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub user: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_matches_docker() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(999), "999B");
        assert_eq!(human_size(7_600_000), "7.6MB");
        assert_eq!(human_size(1_234_567_890), "1.23GB");
        assert_eq!(human_size(3_400_000), "3.4MB");
        assert_eq!(human_size(123_456_789), "123MB");
    }
}
