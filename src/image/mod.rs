//! Container images: names, registries and the public image API.

pub mod auth;
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

/// One step of a pull, as [`Image::pull_with_events`] reports it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PullEvent {
    /// The registry named the manifest (`digest`) and its number of layers.
    Resolved {
        name: String,
        digest: String,
        layers: usize,
    },
    /// Layer `index` (from 0) was stored already.
    LayerCached { index: usize, digest: String },
    /// Layer `index` was downloaded and unpacked; `bytes` is its size.
    LayerDone {
        index: usize,
        digest: String,
        bytes: u64,
    },
}

impl Image {
    /// Find programs and libraries in a stored image that cannot run: built for another
    /// C library or CPU, or missing a library. Files are read, never run.
    pub fn check(name: &str) -> Result<crate::libc_check::Report> {
        from_output(store_op(
            &crate::store::ops::StoreOp::CheckImage {
                name: name.to_string(),
            },
            false,
        )?)
    }

    /// Pull an image from its registry, or reuse it if it is already stored.
    pub fn pull(name: &str) -> Result<Image> {
        pull(name, false)
    }

    /// Like [`Image::pull`], printing progress to stderr.
    #[doc(hidden)]
    pub fn pull_with_progress(name: &str) -> Result<Image> {
        pull(name, true)
    }

    /// Like [`Image::pull`], calling `on` for each step as it happens.
    pub fn pull_with_events(name: &str, mut on: impl FnMut(PullEvent)) -> Result<Image> {
        let op = crate::store::ops::StoreOp::Pull {
            name: name.to_string(),
            progress: false,
            events: true,
        };
        from_output(store_op_lines(&op, &mut |line| {
            if let Ok(ev) = serde_json::from_str::<PullEvent>(line) {
                on(ev);
            }
        })?)
    }

    /// Store the images of an archive read from `input`: `docker save`'s format, an OCI
    /// image layout, or both (as BuildKit writes them), maybe gzip-compressed. Names it
    /// holds move to its images.
    pub fn load(input: &mut dyn std::io::Read) -> Result<crate::store::LoadReport> {
        #[cfg(unix)]
        {
            let out =
                crate::helper::run_store_op_with_input(&crate::store::ops::StoreOp::Load, input)?;
            serde_json::from_value(out).map_err(|e| {
                Error::new(
                    ErrorKind::Internal,
                    "the store helper sent an unreadable result",
                    e.to_string(),
                    "report this bug",
                )
            })
        }
        #[cfg(not(unix))]
        {
            let _ = input;
            Err(Error::unsupported_os())
        }
    }

    /// Write the images `names` (names or ID prefixes) as one archive in `docker save`'s
    /// format to `out`. Layers are written anew from the stored files, so their digests
    /// (and the image's ID) differ from the original's.
    pub fn save(names: &[String], out: &mut dyn std::io::Write) -> Result<()> {
        #[cfg(unix)]
        {
            // Unknown names fail here, before anything is written.
            for name in names {
                Image::get(name)?;
            }
            crate::helper::run_store_op_with_output(
                &crate::store::ops::StoreOp::Save {
                    names: names.to_vec(),
                },
                out,
            )
            .map(|_| ())
        }
        #[cfg(not(unix))]
        {
            let _ = (names, out);
            Err(Error::unsupported_os())
        }
    }

    /// Give a stored image (a name or digest prefix) another name, like `docker tag`.
    /// What the name pointed to before loses it. Returns the full new name.
    pub fn tag(source: &str, name: &str) -> Result<String> {
        from_output(store_op(
            &crate::store::ops::StoreOp::Tag {
                source: source.to_string(),
                name: name.to_string(),
            },
            false,
        )?)
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

/// Remove the images without a name (a newer load or tag took it) that no container uses.
pub fn prune_dangling() -> Result<PruneReport> {
    from_output(store_op(&crate::store::ops::StoreOp::PruneDangling, false)?)
}

fn pull(name: &str, progress: bool) -> Result<Image> {
    let out = store_op(
        &crate::store::ops::StoreOp::Pull {
            name: name.to_string(),
            progress,
            events: false,
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

#[cfg(unix)]
fn store_op_lines(
    op: &crate::store::ops::StoreOp,
    on_line: &mut dyn FnMut(&str),
) -> Result<serde_json::Value> {
    crate::helper::run_store_op_lines(op, on_line)
}

#[cfg(not(unix))]
fn store_op_lines(
    _op: &crate::store::ops::StoreOp,
    _on_line: &mut dyn FnMut(&str),
) -> Result<serde_json::Value> {
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

pub(crate) fn not_pulled(name: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("cannot find image {name}"),
        "it has not been pulled",
        format!("run `podbag pull {name}`"),
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
    /// Names removed.
    pub removed_images: Vec<String>,
    /// Images that had no name, by ID.
    #[serde(default)]
    pub removed_ids: Vec<String>,
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
    /// Paths the image declares as volumes (`Volumes`); each becomes an anonymous volume.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Its health check (`HEALTHCHECK`). Images pulled before podbag read it have none.
    #[serde(default)]
    pub healthcheck: Option<crate::health::HealthSettings>,
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
