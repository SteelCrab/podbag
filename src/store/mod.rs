//! The image store: content objects, layer indexes, image trees and names.

#[cfg(target_os = "linux")]
pub mod composefs;
pub mod erofs;
pub mod layer;
mod load;
pub mod method;
pub mod object;
pub mod ops;
pub(crate) mod perm;
#[cfg(unix)]
mod push;
#[cfg(unix)]
mod save;
pub mod tree;
pub mod zstd;

use crate::config::ImageMount;
use crate::image::manifest::{self, Compression};
use crate::image::reference::Reference;
use crate::image::registry::{Registry, digest_mismatch};
use crate::image::{DiskUsage, ImageConfig, PruneReport, VerifyReport};
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use layer::{EntryKind, LayerIndex};
pub use load::LoadReport;
pub use method::MountMethod;
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
    /// The config's digest, when known: the ID buildx and Docker's classic store show.
    /// Images stored before it was kept have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_digest: Option<String>,
    pub config: ImageConfig,
    pub diff_ids: Vec<String>,
    /// Bytes of file content in the image.
    pub size: u64,
    /// Seconds since the epoch.
    pub created: u64,
    /// It came in OCI's format (else Docker's schema 2): a push keeps the format.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub oci: bool,
}

/// The compressed blobs known to hold one stored layer, and where: what a push sends or
/// mounts instead of writing the layer again. Kept in `registry/<diff_id hex>.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerBlobs {
    pub blobs: Vec<KnownBlob>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KnownBlob {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    /// The digest of its uncompressed content: the layer's own for a blob that was
    /// pulled; another one for a blob podbag wrote from the stored files.
    pub diff_id: String,
    /// For a blob podbag wrote: the time stamp it gave every entry (the image's
    /// creation time). The same files and time give the same blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_at: Option<u64>,
    /// Repositories known to hold it: `registry/repository`.
    #[serde(default)]
    pub repos: Vec<String>,
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
    /// How containers get the image files; decides what a pull builds.
    method: MountMethod,
    /// Where composefs images are mounted for containers: `<runtime>/images`.
    mounts: Option<PathBuf>,
    /// False for a store opened for reading only: it never builds image files.
    can_build: bool,
}

/// The two ways an image's tree is kept for containers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Artifact {
    /// `rootfs.erofs`: the EROFS metadata composefs mounts.
    Erofs,
    /// `rootfs/`: a directory of hard links to the objects.
    Tree,
}

impl Artifact {
    fn name(self) -> &'static str {
        match self {
            Artifact::Erofs => "rootfs.erofs",
            Artifact::Tree => "rootfs",
        }
    }
}

/// Next to an image that EROFS cannot hold: the reason. The image uses its hard-link tree.
const REFUSED: &str = "rootfs.erofs.refused";

/// Where a container's lower layer comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRoot {
    /// A directory of hard links to the objects.
    Tree(PathBuf),
    /// The image's EROFS metadata mounted at `lower`, with the file contents in `objects`
    /// (overlayfs's data-only layer).
    Composefs { lower: PathBuf, objects: PathBuf },
}

impl Store {
    /// The store in `<data>/store`. `can_mknod` is false in rootless mode.
    /// `<data>` and the store are private (0700): image files keep their setuid bits and
    /// file capabilities, so other users must not reach them.
    pub fn open(dirs: &Dirs, can_mknod: bool) -> Result<Store> {
        Store::open_with(dirs, true, can_mknod)
    }

