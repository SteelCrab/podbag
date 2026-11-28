//! What builds and `commit` store: a layer made from a tar, images of layers made here,
//! and names for them. The layers go through the same checks and the same index as pulled
//! and loaded ones.

use super::{HashingReader, Source, Store, io_error, now, remove_tree, tree};
use crate::config::ImageCompression;
use crate::image::ImageConfig;
use crate::image::manifest::{self, Compression};
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};

/// A layer stored by a build step, and the image it made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Committed {
    /// The image of the base layers plus this one.
    pub image: String,
    pub diff_id: String,
}

/// See [`Store::hold_shared`].
pub struct SharedLock(#[allow(dead_code)] super::Lock);

impl Store {
    /// The merged tree of these stored layers.
    pub fn tree_of(&self, diff_ids: &[String]) -> Result<tree::Tree> {
        let mut layers = Vec::new();
        for id in diff_ids {
            layers.push(self.read_layer_index(id)?.entries);
        }
        tree::merge(&layers)
    }

    /// Where the object `key` (a tree entry's file) is.
    pub fn object_path(&self, key: &str) -> std::path::PathBuf {
        self.objects.path(key)
    }

    /// True when only the compressed form of the object `key` is stored: a build has the
    /// store helper thaw it ([`crate::store::ops::StoreOp::ThawLayers`]) before it reads it.
    pub fn is_cold(&self, key: &str) -> bool {
        self.objects.is_cold(key)
    }

    /// The shared store lock, held by its owner: images cannot be removed meanwhile.
    pub fn hold_shared(&self) -> Result<SharedLock> {
        Ok(SharedLock(self.lock(false)?))
    }

    /// True when the image `digest` is stored.
    pub fn has_image(&self, digest: &str) -> bool {
        self.image_dir(digest).join("image.json").exists()
    }

    /// Store the uncompressed layer tar that `write` produces, then record an image of
    /// `base` plus that layer with `config`. The image gets only its record: see
    /// [`Store::ensure_artifact`].
    pub fn commit_layer(
        &self,
        base: &[String],
        write: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
        config: ImageConfig,
        created: u64,
    ) -> Result<Committed> {
        let _shared = self.lock(false)?;
        let scratch = self.scratch_dir(&format!("commit.{}", std::process::id()));
        let _ = remove_tree(&scratch);
        std::fs::create_dir_all(&scratch).map_err(|e| io_error(&scratch, e))?;
        let result = (|| {
            let path = scratch.join("layer.tar");
            let mut file = std::fs::File::create(&path).map_err(|e| io_error(&path, e))?;
            write(&mut file)?;
            file.flush().map_err(|e| io_error(&path, e))?;
            drop(file);
            let mut hashed =
                HashingReader::new(std::fs::File::open(&path).map_err(|e| io_error(&path, e))?);
            std::io::copy(&mut hashed, &mut std::io::sink()).map_err(|e| io_error(&path, e))?;
            let (diff_id, size) = (hashed.digest(), hashed.count);
            if !self.layer_path(&diff_id).exists() {
                let file = std::fs::File::open(&path).map_err(|e| io_error(&path, e))?;
                self.ingest_layer(
                    Source::Build,
                    Box::new(file),
                    Compression::None,
                    &diff_id,
                    size,
                    &diff_id,
                )?;
            }
            let mut diff_ids = base.to_vec();
            diff_ids.push(diff_id.clone());
            let image = self.record_image(config, diff_ids, created, false, Vec::new())?;
            Ok(Committed { image, diff_id })
        })();
        let _ = remove_tree(&scratch);
        result
    }

    /// An image of stored layers with `config`, named by its config's digest as Docker
    /// names images. The same layers, config and time give the same image.
    pub fn create_image(
        &self,
        config: ImageConfig,
        diff_ids: Vec<String>,
        created: u64,
        artifact: bool,
    ) -> Result<String> {
        self.create_image_with_history(config, diff_ids, created, artifact, Vec::new())
    }

    pub(super) fn create_image_with_history(
        &self,
        config: ImageConfig,
        diff_ids: Vec<String>,
        created: u64,
        artifact: bool,
        history: Vec<manifest::HistoryEntry>,
    ) -> Result<String> {
        let _shared = self.lock(false)?;
        self.record_image(config, diff_ids, created, artifact, history)
    }

    pub(super) fn record_image(
        &self,
        config: ImageConfig,
        diff_ids: Vec<String>,
        created: u64,
        artifact: bool,
        history: Vec<manifest::HistoryEntry>,
    ) -> Result<String> {
        for id in &diff_ids {
            if !self.layer_path(id).exists() {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    "cannot make the image",
                    format!("its layer {id} is not stored"),
                    "build again without the cache (--no-cache)",
                ));
            }
        }
        let json = manifest::config_json(&config, &diff_ids, created, &history);
        let digest = format!("sha256:{}", crate::image::registry::sha256_hex(&json));
        if self.has_image(&digest) {
            // The build cache made it again: a use, so an image built often stays plain.
            // Best effort: a time that cannot be written must not fail the build.
            if self.compression.0 != ImageCompression::Off {
                let _ = self.note_use(&digest);
            }
            if artifact {
                self.ensure_artifact(&digest)?;
            }
        } else {
            self.assemble(
                super::ImageRecord {
                    digest: digest.clone(),
                    config_digest: Some(digest.clone()),
                    config,
                    diff_ids,
                    history,
                    created,
                    size: 0,
                    oci: false,
                },
                artifact,
            )?;
        }
        Ok(digest)
    }

    /// Give the image `digest` these names (a name that pointed elsewhere moves), then
    /// remove images left without a name and keep the store in `limit`. Returns the full
    /// names.
    pub fn name_image(
        &self,
        digest: &str,
        names: &[String],
        limit: u64,
        in_use: &dyn Fn() -> HashMap<String, String>,
    ) -> Result<Vec<String>> {
        let mut full_names = Vec::new();
        let mut replaced = Vec::new();
        {
            let _shared = self.lock(false)?;
            if !self.has_image(digest) {
                return Err(super::not_pulled(digest));
            }
            for name in names {
                let reference = crate::image::reference::Reference::parse(name)?;
                if reference.digest.is_some() {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("cannot name the image {name}"),
                        "a name with a digest belongs to the image it was pulled as",
                        "use a tag, like name:1",
                    ));
                }
                let full = reference.name();
                let before = self.read_refs()?.get(&full).map(|r| r.digest.clone());
                if let Some(old) = before.filter(|old| old != digest) {
                    replaced.push(old);
                }
                self.set_ref(&full, digest, now())?;
                full_names.push(full);
            }
        }
        self.after_naming(&replaced, &[digest.to_string()], limit, in_use)?;
        Ok(full_names)
    }

    /// The build cache's directory: one file per step.
    pub fn build_cache_dir(&self) -> std::path::PathBuf {
        self.root.join("build-cache")
    }

    /// Remove build cache entries whose image is gone.
    pub(super) fn sweep_build_cache(&self) {
        let dir = self.build_cache_dir();
        crate::build::cache::sweep(&dir, &|digest| self.has_image(digest));
    }
}

