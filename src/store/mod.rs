//! The image store: content objects, layer indexes, image trees and names.

pub mod layer;
pub mod object;
pub mod ops;
pub mod tree;
pub mod zstd;

use crate::image::manifest::{self, Compression};
use crate::image::reference::Reference;
use crate::image::registry::{Registry, digest_mismatch};
use crate::image::{DiskUsage, ImageConfig, PruneReport};
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use layer::{EntryKind, LayerIndex};
use object::{ObjectStore, io_error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// What the store knows about one image.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ImageRecord {
    /// The manifest digest, `sha256:…`.
    pub digest: String,
    pub config: ImageConfig,
    pub diff_ids: Vec<String>,
    /// Bytes of file content in the image.
    pub size: u64,
    /// Seconds since the epoch.
    pub created: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct RefEntry {
    digest: String,
    last_used: u64,
}

type Refs = BTreeMap<String, RefEntry>;

/// Progress of a pull, for the CLI.
pub enum Progress<'a> {
    Resolved {
        name: &'a str,
        digest: &'a str,
        layers: usize,
    },
    LayerCached {
        index: usize,
        digest: &'a str,
    },
    LayerDone {
        index: usize,
        digest: &'a str,
        bytes: u64,
    },
}

pub struct Store {
    root: PathBuf,
    objects: ObjectStore,
    can_mknod: bool,
}

impl Store {
    /// The store in `<data>/store`. `can_mknod` is false in rootless mode.
    /// `<data>` and the store are private (0700): image files keep their setuid bits and
    /// file capabilities, so other users must not reach them.
    pub fn open(dirs: &Dirs, can_mknod: bool) -> Result<Store> {
        Store::open_with(dirs, true, can_mknod)
    }

