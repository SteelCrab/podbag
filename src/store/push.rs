//! `podbag push`: an image of the store to a registry.
//!
//! The store keeps files, not the layer archives an image came in. For each layer a push
//! sends the first of these that works:
//!
//! 1. a blob the registry has already, in this repository (nothing is sent) or in another
//!    one (the registry mounts it: a pulled image's original layers, when it goes back to
//!    the registry it came from);
//! 2. the layer written anew from its files, as `docker save` writes it, gzip-compressed.
//!    The same files and time stamp give the same blob, so its digest is kept: a second
//!    push finds it in the registry and sends nothing.
//!
//! What it learns goes to the layer's [`LayerBlobs`].

use super::{ImageRecord, KnownBlob, LayerBlobs, Store, io_error};
use crate::image::manifest;
use crate::image::reference::Reference;
use crate::image::registry::{Blob, Registry, sha256_hex};
use crate::image::{PushEvent, PushReport, short_id};
use crate::{Error, ErrorKind, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// A layer as the manifest names it.
struct Pushed {
    media_type: String,
    digest: String,
    size: u64,
    diff_id: String,
}

impl Store {
    /// Push the stored image `name` (a full name with its registry, like
    /// `localhost:5000/app:v1`) to its registry.
    pub fn push(
        &self,
        name: &str,
        registry: &mut Registry,
        progress: &mut dyn FnMut(PushEvent),
    ) -> Result<PushReport> {
        let r = Reference::parse(name)?;
        if r.digest.is_some() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("cannot push {name}"),
                "a name with a digest cannot be pushed",
                "give the image a tag with `podbag tag`, and push that",
            ));
        }
        // Shared: removals wait, so no layer or object goes during the push.
        let _shared = self.lock(false)?;
        let (full, record) = self.find(&r.name())?;
        let scratch = self.scratch_dir(&format!("push.{}", std::process::id()));
        let result = self.push_locked(&r, &full, &record, registry, &scratch, progress);
        let _ = super::remove_tree(&scratch);
        result
    }

    fn push_locked(
        &self,
        r: &Reference,
        full: &str,
        record: &ImageRecord,
        registry: &mut Registry,
        scratch: &Path,
        progress: &mut dyn FnMut(PushEvent),
    ) -> Result<PushReport> {
        progress(PushEvent::Started {
            repository: format!("{}/{}", r.registry, r.repository),
            layers: record.diff_ids.iter().rev().map(|d| short_id(d)).collect(),
        });
        let mut layers = Vec::new();
        for diff_id in &record.diff_ids {
            let mut known = self.layer_blobs(diff_id);
            let pushed = self.push_layer(
                r,
                registry,
                diff_id,
                record.created,
                &mut known,
                scratch,
                progress,
            );
            // Kept also when the push failed: blobs written or found stay known.
            self.write_layer_blobs(diff_id, &known);
            layers.push(pushed?);
        }

        // Zstd layers need OCI's format.
        let oci = record.oci || layers.iter().any(|l| l.media_type.ends_with("+zstd"));
        let diff_ids: Vec<String> = layers.iter().map(|l| l.diff_id.clone()).collect();
        let config = manifest::config_json(&record.config, &diff_ids, record.created);
        let config_digest = format!("sha256:{}", sha256_hex(&config));
        if !registry.blob_exists(r, &config_digest)? {
            registry.upload_blob(r, &config_digest, Blob::Bytes(&config))?;
        }
        let (manifest_type, config_type) = if oci {
            (manifest::OCI_MANIFEST, manifest::OCI_CONFIG)
        } else {
            (manifest::DOCKER_MANIFEST, manifest::DOCKER_CONFIG)
        };
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": manifest_type,
            "config": {"mediaType": config_type, "digest": config_digest, "size": config.len()},
            "layers": layers.iter().map(|l| serde_json::json!({
                "mediaType": layer_type(&l.media_type, oci),
                "digest": l.digest,
                "size": l.size,
            })).collect::<Vec<_>>(),
        }))
        .unwrap_or_default();
        let digest = registry.put_manifest(r, manifest_type, &body)?;
        progress(PushEvent::Done {
            tag: r.tag.clone().unwrap_or_else(|| "latest".to_string()),
            digest: digest.clone(),
            size: body.len() as u64,
        });
        Ok(PushReport {
            name: full.to_string(),
            digest,
            size: body.len() as u64,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn push_layer(
        &self,
        r: &Reference,
        registry: &mut Registry,
        diff_id: &str,
        created: u64,
        known: &mut LayerBlobs,
        scratch: &Path,
        progress: &mut dyn FnMut(PushEvent),
    ) -> Result<Pushed> {
        let id = short_id(diff_id);
        let here = format!("{}/{}", r.registry, r.repository);
        let same_registry = format!("{}/", r.registry);
        let written_now = |b: &KnownBlob| b.written_at == Some(created);

        // 1. A blob this registry has: in this repository first, then in another one.
        let mut candidates: Vec<usize> = (0..known.blobs.len())
            .filter(|&i| {
                let b = &known.blobs[i];
                // A written blob of another time stamp is not this image's layer.
                (b.written_at.is_none() || written_now(b))
                    && b.repos.iter().any(|repo| repo.starts_with(&same_registry))
            })
            .collect();
        candidates.sort_by_key(|&i| !known.blobs[i].repos.contains(&here));
        for i in candidates {
            let digest = known.blobs[i].digest.clone();
            if registry.blob_exists(r, &digest)? {
                progress(PushEvent::LayerExists { id: id.clone() });
                return Ok(hold(&mut known.blobs[i], &here));
            }
            let sources: Vec<String> = known.blobs[i]
                .repos
                .iter()
                .filter(|repo| **repo != here)
                .filter_map(|repo| repo.strip_prefix(&same_registry).map(String::from))
                .collect();
            // The blob is not where it was: forget that, keep the others.
            known.blobs[i].repos.retain(|repo| *repo != here);
            for from in sources {
                if registry.mount_blob(r, &digest, &from)? {
                    progress(PushEvent::LayerMounted {
                        id: id.clone(),
                        from: from.clone(),
                    });
                    return Ok(hold(&mut known.blobs[i], &here));
                }
            }
        }

        // 2. The layer written from its files.
        let index = self.read_layer_index(diff_id)?;
        let path = scratch.join(diff_id.strip_prefix("sha256:").unwrap_or(diff_id));
        let blob = match known.blobs.iter().position(written_now) {
            Some(i) => i,
            None => {
                let written = self.write_blob(&index, created, scratch, &path)?;
                known.blobs.push(written);
                known.blobs.len() - 1
            }
        };
        let digest = known.blobs[blob].digest.clone();
        // Not asked yet when it was not known to be in this registry.
        let asked = known.blobs[blob]
            .repos
            .iter()
            .any(|repo| repo.starts_with(&same_registry));
        if !asked && registry.blob_exists(r, &digest)? {
            progress(PushEvent::LayerExists { id: id.clone() });
            return Ok(hold(&mut known.blobs[blob], &here));
        }
        if !path.exists() {
            // Known from an earlier push: written again, the same.
            let again = self.write_blob(&index, created, scratch, &path)?;
            if again.digest != digest {
                known.blobs[blob] = again;
                let digest = known.blobs[blob].digest.clone();
                registry.upload_blob(r, &digest, Blob::File(&path))?;
                let bytes = known.blobs[blob].size;
                progress(PushEvent::LayerPushed { id, bytes });
                return Ok(hold(&mut known.blobs[blob], &here));
            }
        }
        registry.upload_blob(r, &digest, Blob::File(&path))?;
        let _ = std::fs::remove_file(&path);
        progress(PushEvent::LayerPushed {
            id,
            bytes: known.blobs[blob].size,
        });
        Ok(hold(&mut known.blobs[blob], &here))
    }

    /// Write a layer as a gzip blob at `path`, and describe it.
    fn write_blob(
        &self,
        index: &super::layer::LayerIndex,
        created: u64,
        scratch: &Path,
        path: &PathBuf,
    ) -> Result<KnownBlob> {
        std::fs::create_dir_all(scratch).map_err(|e| io_error(scratch, e))?;
        let file = std::fs::File::create(path).map_err(|e| io_error(path, e))?;
        let compressed = Hashing::new(std::io::BufWriter::with_capacity(256 * 1024, file));
        let gz = flate2::write::GzEncoder::new(compressed, flate2::Compression::default());
        let mut plain = Hashing::new(gz);
        self.write_layer(index, created, &mut plain)?;
        let (gz, diff_id, _) = plain.finish();
        let compressed = gz.finish().map_err(|e| io_error(path, e))?;
        let (mut file, digest, size) = compressed.finish();
        file.flush().map_err(|e| io_error(path, e))?;
        Ok(KnownBlob {
            digest,
            size,
            media_type: manifest::DOCKER_LAYER_GZIP.to_string(),
            diff_id,
            written_at: Some(created),
            repos: Vec::new(),
        })
    }
}

/// The blob is in this repository now; describe it for the manifest.
fn hold(blob: &mut KnownBlob, repo: &str) -> Pushed {
    if !blob.repos.iter().any(|r| r == repo) {
        blob.repos.push(repo.to_string());
    }
    Pushed {
        media_type: blob.media_type.clone(),
        digest: blob.digest.clone(),
        size: blob.size,
        diff_id: blob.diff_id.clone(),
    }
}

/// A layer's media type in the manifest's format. The bytes are the same: gzip is gzip
/// in both.
fn layer_type(media_type: &str, oci: bool) -> String {
    let gzip = media_type.ends_with("gzip");
    let plain = media_type.ends_with(".tar") || media_type.ends_with("tar.diff");
    match (oci, gzip, plain) {
        (true, true, _) => manifest::OCI_LAYER_GZIP.to_string(),
        (false, true, _) => manifest::DOCKER_LAYER_GZIP.to_string(),
        (true, false, true) => "application/vnd.oci.image.layer.v1.tar".to_string(),
        (false, false, true) => "application/vnd.docker.image.rootfs.diff.tar".to_string(),
        _ => media_type.to_string(),
    }
}

/// A writer that hashes and counts what passes through it.
struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    n: u64,
}

impl<W: Write> Hashing<W> {
    fn new(inner: W) -> Hashing<W> {
        Hashing {
            inner,
            hasher: Sha256::new(),
            n: 0,
        }
    }

    /// The inner writer, the digest (`sha256:…`) and the byte count.
    fn finish(self) -> (W, String, u64) {
        let hex: String = self
            .hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        (self.inner, format!("sha256:{hex}"), self.n)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.n += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_types_follow_the_manifest_format() {
        let oci_gz = "application/vnd.oci.image.layer.v1.tar+gzip";
        let docker_gz = "application/vnd.docker.image.rootfs.diff.tar.gzip";
        assert_eq!(layer_type(oci_gz, false), docker_gz);
        assert_eq!(layer_type(docker_gz, true), oci_gz);
        assert_eq!(layer_type(docker_gz, false), docker_gz);
        let zstd = "application/vnd.oci.image.layer.v1.tar+zstd";
        assert_eq!(layer_type(zstd, true), zstd);
        assert_eq!(
            layer_type("application/vnd.oci.image.layer.v1.tar", false),
            "application/vnd.docker.image.rootfs.diff.tar"
        );
    }
}