/// Read all of `input` into `out`; an error names the input.
pub fn copy_all(input: &mut dyn Read, out: &mut dyn Write, what: &str) -> Result<()> {
    std::io::copy(input, out)
        .map(drop)
        .map_err(|e| Error::io(format!("cannot read {what}"), &e, "try again"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Dirs;

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            data: tmp.path().join("data"),
            runtime: tmp.path().join("run"),
            config: tmp.path().join("config"),
        };
        let store = Store::open_at_dirs_for_test(&dirs).unwrap();
        (tmp, store)
    }

    fn tar_with(path: &str, body: &[u8]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_size(body.len() as u64);
        b.append_data(&mut h, path, body).unwrap();
        b.into_inner().unwrap()
    }

    #[test]
    fn layers_chain_into_images_with_stable_ids() {
        let (_tmp, store) = store();
        let config = ImageConfig {
            cmd: vec!["sh".into()],
            ..ImageConfig::default()
        };
        let first = tar_with("a", b"one");
        let c1 = store
            .commit_layer(
                &[],
                &mut |w| copy_all(&mut &first[..], w, "the layer"),
                config.clone(),
                100,
            )
            .unwrap();
        assert!(store.has_image(&c1.image));
        let second = tar_with("b/c", b"two");
        let c2 = store
            .commit_layer(
                std::slice::from_ref(&c1.diff_id),
                &mut |w| copy_all(&mut &second[..], w, "the layer"),
                config.clone(),
                101,
            )
            .unwrap();
        let tree = store
            .tree_of(&[c1.diff_id.clone(), c2.diff_id.clone()])
            .unwrap();
        let paths: Vec<&str> = tree.entries.keys().map(String::as_str).collect();
        assert_eq!(paths, ["a", "b", "b/c"]);
        // The same layers, config and time: the same image.
        let again = store
            .create_image(
                config.clone(),
                vec![c1.diff_id.clone(), c2.diff_id.clone()],
                101,
                true,
            )
            .unwrap();
        assert_eq!(again, c2.image);
        assert!(store.rootfs_path(&again).join("b/c").exists());
        // Another config: another image of the same layers.
        let other = store
            .create_image(ImageConfig::default(), vec![c1.diff_id.clone()], 100, false)
            .unwrap();
        assert_ne!(other, c1.image);
        let missing = store
            .create_image(config, vec![format!("sha256:{}", "0".repeat(64))], 1, false)
            .unwrap_err();
        assert_eq!(missing.kind, ErrorKind::NotFound);
    }

    /// The build cache makes an image again through `commit_layer` and `create_image`:
    /// with compression on, that counts as a use, so the image does not look idle.
    #[test]
    fn an_image_the_build_cache_makes_again_is_used() {
        let layer = tar_with("f", b"x");
        let commit = |store: &Store| {
            store
                .commit_layer(
                    &[],
                    &mut |w| copy_all(&mut &layer[..], w, "the layer"),
                    ImageConfig::default(),
                    1,
                )
                .unwrap()
        };
        let last_used = |store: &Store, digest: &str| store.image_dir(digest).join("last_used");

        let (_off_tmp, off) = store();
        let made = commit(&off);
        commit(&off);
        off.create_image(ImageConfig::default(), vec![made.diff_id.clone()], 1, false)
            .unwrap();
        assert!(!last_used(&off, &made.image).exists(), "compression off");

        let (_on_tmp, mut on) = store();
        on.use_compression(ImageCompression::Idle, 3600);
        let made = commit(&on);
        assert!(
            !last_used(&on, &made.image).exists(),
            "a new image is no reuse"
        );
        assert_eq!(commit(&on).image, made.image);
        assert!(last_used(&on, &made.image).exists(), "commit_layer");
        std::fs::remove_file(last_used(&on, &made.image)).unwrap();
        let again = on
            .create_image(ImageConfig::default(), vec![made.diff_id.clone()], 1, false)
            .unwrap();
        assert_eq!(again, made.image);
        assert!(last_used(&on, &made.image).exists(), "create_image");
    }

    #[test]
    fn names_move_and_old_unnamed_images_go() {
        let (_tmp, store) = store();
        let layer = tar_with("f", b"x");
        let a = store
            .commit_layer(
                &[],
                &mut |w| copy_all(&mut &layer[..], w, "the layer"),
                ImageConfig::default(),
                1,
            )
            .unwrap();
        let b = store
            .create_image(ImageConfig::default(), vec![a.diff_id.clone()], 2, true)
            .unwrap();
        let none = || HashMap::new();
        let names = store
            .name_image(&a.image, &["app:1".to_string()], u64::MAX, &none)
            .unwrap();
        assert_eq!(names, ["docker.io/library/app:1"]);
        store
            .name_image(&b, &["app:1".to_string()], u64::MAX, &none)
            .unwrap();
        assert_eq!(store.find("app:1").unwrap().1.digest, b);
        assert!(
            !store.has_image(&a.image),
            "the image that lost its name is gone"
        );
        assert!(
            store
                .name_image(&b, &["app@sha256:00".to_string()], u64::MAX, &none)
                .is_err()
        );
    }
}