    fn open_with(dirs: &Dirs, set_owner: bool, can_mknod: bool) -> Result<Store> {
        let root = dirs.data.join("store");
        for d in [&dirs.data, &root] {
            perm::private_dir_builder()
                .recursive(true)
                .create(d)
                .map_err(|e| io_error(d, e))?;
            perm::set_mode(d, 0o700).map_err(|e| io_error(d, e))?;
        }
        let mut store = Store::open_at(root, set_owner, can_mknod)?;
        store.mounts = Some(dirs.runtime.join("images"));
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn open_at_dirs_for_test(dirs: &Dirs) -> Result<Store> {
        Store::open_with(dirs, false, false)
    }

    pub fn open_at(root: PathBuf, set_owner: bool, can_mknod: bool) -> Result<Store> {
        for sub in ["layers", "images", "registry"] {
            std::fs::create_dir_all(root.join(sub)).map_err(|e| io_error(&root, e))?;
        }
        let objects = ObjectStore::new(root.join("objects"), set_owner)?;
        Ok(Store {
            root,
            objects,
            can_mknod,
            method: MountMethod::Hardlink,
            mounts: None,
            can_build: true,
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
            method: MountMethod::Hardlink,
            mounts: Some(dirs.runtime.join("images")),
            can_build: false,
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
        let locked = if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        };
        locked.map_err(|e| io_error(&path, e))?;
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

    /// Whether composefs works on this kernel: the cached probe result, or a new probe
    /// (as root on Linux) under the shared lock, which keeps garbage collection away from
    /// the probe's scratch directory.
    pub fn composefs_support(&self) -> method::Support {
        let kernel = method::kernel_release();
        #[cfg(target_os = "linux")]
        let probe = |scratch: &Path| {
            let _shared = self.lock(false).map_err(|e| e.to_string())?;
            composefs::probe(scratch)
        };
        #[cfg(not(target_os = "linux"))]
        let probe = |_: &Path| Err("composefs needs Linux".to_string());
        method::support(&self.root, &kernel, probe)
    }

    /// Choose how containers get image files for this command: what pulls
    /// build and what `image_root` returns. Returns the method.
    pub fn use_mount_method(&mut self, rootless: bool, setting: ImageMount) -> MountMethod {
        let method = method::choose(setting, rootless, || self.composefs_support());
        self.method = method;
        method
    }

    pub fn mount_method(&self) -> MountMethod {
        self.method
    }

    /// Take the shared lock: images cannot be removed while it is held.
    pub fn lock_shared(&self) -> Result<impl Drop> {
        self.lock(false)
    }

    pub fn rootfs_path(&self, digest: &str) -> PathBuf {
        self.image_dir(digest).join("rootfs")
    }

    pub fn erofs_path(&self, digest: &str) -> PathBuf {
        self.image_dir(digest).join("rootfs.erofs")
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    /// A directory for a short job under `<store>/tmp`, which garbage collection empties.
    /// Use it only while holding the shared lock.
    pub fn scratch_dir(&self, name: &str) -> PathBuf {
        self.root.join("tmp").join(name)
    }

    #[cfg(target_os = "linux")]
    fn mount_point(&self, digest: &str) -> Option<PathBuf> {
        self.mounts.as_ref().map(|m| m.join(hex_of(digest)))
    }

    /// What a container of this image is built from: its EROFS metadata when the store uses
    /// composefs and composefs can hold the image, else its hard-link tree.
    fn artifact(&self, digest: &str) -> Artifact {
        if self.method == MountMethod::Composefs && !self.image_dir(digest).join(REFUSED).exists() {
            Artifact::Erofs
        } else {
            Artifact::Tree
        }
    }

    /// True when containers of this image get it through composefs.
    pub fn uses_composefs(&self, digest: &str) -> bool {
        self.artifact(digest) == Artifact::Erofs
    }

    /// Write `kind` for `tree` at `dest`, which must not exist.
    fn write_artifact(
        &self,
        kind: Artifact,
        tree: &tree::Tree,
        dest: &Path,
        created: u64,
    ) -> Result<()> {
        match kind {
            Artifact::Tree => {
                let report = tree::materialize(tree, &self.objects, dest, self.can_mknod, created)?;
                if report.skipped_devices > 0 {
                    eprintln!(
                        "podbag: skipped {} device files: rootless mode cannot create them",
                        report.skipped_devices
                    );
                }
                Ok(())
            }
            Artifact::Erofs => {
                let bytes = erofs::build(tree, created)?;
                std::fs::write(dest, bytes).map_err(|e| io_error(dest, e))
            }
        }
    }

    /// Write `kind` into `dir` under a temporary name, then rename it into place. When
    /// another process finished first (a directory cannot replace one), theirs stays.
    fn place(&self, dir: &Path, kind: Artifact, tree: &tree::Tree, created: u64) -> Result<()> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dest = dir.join(kind.name());
        let tmp = dir.join(format!(
            "{}.{}.{}.tmp",
            kind.name(),
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let result = self
            .write_artifact(kind, tree, &tmp, created)
            // The artifact (and the copies its tree made) reach the disk before its name.
            .and_then(|()| self.objects.sync())
            .and_then(|()| match std::fs::rename(&tmp, &dest) {
                Ok(()) => Ok(()),
                Err(_) if dest.exists() => Ok(()),
                Err(e) => Err(io_error(&dest, e)),
            });
        let _ = remove_path(&tmp);
        result
    }

    /// Write `want` into `dir`. An image that EROFS cannot hold (an inode with more than
    /// 4 KiB of extended attributes, a name longer than 255 bytes) gets a hard-link tree
    /// instead, with the reason in `rootfs.erofs.refused`: one image must not stop root
    /// mode from pulling or running it, as it runs rootless.
    fn build_in(&self, dir: &Path, want: Artifact, tree: &tree::Tree, created: u64) -> Result<()> {
        match self.place(dir, want, tree, created) {
            Err(e) if want == Artifact::Erofs && e.kind == ErrorKind::InvalidInput => {
                eprintln!(
                    "podbag: warning: this image cannot use composefs ({}); it uses a hard-link tree",
                    e.why
                );
                let refused = dir.join(REFUSED);
                std::fs::write(&refused, &e.why).map_err(|err| io_error(&refused, err))?;
                if dir.join(Artifact::Tree.name()).exists() {
                    return Ok(());
                }
                self.place(dir, Artifact::Tree, tree, created)
            }
            other => other,
        }
    }

    /// Build what a container of this image needs when the image does not have it: it was
    /// pulled with the other method (another kernel, another setting). Call it under the
    /// shared lock. Two callers may race; the first finished artifact stays.
    pub fn ensure_artifact(&self, digest: &str) -> Result<()> {
        let want = self.artifact(digest);
        let dir = self.image_dir(digest);
        if dir.join(want.name()).exists() {
            return Ok(());
        }
        let record = self.read_record(digest)?;
        if !self.can_build {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("cannot use image {}", crate::id::short_id(hex_of(digest))),
                format!("its {} is missing from the store", want.name()),
                "pull the image again (`podbag pull <image>`)",
            ));
        }
        let mut layers = Vec::new();
        for id in &record.diff_ids {
            layers.push(self.read_layer_index(id)?.entries);
        }
        let tree = tree::merge(&layers)?;
        self.build_in(&dir, want, &tree, record.created)
    }

    /// Mount the image's EROFS metadata for containers, once per image.
    #[cfg(target_os = "linux")]
    fn mount_composefs(&self, digest: &str) -> Result<ImageRoot> {
        let lower = self.mount_point(digest).ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                "cannot mount the image",
                "this store has no runtime directory for image mounts",
                "report this bug",
            )
        })?;
        composefs::mount_shared(&self.erofs_path(digest), &lower)?;
        Ok(ImageRoot::Composefs {
            lower,
            objects: self.objects_dir(),
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn mount_composefs(&self, _digest: &str) -> Result<ImageRoot> {
        Err(Error::unsupported_os())
    }

    /// The lower layer for a container of this image, built and mounted as needed. Call
    /// it under the shared lock.
    pub fn image_root(&self, digest: &str) -> Result<ImageRoot> {
        self.ensure_artifact(digest)?;
        // Asked again: building may have found that composefs cannot hold the image.
        match self.artifact(digest) {
            Artifact::Tree => Ok(ImageRoot::Tree(self.rootfs_path(digest))),
            Artifact::Erofs => self.mount_composefs(digest),
        }
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
            // An image without a name (loaded from an archive that gave none), or one asked
            // for by its config's digest (buildx reports that one): by reading the records.
            let images = std::fs::read_dir(self.root.join("images"));
            let records = images.into_iter().flatten().flatten().filter_map(|e| {
                let hex = e.file_name().to_string_lossy().into_owned();
                let record = read_json::<ImageRecord>(&e.path().join("image.json")).ok()?;
                (hex.len() == 64).then_some((hex, record))
            });
            for (hex, record) in records {
                let by_config = record
                    .config_digest
                    .as_deref()
                    .is_some_and(|c| hex_of(c).starts_with(wanted));
                if hex.starts_with(wanted) || by_config {
                    let name = refs
                        .iter()
                        .find(|(_, r)| r.digest == record.digest)
                        .map_or_else(|| record.digest.clone(), |(n, _)| n.clone());
                    return Ok((name, record));
                }
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
        // Where these blobs are, for a push to the same registry later.
        let repo = format!("{}/{}", r.registry, r.repository);
        for (desc, diff_id) in resolved.layers.iter().zip(&resolved.config.diff_ids) {
            self.note_blob(
                diff_id,
                KnownBlob {
                    digest: desc.digest.clone(),
                    size: desc.size,
                    media_type: desc.media_type.clone(),
                    diff_id: diff_id.clone(),
                    written_at: None,
                    repos: vec![repo.clone()],
                },
            );
        }
        if self
            .image_dir(&resolved.manifest_digest)
            .join("image.json")
            .exists()
        {
            // Pulled before, maybe with the other mount method.
            return self.ensure_artifact(&resolved.manifest_digest);
        }
        for (i, desc) in resolved.layers.iter().enumerate() {
            let diff_id = &resolved.config.diff_ids[i];
            if self.has_layer(diff_id) {
                progress(Progress::LayerCached {
                    index: i,
                    digest: &desc.digest,
                });
                continue;
            }
            let compression = manifest::layer_compression(&desc.media_type)?;
            let blob = registry.open_blob(r, desc)?;
            let bytes = self.ingest_layer(
                Source::Registry(r),
                blob,
                compression,
                &desc.digest,
                desc.size,
                diff_id,
            )?;
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
            Some(resolved.config_digest.clone()),
            resolved.oci,
        )
    }

    /// Read one layer blob: check its digest and diff_id while storing its files.
    fn ingest_layer(
        &self,
        source: Source<'_>,
        blob: Box<dyn Read + Send>,
        compression: Compression,
        digest: &str,
        size: u64,
        diff_id: &str,
    ) -> Result<u64> {
        let read_error = |e: std::io::Error| {
            Error::new(
                ErrorKind::Runtime,
                source.failure(),
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
                source.failure(),
                format!(
                    "layer {digest}: {} has {}{} bytes, {} says {size}",
                    source.sender(),
                    compressed.count,
                    if compressed.count > size {
                        " or more"
                    } else {
                        ""
                    },
                    source.promise(),
                ),
                "try again; if it keeps failing, the registry or a proxy is corrupting data",
            ));
        }
        if compressed.digest() != digest {
            return Err(source.mismatch(digest));
        }
        let (entries, plain_digest) = parsed?;
        if plain_digest != diff_id {
            return Err(source.mismatch(diff_id));
        }
        // The objects reach the disk before the index that names them.
        self.objects.sync()?;
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
    fn assemble(
        &self,
        digest: &str,
        config: ImageConfig,
        diff_ids: Vec<String>,
        config_digest: Option<String>,
        oci: bool,
    ) -> Result<()> {
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
            let created = now();
            let want = match self.method {
                MountMethod::Composefs => Artifact::Erofs,
                MountMethod::Hardlink => Artifact::Tree,
            };
            self.build_in(&staging, want, &tree, created)?;
            let record = ImageRecord {
                digest: digest.to_string(),
                config_digest,
                config,
                diff_ids,
                size,
                created,
                oci,
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
                ErrorKind::Conflict,
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

    /// Give the image `source` (a name or digest prefix) the name `target` too. The image
    /// `target` named before loses it, and goes when it has no name left and no container
    /// uses it. Returns the full new name.
    pub fn tag(
        &self,
        source: &str,
        target: &str,
        in_use: &dyn Fn() -> HashMap<String, String>,
    ) -> Result<String> {
        let _lock = self.lock(true)?;
        let (_, record) = self.find(source)?;
        let reference = Reference::parse(target)?;
        if reference.digest.is_some() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("cannot name image {source} {target}"),
                "a name with a digest belongs to the image it was pulled as",
                "use a tag, like name:1",
            ));
        }
        let name = reference.name();
        let digest = record.digest;
        let replaced = self.update_refs(|refs| {
            let old = refs.insert(
                name.clone(),
                RefEntry {
                    digest: digest.clone(),
                    last_used: now(),
                },
            );
            old.map(|o| o.digest)
                .filter(|d| *d != digest && !refs.values().any(|r| r.digest == *d))
        })?;
        if let Some(unnamed) = replaced {
            if !in_use().contains_key(&unnamed) {
                self.delete_image(&unnamed)?;
                self.collect_garbage()?;
            }
        }
        Ok(name)
    }

    /// Remove every image that no container uses.
    /// Remove the images without a name that no container uses (`docker image prune`).
    pub fn prune_dangling(
        &self,
        in_use: &dyn Fn() -> HashMap<String, String>,
    ) -> Result<PruneReport> {
        let _lock = self.lock(true)?;
        let in_use = in_use();
        let before = self.objects.total_size()?.1;
        let doomed: Vec<String> = self
            .unnamed_images()?
            .into_iter()
            .filter(|d| !in_use.contains_key(d))
            .collect();
        for digest in &doomed {
            self.delete_image(digest)?;
        }
        self.collect_garbage()?;
        let after = self.objects.total_size()?.1;
        Ok(PruneReport {
            removed_images: Vec::new(),
            removed_ids: doomed,
            freed_bytes: before.saturating_sub(after),
        })
    }

    /// The images no name points to (a load or a tag moved their names away, or they came
    /// without one), by digest.
    fn unnamed_images(&self) -> Result<Vec<String>> {
        let refs = self.read_refs()?;
        let named: HashSet<&str> = refs.values().map(|r| r.digest.as_str()).collect();
        let dir = self.root.join("images");
        let mut unnamed = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| io_error(&dir, e))?
            .flatten()
        {
            let hex = entry.file_name().to_string_lossy().into_owned();
            let digest = format!("sha256:{hex}");
            if crate::image::reference::is_digest(&digest)
                && entry.path().join("image.json").exists()
                && !named.contains(digest.as_str())
            {
                unnamed.push(digest);
            }
        }
        unnamed.sort();
        Ok(unnamed)
    }

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
        // And the images that had no name left.
        let unnamed: Vec<String> = self
            .unnamed_images()?
            .into_iter()
            .filter(|d| !in_use.contains_key(d))
            .collect();
        for digest in &unnamed {
            self.delete_image(digest)?;
        }
        self.collect_garbage()?;
        let after = self.objects.total_size()?.1;
        Ok(PruneReport {
            removed_images: doomed.into_iter().map(|(n, _)| n).collect(),
            removed_ids: unnamed,
            freed_bytes: before.saturating_sub(after),
        })
    }

    /// Read every object and check its contents against its name: one damaged object (a
    /// crash, a failing disk) can break every image that shares the file. `fix`: remove the
    /// images that use a damaged object, except those a container uses; garbage collection
    /// then deletes the objects that no image uses any more. `in_use` is read after the
    /// exclusive lock is taken.
    pub fn verify(
        &self,
        fix: bool,
        in_use: &dyn Fn() -> HashMap<String, String>,
    ) -> Result<VerifyReport> {
        // Reading every object takes long, and needs no lock: an object is complete under
        // its name, and one deleted meanwhile reads as gone. Containers start, pulls and
        // removals go on.
        let (objects, mut damaged) = self.objects.verify()?;
        // Removing takes the exclusive lock. A pull may have written a damaged object again
        // in the meantime (or was writing it when it was read): check those again under
        // the lock.
        let _lock = self.lock(fix)?;
        damaged.retain(|name| self.objects.is_damaged(name).unwrap_or(true));
        let bad: HashSet<&str> = damaged.iter().map(String::as_str).collect();
        let uses_bad = |e: &layer::Entry| match &e.kind {
            EntryKind::File { object, .. } => {
                bad.contains(object.as_str())
                    || e.mtime
                        .is_some_and(|t| bad.contains(ObjectStore::variant_key(object, t).as_str()))
            }
            _ => false,
        };
        let mut affected = Vec::new();
        for digest in self.image_digests()? {
            let record = self.read_record(&digest)?;
            // A layer index that cannot be read breaks the image as well.
            let broken = record.diff_ids.iter().any(|id| {
                self.read_layer_index(id)
                    .map_or(true, |index| index.entries.iter().any(uses_bad))
            });
            if broken {
                affected.push(digest);
            }
        }
        let refs = self.read_refs()?;
        let names_of = |digest: &str| -> Vec<String> {
            let names: Vec<String> = refs
                .iter()
                .filter(|(_, r)| r.digest == digest)
                .map(|(n, _)| n.clone())
                .collect();
            if names.is_empty() {
                vec![digest.to_string()]
            } else {
                names
            }
        };
        let mut report = VerifyReport {
            objects,
            images: affected.iter().flat_map(|d| names_of(d)).collect(),
            ..VerifyReport::default()
        };
        if fix {
            let used = in_use();
            for digest in &affected {
                if used.contains_key(digest) {
                    report.in_use.extend(names_of(digest));
                    continue;
                }
                self.update_refs(|refs| refs.retain(|_, r| r.digest != *digest))?;
                self.delete_image(digest)?;
                report.removed.extend(names_of(digest));
            }
            self.collect_garbage()?;
        }
        report.damaged = damaged;
        report.images.sort();
        report.removed.sort();
        report.in_use.sort();
        Ok(report)
    }

    /// Every stored image, by digest.
    fn image_digests(&self) -> Result<Vec<String>> {
        let dir = self.root.join("images");
        let mut digests: Vec<String> = std::fs::read_dir(&dir)
            .map_err(|e| io_error(&dir, e))?
            .flatten()
            .filter_map(|entry| {
                let digest = format!("sha256:{}", entry.file_name().to_string_lossy());
                (crate::image::reference::is_digest(&digest)
                    && entry.path().join("image.json").exists())
                .then_some(digest)
            })
            .collect();
        digests.sort();
        Ok(digests)
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
            let mut last_used: HashMap<String, u64> = HashMap::new();
            for r in refs.values() {
                let t = last_used.entry(r.digest.clone()).or_insert(0);
                *t = (*t).max(r.last_used);
            }
            // Images without a name go first.
            for digest in self.unnamed_images()? {
                last_used.entry(digest).or_insert(0);
            }
            let victim = last_used
                .into_iter()
                .filter(|(d, _)| d != keep && !in_use.contains_key(d))
                .min_by_key(|(d, t)| (*t, d.clone()))
                .map(|(d, _)| d);
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
        #[cfg(target_os = "linux")]
        if let Some(point) = self.mount_point(digest) {
            composefs::unmount_shared(&point).map_err(|e| {
                Error::io(
                    format!("cannot unmount {}", point.display()),
                    &e,
                    "unmount it by hand (`umount -l`), then try again",
                )
            })?;
        }
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
                // Builds that never finished (the exclusive lock keeps out running ones).
                for e in std::fs::read_dir(dir.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    if e.file_name().to_string_lossy().ends_with(".tmp") {
                        let _ = remove_path(&e.path());
                    }
                }
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
                for e in index.entries {
                    if let EntryKind::File { object, .. } = e.kind {
                        // And the object's copy with this file's time, if a tree needed one.
                        if let Some(t) = e.mtime {
                            keep.insert(ObjectStore::variant_key(&object, t));
                        }
                        keep.insert(object);
                    }
                }
            } else {
                let _ = std::fs::remove_file(f.path());
            }
        }
        // What is known of the blobs of layers that went (and files a write left).
        for f in std::fs::read_dir(self.root.join("registry"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = f.file_name().to_string_lossy().to_string();
            let used = name
                .strip_suffix(".json")
                .is_some_and(|id| used_layers.contains(id));
            if !used {
                let _ = std::fs::remove_file(f.path());
            }
        }
        self.objects.adopt_copies(&keep)?;
        self.objects.sweep(&keep)?;
        // What unfinished jobs left: inserts, record writes, and scratch directories.
        self.objects.clear_tmp();
        for f in std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = f.file_name().to_string_lossy().into_owned();
            if name.starts_with("refs.") && name.ends_with(".tmp") {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let _ = remove_tree(&self.root.join("tmp"));
        Ok(())
    }

    fn blobs_path(&self, diff_id: &str) -> PathBuf {
        self.root
            .join("registry")
            .join(format!("{}.json", hex_of(diff_id)))
    }

    /// The blobs known for a layer; none when nothing is known (or the file is damaged:
    /// it only saves work).
    pub fn layer_blobs(&self, diff_id: &str) -> LayerBlobs {
        read_json(&self.blobs_path(diff_id)).unwrap_or_default()
    }

    /// Write what is known of a layer's blobs. Failures are ignored: the next push writes
    /// the layer again.
    fn write_layer_blobs(&self, diff_id: &str, blobs: &LayerBlobs) {
        let path = self.blobs_path(diff_id);
        // Unique per process: pulls of images that share a layer write at the same time.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let written = serde_json::to_vec(blobs)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Add a blob (or a repository that holds it) to what is known of a layer.
    pub fn note_blob(&self, diff_id: &str, blob: KnownBlob) {
        let mut known = self.layer_blobs(diff_id);
        match known.blobs.iter_mut().find(|b| b.digest == blob.digest) {
            Some(b) => {
                for repo in blob.repos {
                    if !b.repos.contains(&repo) {
                        b.repos.push(repo);
                    }
                }
            }
            None => known.blobs.push(blob),
        }
        self.write_layer_blobs(diff_id, &known);
    }

    /// True when the layer is stored, with file times. An index from an older podbag has
    /// none, and a damaged one cannot be read: the layer is read again and its index
    /// replaced (the object keys stay the same).
    fn has_layer(&self, diff_id: &str) -> bool {
        self.read_layer_index(diff_id)
            .is_ok_and(|index| index.entries.iter().all(|e| e.mtime.is_some()))
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
            if !self.has_layer(id) {
                let size = layer.len() as u64;
                self.ingest_layer(
                    Source::Registry(&r),
                    Box::new(std::io::Cursor::new(layer)),
                    Compression::None,
                    id,
                    size,
                    id,
                )?;
            }
        }
        if !self.image_dir(&digest).exists() {
            self.assemble(&digest, ImageConfig::default(), diff_ids, None, false)?;
        } else {
            self.ensure_artifact(&digest)?;
        }
        self.set_ref(&r.name(), &digest, last_used)?;
        self.read_record(&digest)
    }
}

/// Where a layer comes from, for error messages.
#[derive(Clone, Copy)]
enum Source<'a> {
    Registry(&'a Reference),
    Archive,
}

impl Source<'_> {
    fn failure(self) -> String {
        match self {
            Source::Registry(r) => format!("cannot pull {}", r.name()),
            Source::Archive => "cannot load the image archive".to_string(),
        }
    }