    fn open_with(dirs: &Dirs, set_owner: bool, can_mknod: bool) -> Result<Store> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let root = dirs.data.join("store");
        for d in [&dirs.data, &root] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(d)
                .map_err(|e| io_error(d, e))?;
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| io_error(d, e))?;
        }
        Store::open_at(root, set_owner, can_mknod)
    }

    #[cfg(test)]
    pub(crate) fn open_at_dirs_for_test(dirs: &Dirs) -> Result<Store> {
        Store::open_with(dirs, false, false)
    }

    pub fn open_at(root: PathBuf, set_owner: bool, can_mknod: bool) -> Result<Store> {
        for sub in ["layers", "images"] {
            std::fs::create_dir_all(root.join(sub)).map_err(|e| io_error(&root, e))?;
        }
        let objects = ObjectStore::new(root.join("objects"), set_owner)?;
        Ok(Store {
            root,
            objects,
            can_mknod,
        })
    }

    /// Open the store for reading only, without creating anything.
    pub fn open_existing(dirs: &Dirs) -> Option<Store> {
        let root = dirs.data.join("store");
        let objects = ObjectStore::new(root.join("objects"), false).ok()?;
        root.join("images").is_dir().then_some(Store {
            root,
            objects,
            can_mknod: false,
        })
    }

    /// The store lock. Shared: pulls and container starts. Exclusive: anything that deletes.
    fn lock(&self, exclusive: bool) -> Result<Lock> {
        self.lock_file("lock", exclusive)
    }

    fn lock_file(&self, name: &str, exclusive: bool) -> Result<Lock> {
        let path = self.root.join(name);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        let op = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        // SAFETY: flock on an open descriptor.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), op) } != 0 {
            return Err(io_error(&path, std::io::Error::last_os_error()));
        }
        Ok(Lock(file))
    }

    /// Read, change and write refs.json under its own short lock.
    fn update_refs<T>(&self, change: impl FnOnce(&mut Refs) -> T) -> Result<T> {
        let _lock = self.lock_file("refs.lock", true)?;
        let mut refs = self.read_refs()?;
        let out = change(&mut refs);
        self.write_refs(&refs)?;
        Ok(out)
    }

    /// Take the shared lock: images cannot be removed while it is held.
    pub fn lock_shared(&self) -> Result<impl Drop> {
        self.lock(false)
    }

    pub fn rootfs_path(&self, digest: &str) -> PathBuf {
        self.image_dir(digest).join("rootfs")
    }

    fn image_dir(&self, digest: &str) -> PathBuf {
        self.root.join("images").join(hex_of(digest))
    }

    /// Find an image by name (`alpine`, `ghcr.io/x:1`) or by digest prefix (12+ hex characters).
    pub fn find(&self, name_or_digest: &str) -> Result<(String, ImageRecord)> {
        let refs = self.read_refs()?;
        let wanted = name_or_digest
            .strip_prefix("sha256:")
            .unwrap_or(name_or_digest);
        let looks_like_digest = wanted.len() >= 12
            && wanted.len() <= 64
            && wanted
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if looks_like_digest {
            let matches: Vec<(&String, &RefEntry)> = refs
                .iter()
                .filter(|(_, r)| hex_of(&r.digest).starts_with(wanted))
                .collect();
            if let Some((name, r)) = matches.first() {
                return Ok(((*name).clone(), self.read_record(&r.digest)?));
            }
        }
        let name = Reference::parse(name_or_digest)
            .map(|r| r.name())
            .map_err(|_| not_pulled(name_or_digest))?;
        match refs.get(&name) {
            Some(r) => Ok((name, self.read_record(&r.digest)?)),
            None => Err(not_pulled(name_or_digest)),
        }
    }

    /// Every image name with its record and the time it was last used.
    pub fn list(&self) -> Result<Vec<(String, ImageRecord, u64)>> {
        let refs = self.read_refs()?;
        refs.into_iter()
            .map(|(name, r)| Ok((name, self.read_record(&r.digest)?, r.last_used)))
            .collect()
    }

    pub fn disk_usage(&self, dirs: &Dirs) -> Result<DiskUsage> {
        let images = self
            .read_refs()?
            .values()
            .map(|r| r.digest.clone())
            .collect::<HashSet<_>>()
            .len() as u64;
        let (objects, object_bytes) = self.objects.total_size()?;
        let containers = std::fs::read_dir(dirs.data.join("containers"))
            .map(|d| d.count() as u64)
            .unwrap_or(0);
        Ok(DiskUsage {
            images,
            objects,
            object_bytes,
            containers,
        })
    }

    /// Download `name` (or reuse what is stored), build its tree, and record it.
    /// Then remove least recently used images until the store fits in `limit`.
    ///
    /// The registry is asked without any lock. Storing happens under the shared lock, so
    /// container starts and other pulls go on. Only eviction takes the exclusive lock.
    /// `in_use` is read after that lock is taken.
    pub fn pull(
        &self,
        name: &str,
        registry: &mut Registry,
        limit: u64,
        in_use: &dyn Fn() -> HashMap<String, String>,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<(String, ImageRecord)> {
        let r = Reference::parse(name)?;
        let resolved = registry.resolve(&r)?;
        let full_name = r.name();
        let digest = resolved.manifest_digest.clone();
        progress(Progress::Resolved {
            name: &full_name,
            digest: &digest,
            layers: resolved.layers.len(),
        });

        let stored = {
            let _shared = self.lock(false)?;
            self.store_image(&r, registry, &resolved, progress)
                .and_then(|()| {
                    self.update_refs(|refs| {
                        refs.insert(
                            full_name.clone(),
                            RefEntry {
                                digest: digest.clone(),
                                last_used: now(),
                            },
                        );
                    })
                })
        };
        if let Err(e) = stored {
            // Remove what this pull left behind. The exclusive lock waits for other pulls,
            // so their work in progress is never mistaken for leftovers.
            if let Ok(_exclusive) = self.lock(true) {
                let _ = self.collect_garbage();
            }
            return Err(e);
        }
        {
            let _exclusive = self.lock(true)?;
            self.enforce_limit_locked(limit, &in_use(), &digest)?;
        }
        Ok((full_name, self.read_record(&digest)?))
    }

    fn store_image(
        &self,
        r: &Reference,
        registry: &mut Registry,
        resolved: &crate::image::registry::Resolved,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if self
            .image_dir(&resolved.manifest_digest)
            .join("image.json")
            .exists()
        {
            return Ok(());
        }
        for (i, desc) in resolved.layers.iter().enumerate() {
            let diff_id = &resolved.config.diff_ids[i];
            if self.layer_path(diff_id).exists() {
                progress(Progress::LayerCached {
                    index: i,
                    digest: &desc.digest,
                });
                continue;
            }
            let compression = manifest::layer_compression(&desc.media_type)?;
            let blob = registry.open_blob(r, desc)?;
            let bytes =
                self.ingest_layer(r, blob, compression, &desc.digest, desc.size, diff_id)?;
            progress(Progress::LayerDone {
                index: i,
                digest: &desc.digest,
                bytes,
            });
        }
        self.assemble(
            &resolved.manifest_digest,
            resolved.config.config.clone(),
            resolved.config.diff_ids.clone(),
        )
    }

    /// Read one layer blob: check its digest and diff_id while storing its files.
    fn ingest_layer(
        &self,
        r: &Reference,
        blob: Box<dyn Read + Send>,
        compression: Compression,
        digest: &str,
        size: u64,
        diff_id: &str,
    ) -> Result<u64> {
        let read_error = |e: std::io::Error| {
            Error::new(
                ErrorKind::Runtime,
                format!("cannot pull {}", r.name()),
                format!("reading layer {digest}: {e}"),
                "try again",
            )
        };
        // Read at most one byte more than promised, so an endless stream cannot fill the disk.
        let mut compressed = HashingReader::new(blob.take(size.saturating_add(1)));
        let parsed: Result<(Vec<layer::Entry>, String)> = (|| {
            let decoder: Box<dyn Read + '_> = match compression {
                Compression::None => Box::new(&mut compressed),
                Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(&mut compressed)),
                Compression::Zstd => Box::new(zstd::ZstdFrames::new(std::io::BufReader::new(
                    &mut compressed,
                ))),
            };
            let mut plain = HashingReader::new(decoder);
            let entries = layer::read_layer(&mut plain, &self.objects)?;
            std::io::copy(&mut plain, &mut std::io::sink()).map_err(read_error)?;
            Ok((entries, plain.digest()))
        })();
        // Check the blob digest before reporting any parse error: a corrupted download
        // usually shows up first as a broken gzip or tar stream, and the digest names the
        // real cause.
        std::io::copy(&mut compressed, &mut std::io::sink()).map_err(read_error)?;
        if compressed.count != size {
            return Err(Error::new(
                ErrorKind::Runtime,
                format!("cannot pull {}", r.name()),
                format!(
                    "layer {digest}: the registry sent {}{} bytes, the manifest says {size}",
                    compressed.count,
                    if compressed.count > size {
                        " or more"
                    } else {
                        ""
                    }
                ),
                "try again; if it keeps failing, the registry or a proxy is corrupting data",
            ));
        }
        if compressed.digest() != digest {
            return Err(digest_mismatch(r, digest));
        }
        let (entries, plain_digest) = parsed?;
        if plain_digest != diff_id {
            return Err(digest_mismatch(r, diff_id));
        }
        write_json(
            &self.layer_path(diff_id),
            &LayerIndex {
                diff_id: diff_id.to_string(),
                entries,
            },
        )?;
        Ok(compressed.count)
    }

    /// Merge stored layers into the image tree and write the image record.
    fn assemble(&self, digest: &str, config: ImageConfig, diff_ids: Vec<String>) -> Result<()> {
        let mut layers = Vec::new();
        for id in &diff_ids {
            layers.push(self.read_layer_index(id)?.entries);
        }
        let tree = tree::merge(&layers)?;
        let mut seen = HashSet::new();
        let size = tree
            .entries
            .values()
            .filter_map(|e| match &e.kind {
                EntryKind::File { object, size } if seen.insert(object.clone()) => Some(*size),
                _ => None,
            })
            .sum();

        let dir = self.image_dir(digest);
        // Unique per process: two pulls of the same image may assemble at the same time.
        let staging =
            self.root
                .join("images")
                .join(format!("{}.{}.tmp", hex_of(digest), std::process::id()));
        let _ = remove_tree(&staging);
        std::fs::create_dir(&staging).map_err(|e| io_error(&staging, e))?;
        let result = (|| {
            let report = tree::materialize(
                &tree,
                &self.objects,
                &staging.join("rootfs"),
                self.can_mknod,
            )?;
            if report.skipped_devices > 0 {
                eprintln!(
                    "openpod: skipped {} device files: rootless mode cannot create them",
                    report.skipped_devices
                );
            }
            let record = ImageRecord {
                digest: digest.to_string(),
                config,
                diff_ids,
                size,
                created: now(),
            };
            write_json(&staging.join("image.json"), &record)?;
            match std::fs::rename(&staging, &dir) {
                Ok(()) => Ok(()),
                // Another pull finished the same image first; keep theirs.
                Err(_) if dir.join("image.json").exists() => {
                    remove_tree(&staging).map_err(|e| io_error(&staging, e))
                }
                Err(e) => Err(io_error(&dir, e)),
            }
        })();
        if result.is_err() {
            let _ = remove_tree(&staging);
        }
        result
    }

    /// Remove a name. When no name is left for its image, remove the image too.
    /// Returns the names removed.
    pub fn remove_image(
        &self,
        name: &str,
        in_use: &dyn Fn() -> HashMap<String, String>,
    ) -> Result<Vec<String>> {
        let _lock = self.lock(true)?;
        let (full_name, record) = self.find(name)?;
        if let Some(container) = in_use().get(&record.digest) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("cannot remove image {full_name}"),
                format!("container {} uses it", crate::id::short_id(container)),
                "remove the container first",
            ));
        }
        let by_digest = name.strip_prefix("sha256:").unwrap_or(name);
        let (removed, still_named) = self.update_refs(|refs| {
            let removed: Vec<String> =
                if hex_of(&record.digest).starts_with(by_digest) && full_name != name {
                    refs.iter()
                        .filter(|(_, r)| r.digest == record.digest)
                        .map(|(n, _)| n.clone())
                        .collect()
                } else {
                    vec![full_name.clone()]
                };
            for n in &removed {
                refs.remove(n);
            }
            (removed, refs.values().any(|r| r.digest == record.digest))
        })?;
        if !still_named {
            self.delete_image(&record.digest)?;
            self.collect_garbage()?;
        }
        Ok(removed)
    }

    /// Remove every image that no container uses.
    pub fn prune(&self, in_use: &dyn Fn() -> HashMap<String, String>) -> Result<PruneReport> {
        let _lock = self.lock(true)?;
        let in_use = in_use();
        let before = self.objects.total_size()?.1;
        let doomed: Vec<(String, String)> = self.update_refs(|refs| {
            let doomed: Vec<(String, String)> = refs
                .iter()
                .filter(|(_, r)| !in_use.contains_key(&r.digest))
                .map(|(n, r)| (n.clone(), r.digest.clone()))
                .collect();
            for (n, _) in &doomed {
                refs.remove(n);
            }
            doomed
        })?;
        for digest in doomed.iter().map(|(_, d)| d).collect::<HashSet<_>>() {
            self.delete_image(digest)?;
        }
        self.collect_garbage()?;
        let after = self.objects.total_size()?.1;
        Ok(PruneReport {
            removed_images: doomed.into_iter().map(|(n, _)| n).collect(),
            freed_bytes: before.saturating_sub(after),
        })
    }

    /// Remove least recently used images until the objects fit in `limit` bytes.
    /// Images in `in_use` and the image `keep` stay. Returns the names removed.
    pub fn enforce_limit(
        &self,
        limit: u64,
        in_use: &dyn Fn() -> HashMap<String, String>,
        keep: &str,
    ) -> Result<Vec<String>> {
        let _lock = self.lock(true)?;
        self.enforce_limit_locked(limit, &in_use(), keep)
    }

    fn enforce_limit_locked(
        &self,
        limit: u64,
        in_use: &HashMap<String, String>,
        keep: &str,
    ) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        while self.objects.total_size()?.1 > limit {
            let refs = self.read_refs()?;
            let mut last_used: HashMap<&str, u64> = HashMap::new();
            for r in refs.values() {
                let t = last_used.entry(&r.digest).or_insert(0);
                *t = (*t).max(r.last_used);
            }
            let victim = last_used
                .into_iter()
                .filter(|(d, _)| *d != keep && !in_use.contains_key(*d))
                .min_by_key(|(d, t)| (*t, d.to_string()))
                .map(|(d, _)| d.to_string());
            let Some(victim) = victim else { break };
            let names = self.update_refs(|refs| {
                let names: Vec<String> = refs
                    .iter()
                    .filter(|(_, r)| r.digest == victim)
                    .map(|(n, _)| n.clone())
                    .collect();
                for n in &names {
                    refs.remove(n);
                }
                names
            })?;
            self.delete_image(&victim)?;
            self.collect_garbage()?;
            removed.extend(names);
        }
        Ok(removed)
    }

    fn delete_image(&self, digest: &str) -> Result<()> {
        let dir = self.image_dir(digest);
        remove_tree(&dir).map_err(|e| io_error(&dir, e))
    }

    /// Delete layer indexes no image uses, then objects no layer index uses.
    fn collect_garbage(&self) -> Result<()> {
        let mut used_layers = HashSet::new();
        let images =
            std::fs::read_dir(self.root.join("images")).map_err(|e| io_error(&self.root, e))?;
        for dir in images.flatten() {
            if let Ok(record) = read_json::<ImageRecord>(&dir.path().join("image.json")) {
                used_layers.extend(record.diff_ids.iter().map(|d| hex_of(d).to_string()));
            } else {
                let _ = remove_tree(&dir.path()); // an unfinished pull
            }
        }
        let mut keep = HashSet::new();
        let layers_dir = self.root.join("layers");
        for f in std::fs::read_dir(&layers_dir)
            .map_err(|e| io_error(&layers_dir, e))?
            .flatten()
        {
            let name = f.file_name().to_string_lossy().to_string();
            let id = name.trim_end_matches(".json");
            if used_layers.contains(id) {
                let index: LayerIndex = read_json(&f.path())?;
                keep.extend(index.entries.into_iter().filter_map(|e| match e.kind {
                    EntryKind::File { object, .. } => Some(object),
                    _ => None,
                }));
            } else {
                let _ = std::fs::remove_file(f.path());
            }
        }
        self.objects.sweep(&keep)?;
        Ok(())
    }

    fn layer_path(&self, diff_id: &str) -> PathBuf {
        self.root
            .join("layers")
            .join(format!("{}.json", hex_of(diff_id)))
    }

    /// Load a layer index and check it again: paths must be normalized and object keys
    /// well formed, so a tampered file cannot point outside the store.
    fn read_layer_index(&self, diff_id: &str) -> Result<LayerIndex> {
        let path = self.layer_path(diff_id);
        let index: LayerIndex = read_json(&path)?;
        let normal = |p: &str| {
            layer::normalize_path(p.as_bytes())
                .ok()
                .flatten()
                .as_deref()
                == Some(p)
        };
        for e in &index.entries {
            let target_ok = match &e.kind {
                EntryKind::File { object, .. } => ObjectStore::is_valid_key(object),
                EntryKind::Hardlink { target } => normal(target),
                _ => true,
            };
            if !normal(&e.path) || !target_ok {
                return Err(damaged(&path, format!("unsafe entry {:?}", e.path)));
            }
        }
        Ok(index)
    }

    fn read_record(&self, digest: &str) -> Result<ImageRecord> {
        read_json(&self.image_dir(digest).join("image.json"))
    }

    fn read_refs(&self) -> Result<Refs> {
        match std::fs::read(self.root.join("refs.json")) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| damaged(&self.root.join("refs.json"), e))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Refs::new()),
            Err(e) => Err(io_error(&self.root, e)),
        }
    }

    fn write_refs(&self, refs: &Refs) -> Result<()> {
        write_json(&self.root.join("refs.json"), refs)
    }

    #[cfg(test)]
    fn set_ref(&self, name: &str, digest: &str, last_used: u64) -> Result<()> {
        self.update_refs(|refs| {
            refs.insert(
                name.to_string(),
                RefEntry {
                    digest: digest.to_string(),
                    last_used,
                },
            );
        })
    }

    /// Mark an image as used now, for least-recently-used eviction.
    /// Takes only the refs lock, so it never waits for a pull.
    pub fn touch(&self, name: &str) -> Result<()> {
        self.update_refs(|refs| {
            if let Some(r) = refs.get_mut(name) {
                r.last_used = now();
            }
        })
    }

    /// Import uncompressed tar layers without a registry. `last_used` orders eviction.
    #[cfg(test)]
    pub(crate) fn import_for_test(
        &self,
        name: &str,
        layers: Vec<Vec<u8>>,
        last_used: u64,
    ) -> Result<ImageRecord> {
        let r = Reference::parse(name)?;
        let diff_ids: Vec<String> = layers
            .iter()
            .map(|l| format!("sha256:{}", crate::image::registry::sha256_hex(l)))
            .collect();
        let digest = format!(
            "sha256:{}",
            crate::image::registry::sha256_hex(diff_ids.join(",").as_bytes())
        );
        for (layer, id) in layers.into_iter().zip(&diff_ids) {
            if !self.layer_path(id).exists() {
                let size = layer.len() as u64;
                self.ingest_layer(
                    &r,
                    Box::new(std::io::Cursor::new(layer)),
                    Compression::None,
                    id,
                    size,
                    id,
                )?;
            }
        }
        if !self.image_dir(&digest).exists() {
            self.assemble(&digest, ImageConfig::default(), diff_ids)?;
        }
        self.set_ref(&r.name(), &digest, last_used)?;
        self.read_record(&digest)
    }
}