    fn sender(self) -> &'static str {
        match self {
            Source::Registry(_) => "the registry's blob",
            Source::Archive => "the archive's blob",
        }
    }

    fn promise(self) -> &'static str {
        match self {
            Source::Registry(_) => "the manifest",
            Source::Archive => "its record",
        }
    }

    fn mismatch(self, digest: &str) -> Error {
        match self {
            Source::Registry(r) => digest_mismatch(r, digest),
            Source::Archive => Error::new(
                ErrorKind::InvalidInput,
                "cannot load the image archive",
                format!("sha256 mismatch for {digest}: the archive's content is not what it names"),
                "the archive is damaged or was changed; save it again",
            ),
        }
    }
}

struct Lock(File);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Mount the image of a container's root filesystem again when its mount is gone: image
/// mounts live in the runtime directory, which a reboot empties. Only composefs images
/// have one (root mode); a tree needs nothing.
#[cfg(target_os = "linux")]
pub fn ensure_image_mount(dirs: &Dirs, rootfs: &crate::spec::Rootfs) -> Result<()> {
    let crate::spec::Rootfs::Overlay {
        lower,
        data: Some(_),
        ..
    } = rootfs
    else {
        return Ok(());
    };
    let lower = Path::new(lower);
    if composefs::is_erofs(lower) {
        return Ok(());
    }
    let hex = lower
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let digest = format!("sha256:{hex}");
    let store = Store::open_existing(dirs)
        .ok_or_else(|| crate::image::not_pulled(&crate::id::short_id(hex)))?;
    let _shared = store.lock(false)?;
    composefs::mount_shared(&store.erofs_path(&digest), lower)
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
    // A dead container's overlay mount is taken down by the kernel a moment after its
    // last process (later still when many containers end at once), and until then it may
    // put files into its work directory: "not empty" is tried again for a short time.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match remove_tree_once(path) {
            Err(e)
                if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            done => return done,
        }
    }
}