struct Lock(File);

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: unlocking an open descriptor.
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN) };
    }
}

/// Digests of images used by existing containers, mapped to one container ID each.
pub fn images_in_use(dirs: &Dirs) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct Partial {
        id: String,
        #[serde(default)]
        image: Option<String>,
    }
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dirs.data.join("containers")) else {
        return out;
    };
    for e in entries.flatten() {
        if let Ok(p) = read_json::<Partial>(&e.path().join("config.json")) {
            if let Some(image) = p.image {
                out.insert(image, p.id);
            }
        }
    }
    out
}

/// Remove a directory tree, making read-only directories writable first if needed.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            make_dirs_writable(path);
            std::fs::remove_dir_all(path)
        }
        Err(e) => Err(e),
    }
}

fn make_dirs_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.is_dir() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
            if let Ok(entries) = std::fs::read_dir(path) {
                for e in entries.flatten() {
                    make_dirs_writable(&e.path());
                }
            }
        }
    }
}

/// Reads through and computes a sha256 digest and a byte count.
struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    count: u64,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        HashingReader {
            inner,
            hasher: Sha256::new(),
            count: 0,
        }
    }

    fn digest(&self) -> String {
        let hex: String = self
            .hasher
            .clone()
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256:{hex}")
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

fn hex_of(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn not_pulled(name: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("cannot find image {name}"),
        "it has not been pulled",
        format!("run `openpod pull {name}`"),
    )
}

fn damaged(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::Internal,
        "the image store is damaged",
        format!("{}: {e}", path.display()),
        "remove the file, then pull the images again",
    )
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).map_err(|e| io_error(path, e))?;
    serde_json::from_slice(&bytes).map_err(|e| damaged(path, e))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec(value).map_err(|e| damaged(path, e))?;
    let mut f = File::create(&tmp).map_err(|e| io_error(&tmp, e))?;
    f.write_all(&bytes).map_err(|e| io_error(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| io_error(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn file_layer(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            h.set_uid(0);
            h.set_gid(0);
            h.set_size(data.len() as u64);
            b.append_data(&mut h, path, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn open(tmp: &tempfile::TempDir) -> Store {
        Store::open_at(tmp.path().join("store"), false, false).unwrap()
    }

    #[test]
    fn refs_resolve_by_name_and_digest_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let (name, found) = s.find("a").unwrap();
        assert_eq!(name, "docker.io/library/a:latest");
        assert_eq!(found.digest, rec.digest);
        let prefix = &rec.digest["sha256:".len().."sha256:".len() + 12];
        assert_eq!(s.find(prefix).unwrap().1.digest, rec.digest);
        assert_eq!(s.find("b").unwrap_err().kind, ErrorKind::NotFound);
        assert!(s.rootfs_path(&rec.digest).join("x").exists());
    }

    #[test]
    fn enforce_limit_removes_least_recently_used_first() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("old", vec![file_layer(&[("u", &[1u8; 3000])])], 1)
            .unwrap();
        s.import_for_test("new", vec![file_layer(&[("u", &[2u8; 3000])])], 2)
            .unwrap();
        let removed = s.enforce_limit(4000, &HashMap::new, "").unwrap();
        assert_eq!(removed, ["docker.io/library/old:latest"]);
        let names: Vec<String> = s.list().unwrap().into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(names, ["docker.io/library/new:latest"]);
        assert_eq!(s.objects.total_size().unwrap(), (1, 3000));
    }

    #[test]
    fn enforce_limit_keeps_images_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let old = s
            .import_for_test("old", vec![file_layer(&[("u", &[1u8; 3000])])], 1)
            .unwrap();
        s.import_for_test("new", vec![file_layer(&[("u", &[2u8; 3000])])], 2)
            .unwrap();
        let in_use = || HashMap::from([(old.digest.clone(), "c".repeat(64))]);
        let removed = s.enforce_limit(4000, &in_use, "").unwrap();
        assert_eq!(removed, ["docker.io/library/new:latest"]);
    }

    #[test]
    fn remove_image_drops_unshared_layers_and_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let shared = file_layer(&[("s", b"shared")]);
        s.import_for_test(
            "a",
            vec![shared.clone(), file_layer(&[("u", b"unique")])],
            1,
        )
        .unwrap();
        s.import_for_test("b", vec![shared], 2).unwrap();
        assert_eq!(s.objects.total_size().unwrap().0, 2);
        let removed = s.remove_image("a", &HashMap::new).unwrap();
        assert_eq!(removed, ["docker.io/library/a:latest"]);
        assert_eq!(s.objects.total_size().unwrap(), (1, 6));
        assert_eq!(std::fs::read_dir(s.root.join("layers")).unwrap().count(), 1);
    }

    #[test]
    fn removing_one_of_two_names_keeps_the_image() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let layers = vec![file_layer(&[("x", b"1")])];
        s.import_for_test("a", layers.clone(), 1).unwrap();
        s.import_for_test("a2", layers, 2).unwrap();
        s.remove_image("a", &HashMap::new).unwrap();
        assert_eq!(s.objects.total_size().unwrap().0, 1);
        assert!(s.find("a2").is_ok());
    }

    // Images keep setuid bits and file capabilities. Other users must not reach them.
    #[test]
    fn store_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            data: tmp.path().join("data"),
            runtime: tmp.path().join("run"),
            config: tmp.path().join("cfg"),
        };
        Store::open_at_dirs_for_test(&dirs).unwrap();
        for d in [dirs.data.clone(), dirs.data.join("store")] {
            assert_eq!(
                std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                d.display()
            );
        }
    }

    /// "Who uses which image" must be read after the exclusive lock is taken; otherwise a
    /// container created in between loses its image.
    #[test]
    fn in_use_is_read_while_the_store_is_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let lock_path = s.root.join("lock");
        let locked = || {
            let f = std::fs::File::open(&lock_path).unwrap();
            // SAFETY: flock on an open descriptor.
            let rc = unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&f),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            };
            assert_ne!(rc, 0, "the store was not locked when in_use was read");
            HashMap::new()
        };
        s.enforce_limit(0, &locked, "").unwrap();
        s.import_for_test("b", vec![file_layer(&[("y", b"2")])], 1)
            .unwrap();
        s.remove_image("b", &locked).unwrap();
        s.prune(&locked).unwrap();
    }

    #[test]
    fn a_tampered_layer_index_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let id = format!("sha256:{}", "b".repeat(64));
        for entry in [
            r#"{"path":"/etc/cron.d/x","kind":"Dir","mode":493,"uid":0,"gid":0,"xattrs":[]}"#,
            r#"{"path":"ok","kind":{"File":{"object":"..","size":1}},"mode":420,"uid":0,"gid":0,"xattrs":[]}"#,
        ] {
            std::fs::write(
                s.layer_path(&id),
                format!(r#"{{"diff_id":"{id}","entries":[{entry}]}}"#),
            )
            .unwrap();
            let e = s.read_layer_index(&id).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Internal, "{entry}");
        }
    }

    #[test]
    fn image_in_use_cannot_be_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let in_use = || HashMap::from([(rec.digest.clone(), "c".repeat(64))]);
        let e = s.remove_image("a", &in_use).unwrap_err();
        assert_eq!(e.what, "cannot remove image docker.io/library/a:latest");
        assert!(e.why.contains("cccccccccccc uses it"), "{}", e.why);
    }
}