fn remove_tree_once(path: &Path) -> std::io::Result<()> {
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

/// Remove a file, or a directory tree.
fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => remove_tree(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn make_dirs_writable(path: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.is_dir() {
            let _ = perm::set_mode(path, 0o700);
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
        format!("run `podbag pull {name}`"),
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

/// Replace `path` with `value` as JSON, whole: through a temporary file of this writer's
/// own (two pulls may write one layer's index at once), on disk before the rename.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let bytes = serde_json::to_vec(value).map_err(|e| damaged(path, e))?;
    let written = File::create(&tmp)
        .and_then(|mut f| f.write_all(&bytes).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_error(path, e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_tree_waits_out_a_late_writer() {
        // The kernel takes a dead container's overlay mount down a moment after its last
        // process; until then it may still put files into the mount's work directory.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("container");
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let writers: Vec<_> = (0..2)
            .map(|w| {
                let busy = dir.join("work");
                std::thread::spawn(move || {
                    let end = std::time::Instant::now() + std::time::Duration::from_millis(200);
                    let mut n = 0u32;
                    while std::time::Instant::now() < end {
                        let _ = std::fs::write(busy.join(format!("late-{w}-{n}")), b"");
                        n += 1;
                    }
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let removed = remove_tree(&dir);
        for w in writers {
            w.join().unwrap();
        }
        removed.unwrap();
        assert!(!dir.exists());
    }
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

    /// One file with an extended attribute (a PAX `SCHILY.xattr.*` record).
    fn xattr_layer(path: &str, data: &[u8], name: &str, value: &[u8]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let key = format!("SCHILY.xattr.{name}");
        b.append_pax_extensions([(key.as_str(), value)]).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_size(data.len() as u64);
        b.append_data(&mut h, path, data).unwrap();
        b.into_inner().unwrap()
    }

    /// Regular files with the given contents and modification times.
    fn timed_layer(files: &[(&str, &[u8], u64)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data, mtime) in files {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            h.set_uid(0);
            h.set_gid(0);
            h.set_mtime(*mtime);
            h.set_size(data.len() as u64);
            b.append_data(&mut h, path, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn mtime(path: &Path) -> u64 {
        object::mtime_of(&std::fs::metadata(path).unwrap()).unwrap()
    }

    /// The same contents with another time need an inode of their own in a hard-link
    /// tree. That copy is kept while an image needs it, and goes with the last one.
    #[test]
    fn a_time_copy_of_an_object_lives_as_long_as_an_image_needs_it() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let a = s
            .import_for_test("a", vec![timed_layer(&[("x", b"same", 100)])], 1)
            .unwrap();
        let b = s
            .import_for_test("b", vec![timed_layer(&[("y", b"same", 200)])], 2)
            .unwrap();
        assert_eq!(mtime(&s.rootfs_path(&a.digest).join("x")), 100);
        assert_eq!(mtime(&s.rootfs_path(&b.digest).join("y")), 200);
        assert_eq!(s.objects.total_size().unwrap(), (2, 8));
        // Image b still needs its copy, image a no longer needs the object's own inode.
        s.remove_image("a", &HashMap::new).unwrap();
        assert_eq!(s.objects.total_size().unwrap(), (1, 4));
        s.remove_image("b", &HashMap::new).unwrap();
        assert_eq!(s.objects.total_size().unwrap(), (0, 0));
    }

    /// When no tree links an object's own inode any more, a time copy that a tree links
    /// takes its place: the store keeps one inode, not two.
    #[test]
    fn a_time_copy_takes_the_place_of_an_object_no_tree_links() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("a", vec![timed_layer(&[("x", b"same", 100)])], 1)
            .unwrap();
        let b = s
            .import_for_test("b", vec![timed_layer(&[("y", b"same", 200)])], 2)
            .unwrap();
        s.remove_image("a", &HashMap::new).unwrap();
        let sha: String = Sha256::digest(b"same")
            .iter()
            .map(|c| format!("{c:02x}"))
            .collect();
        let object = s.objects.path(&format!("{sha}.0.0.644"));
        let y = s.rootfs_path(&b.digest).join("y");
        assert_eq!(
            std::fs::metadata(&object).unwrap().ino(),
            std::fs::metadata(&y).unwrap().ino()
        );
        assert_eq!(mtime(&object), 200);
    }

    /// A crash leaves temporary files; garbage collection removes them.
    #[test]
    fn garbage_collection_removes_leftover_temporary_files() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("a", vec![timed_layer(&[("x", b"1", 1)])], 1)
            .unwrap();
        let leftovers = [
            s.objects_dir().join("tmp").join("12345-0"),
            s.root.join("refs.12345.0.tmp"),
        ];
        for f in &leftovers {
            std::fs::write(f, b"").unwrap();
        }
        s.remove_image("a", &HashMap::new).unwrap();
        for f in &leftovers {
            assert!(!f.exists(), "{} is left", f.display());
        }
    }

    /// A layer index from an older podbag has no file times: the next pull or load of an
    /// image with that layer reads the layer again.
    #[test]
    fn a_layer_stored_without_times_is_read_again() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let layer = timed_layer(&[("x", b"1", 100)]);
        let rec = s.import_for_test("a", vec![layer.clone()], 1).unwrap();
        let path = s.layer_path(&rec.diff_ids[0]);
        let mut old: LayerIndex = read_json(&path).unwrap();
        for e in &mut old.entries {
            e.mtime = None;
        }
        write_json(&path, &old).unwrap();
        s.import_for_test("b", vec![layer, timed_layer(&[("y", b"2", 5)])], 2)
            .unwrap();
        let index: LayerIndex = read_json(&path).unwrap();
        let times: Vec<Option<u64>> = index.entries.iter().map(|e| e.mtime).collect();
        assert_eq!(times, [Some(100)]);
    }

    /// Overwrite the object of `contents` (0644, root) with other bytes of the same size.
    fn damage(s: &Store, contents: &[u8]) -> String {
        let meta = object::Meta {
            uid: 0,
            gid: 0,
            mode: 0o644,
            xattrs: vec![],
        };
        let sha: String = Sha256::digest(contents)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let key = ObjectStore::key(&sha, &meta);
        std::fs::write(s.objects.path(&key), vec![b'#'; contents.len()]).unwrap();
        key
    }

    #[test]
    fn verify_names_the_images_that_use_a_damaged_object() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("a", vec![timed_layer(&[("x", b"shared", 1)])], 1)
            .unwrap();
        s.import_for_test("b", vec![timed_layer(&[("y", b"shared", 1)])], 2)
            .unwrap();
        s.import_for_test("c", vec![timed_layer(&[("z", b"other", 1)])], 3)
            .unwrap();
        let key = damage(&s, b"shared");
        let report = s.verify(false, &HashMap::new).unwrap();
        assert_eq!(report.objects, 2);
        assert_eq!(report.damaged, [key]);
        assert_eq!(
            report.images,
            ["docker.io/library/a:latest", "docker.io/library/b:latest"]
        );
        assert!(report.removed.is_empty() && report.in_use.is_empty());
        // Nothing was removed.
        assert_eq!(s.list().unwrap().len(), 3);
    }

    /// `--fix` removes the images that use a damaged object, except those a container uses;
    /// the object goes with the last image that uses it.
    #[test]
    fn verify_fix_removes_the_images_that_use_a_damaged_object() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("a", vec![timed_layer(&[("x", b"shared", 1)])], 1)
            .unwrap();
        let b = s
            .import_for_test("b", vec![timed_layer(&[("y", b"shared", 1)])], 2)
            .unwrap();
        s.import_for_test("c", vec![timed_layer(&[("z", b"other", 1)])], 3)
            .unwrap();
        let key = damage(&s, b"shared");
        let used = || HashMap::from([(b.digest.clone(), "c1".to_string())]);
        let report = s.verify(true, &used).unwrap();
        assert_eq!(report.removed, ["docker.io/library/a:latest"]);
        assert_eq!(report.in_use, ["docker.io/library/b:latest"]);
        assert!(s.objects.path(&key).exists(), "image b still uses it");
        let report = s.verify(true, &HashMap::new).unwrap();
        assert_eq!(report.removed, ["docker.io/library/b:latest"]);
        assert!(!s.objects.path(&key).exists());
        let report = s.verify(false, &HashMap::new).unwrap();
        assert!(report.damaged.is_empty());
        let names: Vec<String> = s.list().unwrap().into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(names, ["docker.io/library/c:latest"]);
    }

    /// Two pulls that share a new layer both write its index: neither may fail.
    #[test]
    fn writers_of_one_file_do_not_disturb_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.json");
        let failures: usize = std::thread::scope(|s| {
            let writers: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        (0..40)
                            .filter(|n| write_json(&path, &vec![*n; 100]).is_err())
                            .count()
                    })
                })
                .collect();
            writers.into_iter().map(|w| w.join().unwrap()).sum()
        });
        assert_eq!(failures, 0);
        let last: Vec<i32> = read_json(&path).unwrap();
        assert_eq!(last.len(), 100);
    }

    #[test]
    fn an_image_composefs_cannot_hold_gets_a_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        s.method = MountMethod::Composefs;
        let big = vec![b'v'; 5000];
        let rec = s
            .import_for_test("a", vec![xattr_layer("x", b"1", "user.big", &big)], 1)
            .unwrap();
        assert!(s.rootfs_path(&rec.digest).join("x").exists());
        assert!(!s.erofs_path(&rec.digest).exists());
        assert_eq!(
            s.image_root(&rec.digest).unwrap(),
            ImageRoot::Tree(s.rootfs_path(&rec.digest))
        );
        // A store whose image has only its tree (pulled with hard links) keeps using it.
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![xattr_layer("x", b"1", "user.big", &big)], 1)
            .unwrap();
        s.method = MountMethod::Composefs;
        assert_eq!(
            s.image_root(&rec.digest).unwrap(),
            ImageRoot::Tree(s.rootfs_path(&rec.digest))
        );
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
        // What is known of each layer's blobs goes with the layer.
        for id in s.find("a").unwrap().1.diff_ids {
            s.note_blob(
                &id,
                KnownBlob {
                    digest: id.clone(),
                    size: 1,
                    media_type: manifest::DOCKER_LAYER_GZIP.into(),
                    diff_id: id.clone(),
                    written_at: None,
                    repos: vec!["example.com/a".into()],
                },
            );
        }
        let removed = s.remove_image("a", &HashMap::new).unwrap();
        assert_eq!(removed, ["docker.io/library/a:latest"]);
        assert_eq!(s.objects.total_size().unwrap(), (1, 6));
        assert_eq!(std::fs::read_dir(s.root.join("layers")).unwrap().count(), 1);
        assert_eq!(
            std::fs::read_dir(s.root.join("registry")).unwrap().count(),
            1
        );
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

    #[test]
    fn composefs_pulls_write_only_the_erofs_image() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        s.method = MountMethod::Composefs;
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let image = std::fs::read(s.erofs_path(&rec.digest)).unwrap();
        assert_eq!(&image[1024..1028], &0xE0F5_E1E2u32.to_le_bytes());
        assert!(!s.rootfs_path(&rec.digest).exists());
    }

    #[test]
    fn a_changed_method_builds_the_missing_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        s.method = MountMethod::Composefs;
        s.ensure_artifact(&rec.digest).unwrap();
        assert!(s.erofs_path(&rec.digest).is_file());
        assert!(s.rootfs_path(&rec.digest).join("x").exists());

        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        s.method = MountMethod::Composefs;
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        s.method = MountMethod::Hardlink;
        s.ensure_artifact(&rec.digest).unwrap();
        assert!(s.rootfs_path(&rec.digest).join("x").exists());
        // Nothing half-built is left next to them.
        let leftovers: Vec<_> = std::fs::read_dir(s.image_dir(&rec.digest))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn pulling_again_builds_a_missing_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        let layers = vec![file_layer(&[("x", b"1")])];
        let rec = s.import_for_test("a", layers.clone(), 1).unwrap();
        s.method = MountMethod::Composefs;
        s.import_for_test("a", layers, 2).unwrap();
        assert!(s.erofs_path(&rec.digest).is_file());
    }

    #[test]
    fn a_read_only_store_does_not_build() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            data: tmp.path().join("data"),
            runtime: tmp.path().join("run"),
            config: tmp.path().join("cfg"),
        };
        let rec = Store::open_at_dirs_for_test(&dirs)
            .unwrap()
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let mut ro = Store::open_existing(&dirs).unwrap();
        ro.method = MountMethod::Composefs;
        let e = ro.ensure_artifact(&rec.digest).unwrap_err();
        assert!(e.next.contains("podbag pull"), "{e:?}");
        assert!(!ro.erofs_path(&rec.digest).exists());
    }

    #[test]
    fn garbage_collection_removes_unfinished_builds() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let dir = s.image_dir(&rec.digest);
        std::fs::write(dir.join("rootfs.erofs.77.tmp"), b"half").unwrap();
        std::fs::create_dir(dir.join("rootfs.77.tmp")).unwrap();
        std::fs::create_dir_all(s.root.join("tmp/probe.77")).unwrap();
        s.collect_garbage().unwrap();
        assert!(!dir.join("rootfs.erofs.77.tmp").exists());
        assert!(!dir.join("rootfs.77.tmp").exists());
        assert!(!s.root.join("tmp/probe.77").exists());
        assert!(s.rootfs_path(&rec.digest).join("x").exists());
    }

    #[test]
    fn a_hardlink_image_root_is_its_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        assert_eq!(
            s.image_root(&rec.digest).unwrap(),
            ImageRoot::Tree(s.rootfs_path(&rec.digest))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn removing_an_image_removes_its_mount_point() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            data: tmp.path().join("data"),
            runtime: tmp.path().join("run"),
            config: tmp.path().join("cfg"),
        };
        let s = Store::open_at_dirs_for_test(&dirs).unwrap();
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let point = dirs.runtime.join("images").join(hex_of(&rec.digest));
        std::fs::create_dir_all(&point).unwrap();
        std::fs::write(point.with_extension("lock"), b"").unwrap();
        s.remove_image("a", &HashMap::new).unwrap();
        assert!(!point.exists());
        assert!(!point.with_extension("lock").exists());
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
    fn tagging_adds_a_name_and_drops_the_image_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let a = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let b = s
            .import_for_test("b", vec![file_layer(&[("y", b"2")])], 2)
            .unwrap();
        let name = s.tag("a", "mine:1", &HashMap::new).unwrap();
        assert_eq!(name, "docker.io/library/mine:1");
        assert_eq!(s.find("mine:1").unwrap().1.digest, a.digest);
        // b loses its only name, so it goes.
        s.tag("a", "b", &HashMap::new).unwrap();
        assert_eq!(s.find("b").unwrap().1.digest, a.digest);
        assert!(!s.image_dir(&b.digest).exists());
        assert_eq!(s.objects.total_size().unwrap().0, 1, "b's object is swept");
    }

    #[test]
    fn a_replaced_image_that_a_container_uses_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let b = s
            .import_for_test("b", vec![file_layer(&[("y", b"2")])], 2)
            .unwrap();
        let in_use = || HashMap::from([(b.digest.clone(), "c".repeat(64))]);
        s.tag("a", "b", &in_use).unwrap();
        assert!(s.image_dir(&b.digest).join("image.json").exists());
    }

    #[test]
    fn a_name_with_a_digest_is_not_given_to_an_image() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let a = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 1)
            .unwrap();
        let e = s
            .tag("a", &format!("x@{}", a.digest), &HashMap::new)
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert!(s.find("x").is_err());
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
