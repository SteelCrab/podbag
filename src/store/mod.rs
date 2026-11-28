//! The image store: content objects, layer indexes, image trees and names.

#[cfg(unix)]
mod build;
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
#[cfg(unix)]
pub use build::{Committed, SharedLock, copy_all};
pub mod zstd;

use crate::config::{DEFAULT_COMPRESS_IDLE_AFTER, ImageCompression, ImageMount};
use crate::image::manifest::{self, Compression};
use crate::image::reference::Reference;
use crate::image::registry::{Registry, digest_mismatch};
use crate::image::{DiskUsage, HistoryItem, ImageConfig, PruneReport, VerifyReport};
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use layer::{EntryKind, LayerIndex};
#[cfg(target_os = "linux")]
pub(crate) use load::DISK_RESERVE;
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<manifest::HistoryEntry>,
    /// Bytes of file content in the image.
    pub size: u64,
    /// Seconds since the epoch.
    pub created: u64,
    /// It came in OCI's format (else Docker's schema 2): a push keeps the format.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub oci: bool,
}

impl ImageRecord {
    /// Pad older images that have no recorded history for some or all of their layers.
    pub(crate) fn history_with_layers(&self) -> Vec<manifest::HistoryEntry> {
        let recorded = self
            .history
            .iter()
            .filter(|entry| !entry.empty_layer)
            .count();
        let missing = self.diff_ids.len().saturating_sub(recorded);
        let mut history = vec![manifest::HistoryEntry::default(); missing];
        history.extend_from_slice(&self.history);
        if self.history.is_empty() {
            if let Some(last) = history.last_mut() {
                last.created = Some(crate::client::time::rfc3339(self.created, 0));
            }
        }
        history
    }
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
    /// Whether commands compress idle images, and after how many idle seconds.
    compression: (ImageCompression, u64),
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

/// Which images `freeze_images` compresses. Images a container uses and the image to keep
/// are never among them.
pub enum FreezeSelect {
    /// The images not used for `after` seconds before `now` (seconds since the epoch).
    Idle { now: u64, after: u64 },
    /// Every image, least recently used first.
    Evictable,
    /// These images, by digest, however recently used.
    Images(Vec<String>),
}

/// How much one `freeze_images` call does at most: plain bytes compressed, and time.
/// `largest`: objects with more plain bytes stay plain. One object is compressed in one
/// go under the exclusive lock, so a large one would hold up pulls and container starts.
#[derive(Clone, Copy, Debug)]
pub struct FreezeBudget {
    pub bytes: u64,
    pub time: std::time::Duration,
    pub largest: u64,
}

impl FreezeBudget {
    /// For the freeze a command does on its own: short enough not to be noticed.
    pub const AUTO: FreezeBudget = FreezeBudget {
        bytes: 256 << 20,
        time: std::time::Duration::from_secs(2),
        // About a quarter of a second of compression: `system compress` does larger ones.
        largest: 32 << 20,
    };
    pub const UNLIMITED: FreezeBudget = FreezeBudget {
        bytes: u64::MAX,
        time: std::time::Duration::MAX,
        largest: u64::MAX,
    };
}

/// What `freeze_images` did. `finished`: nothing is left to compress in the images it
/// chose; false when the budget ran out first.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct FreezeReport {
    /// Images whose artifact was removed.
    pub images: u64,
    /// Objects compressed.
    pub objects: u64,
    /// Their plain size, and the size of their compressed files.
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub finished: bool,
}

/// How much disk an image takes with its compressed objects, and how much without them.
/// Every file of the image counts, also files it shares with other images.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageCompressionStats {
    /// Bytes on disk: the compressed file of a compressed object, the plain file otherwise.
    pub stored: u64,
    /// The plain size of every file.
    pub original: u64,
}

/// In an image directory: the image's compression numbers, and the objects' cold
/// generation they were counted at.
const COMPRESSION_CACHE: &str = "compression.json";

/// `{"generation": …, "stored": …, "original": …}`, or `{"generation": …, "none": true}`
/// for an image without compressed objects.
#[derive(Serialize, Deserialize)]
struct CompressionCache {
    generation: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    none: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stored: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original: Option<u64>,
}

impl CompressionCache {
    fn new(generation: String, stats: Option<ImageCompressionStats>) -> CompressionCache {
        CompressionCache {
            generation,
            none: stats.is_none(),
            stored: stats.map(|s| s.stored),
            original: stats.map(|s| s.original),
        }
    }

    /// What `compression_of` returns; `None` for a cache that says neither.
    fn stats(&self) -> Option<Option<ImageCompressionStats>> {
        match (self.none, self.stored, self.original) {
            (true, _, _) => Some(None),
            (false, Some(stored), Some(original)) => {
                Some(Some(ImageCompressionStats { stored, original }))
            }
            _ => None,
        }
    }
}

/// Gives the objects' cold generation a new value when dropped, if `.1` is set: some
/// object changed between plain and compressed.
struct NewGenerationOnDrop<'a>(&'a ObjectStore, bool);

impl Drop for NewGenerationOnDrop<'_> {
    fn drop(&mut self) {
        if self.1 {
            self.0.new_cold_generation();
        }
    }
}

/// One freeze batch holds the exclusive lock (which keeps out pulls and container starts)
/// for at most this many plain bytes or this long.
const FREEZE_BATCH_BYTES: u64 = 64 << 20;
const FREEZE_BATCH_TIME: std::time::Duration = std::time::Duration::from_secs(1);

/// In an image directory: every object of the image was handled by a freeze since its
/// artifact was last removed. A freeze skips such an image without an artifact, and
/// removes the file when it removes the image's artifact.
const FROZEN: &str = "frozen";

/// Why a freeze batch stops before the end of its work.
enum FreezeCut {
    /// The call's budget is spent.
    Budget,
    /// The batch is full: the lock is let go, and the next batch goes on.
    Batch,
}

/// What one `freeze_images` call carries from batch to batch.
struct FreezeRun {
    start: std::time::Instant,
    budget: FreezeBudget,
    report: FreezeReport,
    /// Objects this call already looked at: frozen, found compressed, or left plain.
    tried: HashSet<String>,
    /// Objects left plain because a tree still links them (one a freeze is deleting, or
    /// one a crash left), or because they are larger than the budget's `largest`. The
    /// images that use them are not marked frozen.
    linked: HashSet<String>,
    /// Object keys per layer and per image, each once: neither ever changes.
    layer_keys: HashMap<String, Vec<String>>,
    image_keys: HashMap<String, Vec<String>>,
}

impl FreezeRun {
    /// Whether the freeze stops before its next step. `batch` is when the batch took the
    /// lock, `batch_bytes` what it compressed, `steps` the work it did that the next
    /// batch need not do again: a batch is cut only after some, so the call always ends.
    fn cut(&self, batch: std::time::Instant, batch_bytes: u64, steps: u64) -> Option<FreezeCut> {
        if self.report.bytes_before >= self.budget.bytes || self.start.elapsed() >= self.budget.time
        {
            Some(FreezeCut::Budget)
        } else if steps > 0
            && (batch_bytes >= FREEZE_BATCH_BYTES || batch.elapsed() >= FREEZE_BATCH_TIME)
        {
            Some(FreezeCut::Batch)
        } else {
            None
        }
    }
}

/// A command's own freeze is best effort: it reports a failure and the command goes on.
fn warn_unfrozen(result: Result<()>) {
    if let Err(e) = result {
        eprintln!("podbag: warning: cannot compress idle images: {e}");
    }
}

/// True when the file has more than one name (a tree links it).
fn is_linked(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).is_ok_and(|m| m.nlink() > 1)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
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
            compression: (ImageCompression::Off, DEFAULT_COMPRESS_IDLE_AFTER),
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
            compression: (ImageCompression::Off, DEFAULT_COMPRESS_IDLE_AFTER),
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

    /// Choose whether this command compresses images idle for `idle_after` seconds when
    /// it looks after the store (`maintain`, and after removals).
    pub fn use_compression(&mut self, mode: ImageCompression, idle_after: u64) {
        self.compression = (mode, idle_after);
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
        // An image with an artifact has only plain objects, and building never opens them.
        self.thaw_tree(tree)?;
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

    /// Make the plain file of every compressed object the tree names: a tree links plain
    /// files, and EROFS metadata names them.
    #[cfg(unix)]
    fn thaw_tree(&self, tree: &tree::Tree) -> Result<()> {
        // Dropped on every return: objects thawed before a failure changed state too.
        let mut thawed = NewGenerationOnDrop(&self.objects, false);
        for e in tree.entries.values() {
            if let EntryKind::File { object, .. } = &e.kind
                && self.objects.is_cold(object)
            {
                if let Err(err) = self.objects.thaw(object, &tree::meta_of(e)) {
                    // The plain file may be there already (the compressed one stayed).
                    thawed.1 |= self.objects.path(object).exists();
                    return Err(err);
                }
                thawed.1 = true;
            }
        }
        // The plain files reach the disk before the artifact that names them.
        if thawed.1 {
            self.objects.sync()?;
        }
        Ok(())
    }

    /// Only unix builds image trees; nothing is frozen elsewhere.
    #[cfg(not(unix))]
    fn thaw_tree(&self, _tree: &tree::Tree) -> Result<()> {
        Ok(())
    }

    /// Make the plain file of every compressed object of these layers' merged tree, under
    /// the shared lock: a build reads them by path (`COPY --chown`, `COPY --from`).
    #[cfg(unix)]
    pub fn thaw_layers(&self, diff_ids: &[String]) -> Result<()> {
        let _shared = self.lock(false)?;
        self.thaw_tree(&self.tree_of(diff_ids)?)
    }

    /// True when the image has what a container of it is built from (see `artifact`).
    pub fn has_artifact(&self, digest: &str) -> bool {
        self.image_dir(digest)
            .join(self.artifact(digest).name())
            .exists()
    }

    /// How much the image's compressed objects save: `None` when it has none. An image
    /// with an artifact has none (all its objects are plain), so only those without one
    /// are looked at. The numbers are kept in the image's `compression.json` with the
    /// objects' cold generation, and counted again only when that changed. It takes no
    /// lock: an object that changes while it is counted changes the generation after.
    pub fn compression_of(&self, digest: &str) -> Result<Option<ImageCompressionStats>> {
        if self.has_any_artifact(digest) {
            return Ok(None);
        }
        // Read before counting: a change during the count leaves a cache that is not used.
        let generation = self.objects.cold_generation();
        let path = self.image_dir(digest).join(COMPRESSION_CACHE);
        if let Ok(cache) = read_json::<CompressionCache>(&path)
            && cache.generation == generation
            && let Some(stats) = cache.stats()
        {
            return Ok(stats);
        }
        let stats = self.count_compression(digest)?;
        // Best effort: a store this process cannot write counts again next time.
        let _ = write_json(&path, &CompressionCache::new(generation, stats));
        Ok(stats)
    }

    /// `compression_of` without its cache: read every object of the image.
    fn count_compression(&self, digest: &str) -> Result<Option<ImageCompressionStats>> {
        let mut seen = HashSet::new();
        let mut stats = ImageCompressionStats {
            stored: 0,
            original: 0,
        };
        let mut compressed = false;
        for id in &self.read_record(digest)?.diff_ids {
            for entry in self.read_layer_index(id)?.entries {
                let EntryKind::File { object, size } = entry.kind else {
                    continue;
                };
                if !seen.insert(object.clone()) {
                    continue;
                }
                match self.cold_size(&object)? {
                    Some((stored, original)) => {
                        compressed = true;
                        stats.stored += stored;
                        stats.original += original;
                    }
                    None => {
                        stats.stored += size;
                        stats.original += size;
                    }
                }
            }
        }
        Ok(compressed.then_some(stats))
    }

    /// The size on disk and the plain size of a compressed object; `None` when the object
    /// is plain. A plain file counts when it is there, also beside a compressed file.
    fn cold_size(&self, key: &str) -> Result<Option<(u64, u64)>> {
        if self.objects.path(key).exists() {
            return Ok(None);
        }
        let cold = self.objects.cold_path(key);
        let mut file = match File::open(&cold) {
            Ok(f) => f,
            // Thawed since the check above (a thaw needs only the shared lock).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error(&cold, e)),
        };
        let stored = file.metadata().map_err(|e| io_error(&cold, e))?.len();
        let header = zstd::read_cold_header(&mut file).map_err(|e| io_error(&cold, e))?;
        Ok(Some((stored, header.size)))
    }

    /// Record that the image is in use now: create `last_used` in its directory, or set
    /// its time to now. Idle images are the ones whose objects may be compressed.
    pub fn note_use(&self, digest: &str) -> Result<()> {
        let path = self.image_dir(digest).join("last_used");
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()))
            .map_err(|e| io_error(&path, e))
    }

    /// When the image was last used, in seconds since the epoch: the latest of its
    /// `last_used` file, the use times of the names that point at it, and its pull time
    /// (`image.json`). 0 when nothing is known.
    fn last_use(&self, digest: &str, refs: &Refs) -> u64 {
        let dir = self.image_dir(digest);
        let file_time = |name: &str| {
            std::fs::metadata(dir.join(name))
                .ok()
                .and_then(|m| object::mtime_of(&m))
        };
        let named = refs
            .values()
            .filter(|r| r.digest == digest)
            .map(|r| r.last_used);
        file_time("last_used")
            .into_iter()
            .chain(file_time("image.json"))
            .chain(named)
            .max()
            .unwrap_or(0)
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
        let usage = self.objects.usage()?;
        let containers = std::fs::read_dir(dirs.data.join("containers"))
            .map(|d| d.count() as u64)
            .unwrap_or(0);
        let cold = usage.cold;
        Ok(DiskUsage {
            images,
            objects: usage.objects,
            object_bytes: usage.bytes,
            containers,
            compressed_objects: cold.objects,
            compressed_bytes: cold.bytes,
            original_bytes: cold.original_bytes,
        })
    }

    /// Download `name` (or reuse what is stored), build its tree, and record it.
    /// Then look after the store (`maintain`): freeze idle images when compression is on,
    /// and remove least recently used images until the store fits in `limit`.
    ///
    /// The registry is asked without any lock. Storing happens under the shared lock, so
    /// container starts and other pulls go on. Only freezing and eviction take the
    /// exclusive lock. `in_use` is read after that lock is taken.
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
        self.maintain(limit, in_use, &digest)?;
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
            // Pulled before, maybe before history was stored or with another mount method.
            self.remember_history(&resolved.manifest_digest, resolved.config.history.clone())?;
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
            ImageRecord {
                digest: resolved.manifest_digest.clone(),
                config_digest: Some(resolved.config_digest.clone()),
                config: resolved.config.config.clone(),
                diff_ids: resolved.config.diff_ids.clone(),
                history: resolved.config.history.clone(),
                created: now(),
                size: 0,
                oci: resolved.oci,
            },
            true,
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
        self.write_layer_index(diff_id, entries)?;
        Ok(compressed.count)
    }

    fn write_layer_index(&self, diff_id: &str, entries: Vec<layer::Entry>) -> Result<()> {
        // The objects reach the disk before the index that names them.
        self.objects.sync()?;
        write_json(
            &self.layer_path(diff_id),
            &LayerIndex {
                diff_id: diff_id.to_string(),
                entries,
            },
        )
    }

    /// Merge stored layers into the image tree and write the image record.
    /// Without `artifact`, only the record: [`Store::ensure_artifact`] builds the rest
    /// when a container needs it (build steps that no `RUN` follows).
    fn assemble(&self, mut record: ImageRecord, artifact: bool) -> Result<()> {
        let mut layers = Vec::new();
        for id in &record.diff_ids {
            layers.push(self.read_layer_index(id)?.entries);
        }
        let tree = tree::merge(&layers)?;
        let mut seen = HashSet::new();
        record.size = tree
            .entries
            .values()
            .filter_map(|e| match &e.kind {
                EntryKind::File { object, size } if seen.insert(object.clone()) => Some(*size),
                _ => None,
            })
            .sum();

        let digest = &record.digest;
        let dir = self.image_dir(digest);
        // Unique per process: two pulls of the same image may assemble at the same time.
        let staging =
            self.root
                .join("images")
                .join(format!("{}.{}.tmp", hex_of(digest), std::process::id()));
        let _ = remove_tree(&staging);
        std::fs::create_dir(&staging).map_err(|e| io_error(&staging, e))?;
        let result = (|| {
            let want = match self.method {
                MountMethod::Composefs => Artifact::Erofs,
                MountMethod::Hardlink => Artifact::Tree,
            };
            if artifact {
                self.build_in(&staging, want, &tree, record.created)?;
            }
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
    /// Returns the names removed. With compression on, idle images are frozen after.
    pub fn remove_image(
        &self,
        name: &str,
        in_use: &dyn Fn() -> HashMap<String, String>,
    ) -> Result<Vec<String>> {
        let removed = self.remove_image_locked(name, in_use)?;
        self.freeze_idle(in_use, "");
        Ok(removed)
    }

    fn remove_image_locked(
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

    /// Remove the images without a name that no container uses (`docker image prune`).
    /// With compression on, idle images are frozen after.
    pub fn prune_dangling(
        &self,
        in_use: &dyn Fn() -> HashMap<String, String>,
    ) -> Result<PruneReport> {
        let report = self.prune_dangling_locked(in_use)?;
        self.freeze_idle(in_use, "");
        Ok(report)
    }

    fn prune_dangling_locked(
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

    /// Remove every image that no container uses. With compression on, the idle images
    /// left are frozen after.
    pub fn prune(&self, in_use: &dyn Fn() -> HashMap<String, String>) -> Result<PruneReport> {
        let report = self.prune_locked(in_use)?;
        self.freeze_idle(in_use, "");
        Ok(report)
    }

    fn prune_locked(&self, in_use: &dyn Fn() -> HashMap<String, String>) -> Result<PruneReport> {
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
        // A damaged compressed file beside a sound plain file is a stale copy (an insert
        // made the object plain again and stopped before it deleted the copy): images
        // read the plain file, so the copy breaks none of them.
        let stale: Vec<&str> = damaged
            .iter()
            .map(String::as_str)
            .filter(|name| {
                name.strip_suffix(".zst").is_some_and(|key| {
                    self.objects.path(key).exists() && !self.objects.is_damaged(key).unwrap_or(true)
                })
            })
            .collect();
        // Any other damaged compressed file belongs to its object.
        let bad: HashSet<&str> = damaged
            .iter()
            .filter(|name| !stale.contains(&name.as_str()))
            .map(|name| name.strip_suffix(".zst").unwrap_or(name))
            .collect();
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
            for name in &stale {
                let path = self.objects.path(name);
                match std::fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(io_error(&path, e));
                    }
                    _ => {}
                }
            }
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

    /// What a command does to the store after it adds an image: with compression on,
    /// freeze the idle images, and, when the store is still over `limit`, the others least
    /// recently used first; then remove least recently used images until the store fits.
    /// Images in `in_use` and the image `keep` stay. Returns the names removed. Freezing
    /// is best effort (a failure is a warning); eviction errors are returned.
    /// Call it without holding a store lock: freezing takes and lets go of it per batch.
    pub fn maintain(
        &self,
        limit: u64,
        in_use: &dyn Fn() -> HashMap<String, String>,
        keep: &str,
    ) -> Result<Vec<String>> {
        self.freeze_to_fit(limit, in_use, keep);
        let _lock = self.lock(true)?;
        self.enforce_limit_locked(limit, &in_use(), keep)
    }

    /// With compression on: freeze the idle images, then, when the store is over `limit`,
    /// the others least recently used first. Best effort: a failure is a warning. Call it
    /// without holding a store lock.
    fn freeze_to_fit(&self, limit: u64, in_use: &dyn Fn() -> HashMap<String, String>, keep: &str) {
        let (mode, after) = self.compression;
        if mode == ImageCompression::Off {
            return;
        }
        let idle = FreezeSelect::Idle { now: now(), after };
        let frozen = self
            .freeze_images(&idle, in_use, keep, FreezeBudget::AUTO)
            .and_then(|_| {
                let size = {
                    let _shared = self.lock(false)?;
                    self.objects.total_size()?.1
                };
                if size > limit {
                    let select = FreezeSelect::Evictable;
                    self.freeze_images(&select, in_use, keep, FreezeBudget::AUTO)?;
                }
                Ok(())
            });
        warn_unfrozen(frozen);
    }

    /// With compression on, freeze the images idle for the chosen time, within the
    /// budget of a command's own freeze. Best effort: a failure is a warning. Call it
    /// without holding a store lock.
    fn freeze_idle(&self, in_use: &dyn Fn() -> HashMap<String, String>, keep: &str) {
        let (mode, after) = self.compression;
        if mode == ImageCompression::Off {
            return;
        }
        let idle = FreezeSelect::Idle { now: now(), after };
        warn_unfrozen(
            self.freeze_images(&idle, in_use, keep, FreezeBudget::AUTO)
                .map(drop),
        );
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

    /// Compress the objects of the images `select` names, except those a container uses
    /// (`in_use`, read under the exclusive lock) and `keep`. An image with an artifact has
    /// only plain objects, so each image loses its artifacts first (its next use builds
    /// them again), and an object that an image with an artifact uses stays plain.
    ///
    /// The work goes in batches: each holds the exclusive lock for at most
    /// `FREEZE_BATCH_BYTES` or `FREEZE_BATCH_TIME`, so pulls and container starts wait
    /// little. The call stops when nothing is left (`finished`) or `budget` is spent.
    pub fn freeze_images(
        &self,
        select: &FreezeSelect,
        in_use: &dyn Fn() -> HashMap<String, String>,
        keep: &str,
        budget: FreezeBudget,
    ) -> Result<FreezeReport> {
        let mut run = FreezeRun {
            start: std::time::Instant::now(),
            budget,
            report: FreezeReport::default(),
            tried: HashSet::new(),
            linked: HashSet::new(),
            layer_keys: HashMap::new(),
            image_keys: HashMap::new(),
        };
        loop {
            let (cut, doomed) = self.freeze_batch(select, in_use, keep, &mut run)?;
            // Removed artifacts go without the lock: they were renamed, the renames are
            // on disk, and garbage collection deletes what a crash leaves.
            for path in doomed {
                let _ = remove_path(&path);
            }
            match cut {
                None => {
                    run.report.finished = true;
                    return Ok(run.report);
                }
                Some(FreezeCut::Budget) => return Ok(run.report),
                Some(FreezeCut::Batch) => {}
            }
        }
    }

    /// One batch of `freeze_images`, under the exclusive lock. Returns why it stopped
    /// (`None`: nothing is left) and the renamed artifacts to delete once the lock is let go.
    fn freeze_batch(
        &self,
        select: &FreezeSelect,
        in_use: &dyn Fn() -> HashMap<String, String>,
        keep: &str,
        run: &mut FreezeRun,
    ) -> Result<(Option<FreezeCut>, Vec<PathBuf>)> {
        let _lock = self.lock(true)?;
        // Dropped before the lock, also when the batch fails half-way.
        let mut changed = NewGenerationOnDrop(&self.objects, false);
        // Everything done under the lock counts, the reading too.
        let batch = std::time::Instant::now();
        // Read again for every batch: a container may have started in between.
        let in_use = in_use();
        let refs = self.read_refs()?;
        let images = self.image_digests()?;
        let pending: Vec<(String, bool)> = self
            .freeze_candidates(select, &images, &refs, &in_use, keep)
            .into_iter()
            .filter_map(|d| {
                let artifact = self.has_any_artifact(&d);
                (artifact || !self.image_dir(&d).join(FROZEN).exists()).then_some((d, artifact))
            })
            .collect();
        if pending.is_empty() {
            return Ok((None, Vec::new()));
        }
        // The objects of warm images stay plain: images with an artifact, and, to be safe,
        // those a container uses and `keep`, which is about to run. Each object counts the
        // warm images that use it.
        let mut warm: HashMap<String, usize> = HashMap::new();
        for digest in &images {
            if self.has_any_artifact(digest) || in_use.contains_key(digest) || digest == keep {
                for key in self.image_keys(digest, run)? {
                    *warm.entry(key).or_default() += 1;
                }
            }
        }
        let (mut steps, mut batch_bytes) = (0, 0);
        let mut cut = None;
        let mut doomed = Vec::new();
        let mut done = Vec::new();
        'images: for (digest, artifact) in &pending {
            if let Some(c) = run.cut(batch, batch_bytes, steps) {
                cut = Some(c);
                break;
            }
            if *artifact {
                // Under a size limit, an image whose objects all stay plain keeps its
                // artifact: removing it saves nothing, and its next use builds it again.
                if run.budget.largest < u64::MAX
                    && !self.has_freezable_object(digest, &warm, run)?
                {
                    continue;
                }
                doomed = self.remove_artifacts(digest)?;
                run.report.images += 1;
                // A tree links the objects until it is deleted, after the lock is let go:
                // the next batch compresses them.
                cut = Some(FreezeCut::Batch);
                break;
            }
            let mut whole = true;
            for key in self.image_keys(digest, run)? {
                if let Some(c) = run.cut(batch, batch_bytes, steps) {
                    cut = Some(c);
                    break 'images;
                }
                if run.linked.contains(&key) {
                    whole = false;
                    continue;
                }
                if warm.contains_key(&key) {
                    continue;
                }
                if run.tried.contains(&key) {
                    // An earlier batch compressed it or left it plain, but a build may
                    // have made it plain since: the image is whole only while the object
                    // has no plain file a freeze would compress.
                    let path = self.objects.path(&key);
                    if std::fs::metadata(&path).is_ok_and(|m| m.len() >= object::MIN_COLD_SIZE) {
                        whole = false;
                    }
                    continue;
                }
                steps += 1;
                let path = self.objects.path(&key);
                let size = std::fs::metadata(&path).map(|m| m.len());
                if size.as_ref().is_ok_and(|n| *n > run.budget.largest) {
                    // Like a linked object: the image is not marked, a later freeze with a
                    // larger limit (`system compress`) compresses it.
                    run.linked.insert(key);
                    whole = false;
                    continue;
                }
                if size.is_ok() {
                    let (plain, packed) = match self.objects.freeze(&key) {
                        Ok(sizes) => sizes,
                        Err(e) => {
                            // The plain name may be gone already (a time copy that could
                            // not be deleted): then the object changed state.
                            changed.1 |= self.objects.is_cold(&key);
                            return Err(e);
                        }
                    };
                    if plain > 0 {
                        changed.1 = true;
                        run.report.objects += 1;
                        run.report.bytes_before += plain;
                        run.report.bytes_after += packed;
                        batch_bytes += plain;
                    } else if is_linked(&path) {
                        run.linked.insert(key.clone());
                        whole = false;
                    }
                }
                run.tried.insert(key);
            }
            if whole {
                done.push(digest);
                steps += 1;
            }
        }
        self.objects.sync()?;
        drop(changed);
        // Marked only once its compressed objects are on disk.
        for digest in done {
            let path = self.image_dir(digest).join(FROZEN);
            File::create(&path).map_err(|e| io_error(&path, e))?;
        }
        Ok((cut, doomed))
    }

    /// True when the image (which has an artifact) has an object a later batch could
    /// compress within the budget's `largest`: one no other warm image uses, that has a
    /// plain file no larger than that and not smaller than a block (`freeze` leaves
    /// smaller ones plain).
    fn has_freezable_object(
        &self,
        digest: &str,
        warm: &HashMap<String, usize>,
        run: &mut FreezeRun,
    ) -> Result<bool> {
        let largest = run.budget.largest;
        Ok(self.image_keys(digest, run)?.iter().any(|key| {
            // The image counts itself among the warm ones: its artifact makes it warm.
            warm.get(key).is_none_or(|n| *n <= 1)
                && std::fs::metadata(self.objects.path(key))
                    .is_ok_and(|m| (object::MIN_COLD_SIZE..=largest).contains(&m.len()))
        }))
    }

    /// The images `select` names that may be frozen, in the order to freeze them.
    fn freeze_candidates(
        &self,
        select: &FreezeSelect,
        images: &[String],
        refs: &Refs,
        in_use: &HashMap<String, String>,
        keep: &str,
    ) -> Vec<String> {
        let free = |d: &&String| d.as_str() != keep && !in_use.contains_key(*d);
        match select {
            FreezeSelect::Idle { now, after } => images
                .iter()
                .filter(free)
                .filter(|d| now.saturating_sub(self.last_use(d, refs)) >= *after)
                .cloned()
                .collect(),
            FreezeSelect::Evictable => {
                let mut by_age: Vec<(u64, &String)> = images
                    .iter()
                    .filter(free)
                    .map(|d| (self.last_use(d, refs), d))
                    .collect();
                by_age.sort();
                by_age.into_iter().map(|(_, d)| d.clone()).collect()
            }
            FreezeSelect::Images(wanted) => {
                let mut seen = HashSet::new();
                wanted
                    .iter()
                    .filter(|d| images.contains(d) && seen.insert(d.as_str()))
                    .filter(free)
                    .cloned()
                    .collect()
            }
        }
    }

    /// True when the image has the artifact of either method: one left from another
    /// method is used again when the method changes back, so its objects must stay plain.
    fn has_any_artifact(&self, digest: &str) -> bool {
        let dir = self.image_dir(digest);
        [Artifact::Erofs, Artifact::Tree]
            .iter()
            .any(|a| dir.join(a.name()).exists())
    }

    /// Unmount the image, drop its `frozen` mark and rename its artifacts to
    /// `<name>.<pid>.freeze.tmp`, for the caller to delete. The renames reach the disk
    /// before anything else happens: after a crash the image has its whole artifact or
    /// none (it is then built again), never one that names compressed objects. Garbage
    /// collection deletes renamed artifacts a crash leaves.
    fn remove_artifacts(&self, digest: &str) -> Result<Vec<PathBuf>> {
        self.unmount_image(digest)?;
        let dir = self.image_dir(digest);
        let mark = dir.join(FROZEN);
        match std::fs::remove_file(&mark) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io_error(&mark, e)),
            _ => {}
        }
        let mut doomed = Vec::new();
        for artifact in [Artifact::Erofs, Artifact::Tree] {
            let path = dir.join(artifact.name());
            if !path.exists() {
                continue;
            }
            let tmp = dir.join(format!(
                "{}.{}.freeze.tmp",
                artifact.name(),
                std::process::id()
            ));
            // A leftover of an earlier process with this pid would stop the rename.
            remove_path(&tmp).map_err(|e| io_error(&tmp, e))?;
            std::fs::rename(&path, &tmp).map_err(|e| io_error(&path, e))?;
            doomed.push(tmp);
        }
        File::open(&dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_error(&dir, e))?;
        Ok(doomed)
    }

    /// The object keys of the files in the image's layers, each once, kept in `run`.
    fn image_keys(&self, digest: &str, run: &mut FreezeRun) -> Result<Vec<String>> {
        if let Some(keys) = run.image_keys.get(digest) {
            return Ok(keys.clone());
        }
        let diff_ids = self.read_record(digest)?.diff_ids;
        for id in &diff_ids {
            if !run.layer_keys.contains_key(id) {
                let objects = self
                    .read_layer_index(id)?
                    .entries
                    .into_iter()
                    .filter_map(|e| match e.kind {
                        EntryKind::File { object, .. } => Some(object),
                        _ => None,
                    })
                    .collect();
                run.layer_keys.insert(id.clone(), objects);
            }
        }
        let mut seen = HashSet::new();
        let keys: Vec<String> = diff_ids
            .iter()
            .flat_map(|id| &run.layer_keys[id])
            .filter(|key| seen.insert(key.as_str()))
            .cloned()
            .collect();
        run.image_keys.insert(digest.to_string(), keys.clone());
        Ok(keys)
    }

    /// Take down the image's composefs mount, if it has one.
    fn unmount_image(&self, _digest: &str) -> Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(point) = self.mount_point(_digest) {
            composefs::unmount_shared(&point).map_err(|e| {
                Error::io(
                    format!("cannot unmount {}", point.display()),
                    &e,
                    "unmount it by hand (`umount -l`), then try again",
                )
            })?;
        }
        Ok(())
    }

    fn delete_image(&self, digest: &str) -> Result<()> {
        self.unmount_image(digest)?;
        let dir = self.image_dir(digest);
        remove_tree(&dir).map_err(|e| io_error(&dir, e))
    }

    /// Delete layer indexes no image uses, then objects no layer index uses. Every
    /// `frozen` mark goes too.
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
                // An image that went may have thawed objects a frozen image uses: the
                // next freeze looks at every image again.
                let _ = std::fs::remove_file(dir.path().join(FROZEN));
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
        #[cfg(unix)]
        self.sweep_build_cache();
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

    /// Present usable layer sizes even for images stored without historical commands.
    pub fn image_history(&self, record: &ImageRecord) -> Result<Vec<HistoryItem>> {
        let names = self
            .read_refs()?
            .into_iter()
            .filter(|(name, entry)| entry.digest == record.digest && !name.contains('@'))
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        let mut ids = record.diff_ids.iter();
        let mut rows = Vec::new();
        for entry in record.history_with_layers() {
            let size = match if entry.empty_layer { None } else { ids.next() } {
                Some(id) => self
                    .read_layer_index(id)?
                    .entries
                    .iter()
                    .map(|entry| match entry.kind {
                        EntryKind::File { size, .. } => size,
                        _ => 0,
                    })
                    .sum(),
                None => 0,
            };
            rows.push(HistoryItem {
                id: "<missing>".to_string(),
                created: entry
                    .created
                    .as_deref()
                    .and_then(crate::client::time::epoch_of)
                    .map_or(0, |time| time.floor() as i64),
                created_by: entry.created_by,
                tags: Vec::new(),
                size,
                comment: entry.comment,
            });
        }
        if let Some(top) = rows.last_mut() {
            top.id = record.digest.clone();
            top.tags = names;
        }
        rows.reverse();
        Ok(rows)
    }

    fn remember_history(&self, digest: &str, history: Vec<manifest::HistoryEntry>) -> Result<()> {
        if !history.is_empty() {
            let mut image = self.read_record(digest)?;
            if image.history.is_empty() {
                image.history = history;
                write_json(&self.image_dir(digest).join("image.json"), &image)?;
            }
        }
        Ok(())
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
            self.assemble(
                ImageRecord {
                    digest: digest.clone(),
                    config_digest: None,
                    config: ImageConfig::default(),
                    diff_ids,
                    history: Vec::new(),
                    created: now(),
                    size: 0,
                    oci: false,
                },
                true,
            )?;
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
    /// A layer a build step made.
    #[cfg_attr(not(unix), allow(dead_code))]
    Build,
}

impl Source<'_> {
    fn failure(self) -> String {
        match self {
            Source::Registry(r) => format!("cannot pull {}", r.name()),
            Source::Archive => "cannot load the image archive".to_string(),
            Source::Build => "cannot store the build step's files".to_string(),
        }
    }

    fn sender(self) -> &'static str {
        match self {
            Source::Registry(_) => "the registry's blob",
            Source::Archive => "the archive's blob",
            Source::Build => "the step's layer",
        }
    }

    fn promise(self) -> &'static str {
        match self {
            Source::Registry(_) => "the manifest",
            Source::Archive => "its record",
            Source::Build => "its writer",
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
            Source::Build => Error::new(
                ErrorKind::Internal,
                "cannot store the build step's files",
                format!("sha256 mismatch for {digest}"),
                "build again; if it keeps failing, report this bug",
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
        let start = std::sync::Barrier::new(3);
        std::thread::scope(|threads| {
            for writer in 0..2 {
                let file = dir.join("work").join(format!("late-{writer}"));
                let start = &start;
                threads.spawn(move || {
                    std::fs::write(&file, b"").unwrap();
                    start.wait();
                    let end = std::time::Instant::now() + std::time::Duration::from_millis(200);
                    while std::time::Instant::now() < end {
                        if let Err(err) = std::fs::write(&file, b"") {
                            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
                            break;
                        }
                    }
                });
            }
            start.wait();
            remove_tree(&dir).unwrap();
        });
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

    #[test]
    fn verify_fix_removes_images_with_a_damaged_compressed_object() {
        use std::io::{Seek, SeekFrom, Write};
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[7u8; 8192])])], 1)
            .unwrap();
        remove_tree(&s.rootfs_path(&rec.digest)).unwrap();
        let index = s.read_layer_index(&rec.diff_ids[0]).unwrap();
        let keys: Vec<String> = index
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::File { object, .. } => Some(object.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), 1);
        assert_eq!(s.objects.freeze(&keys[0]).unwrap().0, 8192);
        let mut cold = std::fs::OpenOptions::new()
            .write(true)
            .open(s.objects.cold_path(&keys[0]))
            .unwrap();
        cold.seek(SeekFrom::End(-16)).unwrap();
        cold.write_all(&[0u8; 16]).unwrap();
        drop(cold);
        let report = s.verify(true, &HashMap::new).unwrap();
        assert_eq!(report.damaged, [format!("{}.zst", keys[0])]);
        assert_eq!(report.removed, ["docker.io/library/a:latest"]);
        assert!(s.list().unwrap().is_empty());
        assert!(!s.objects.cold_path(&keys[0]).exists(), "garbage collected");
    }

    /// Overwrite the end of the object's compressed file with zeros.
    fn damage_cold(s: &Store, key: &str) {
        use std::io::{Seek, SeekFrom, Write};
        let mut cold = std::fs::OpenOptions::new()
            .write(true)
            .open(s.objects.cold_path(key))
            .unwrap();
        cold.seek(SeekFrom::End(-16)).unwrap();
        cold.write_all(&[0u8; 16]).unwrap();
    }

    /// A damaged compressed file beside a sound plain file is a stale copy: `--fix`
    /// deletes the copy and keeps the image, which reads the plain file.
    #[cfg(unix)]
    #[test]
    fn verify_fix_removes_only_a_stale_compressed_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[7u8; 8192])])], 1)
            .unwrap();
        remove_tree(&s.rootfs_path(&rec.digest)).unwrap();
        let key = object_keys(&s, &rec).remove(0);
        let contents = std::fs::read(s.objects.path(&key)).unwrap();
        assert_eq!(s.objects.freeze(&key).unwrap().0, 8192);
        damage_cold(&s, &key);
        // An insert made the object plain again and stopped before it deleted the copy.
        std::fs::write(s.objects.path(&key), &contents).unwrap();
        let report = s.verify(true, &HashMap::new).unwrap();
        assert_eq!(report.damaged, [format!("{key}.zst")]);
        assert!(report.images.is_empty(), "{:?}", report.images);
        assert!(report.removed.is_empty(), "{:?}", report.removed);
        assert_eq!(s.list().unwrap().len(), 1);
        assert!(!s.objects.cold_path(&key).exists());
        s.ensure_artifact(&rec.digest).unwrap();
        let f = s.rootfs_path(&rec.digest).join("f");
        assert_eq!(std::fs::read(f).unwrap(), contents);
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
    fn maintain_with_compression_off_evicts_as_before() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("old", vec![file_layer(&[("u", &[1u8; 3000])])], 1)
            .unwrap();
        s.import_for_test("new", vec![file_layer(&[("u", &[2u8; 3000])])], 2)
            .unwrap();
        let removed = s.maintain(4000, &HashMap::new, "").unwrap();
        assert_eq!(removed, ["docker.io/library/old:latest"]);
        let names: Vec<String> = s.list().unwrap().into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(names, ["docker.io/library/new:latest"]);
        assert_eq!(s.objects.total_size().unwrap(), (1, 3000));
        assert_eq!(s.objects.cold_stats().unwrap().objects, 0);
    }

    /// 20 KiB of a 1 KiB pattern: it compresses well.
    fn patterned(seed: u8) -> Vec<u8> {
        let pattern: Vec<u8> = (0..1024u32)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
            .collect();
        pattern.repeat(20)
    }

    /// 20 KiB of pseudo-random bytes (xorshift): it does not compress.
    fn noise(seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..20 * 1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn maintain_freezes_before_it_evicts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        s.use_compression(ImageCompression::Idle, 3600);
        s.import_for_test("old", vec![file_layer(&[("u", &patterned(1))])], 1)
            .unwrap();
        s.import_for_test("new", vec![file_layer(&[("u", &patterned(2))])], 2)
            .unwrap();
        let plain = s.objects.total_size().unwrap().1;
        assert_eq!(plain, 2 * 20 * 1024);
        let limit = plain * 7 / 10;
        let removed = s.maintain(limit, &HashMap::new, "").unwrap();
        assert!(removed.is_empty(), "{removed:?}");
        assert_eq!(s.list().unwrap().len(), 2);
        assert!(s.objects.total_size().unwrap().1 <= limit);
    }

    #[test]
    fn maintain_evicts_when_freezing_is_not_enough() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        s.use_compression(ImageCompression::Idle, 3600);
        s.import_for_test("old", vec![file_layer(&[("u", &noise(1))])], 1)
            .unwrap();
        s.import_for_test("new", vec![file_layer(&[("u", &noise(2))])], 2)
            .unwrap();
        let removed = s.maintain(25_000, &HashMap::new, "").unwrap();
        assert_eq!(removed, ["docker.io/library/old:latest"]);
        let names: Vec<String> = s.list().unwrap().into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(names, ["docker.io/library/new:latest"]);
        assert!(s.objects.total_size().unwrap().1 <= 25_000);
    }

    /// Freezing is best effort: a freeze that fails leaves the command to finish.
    #[cfg(unix)]
    #[test]
    fn maintain_goes_on_when_freezing_fails() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores directory permissions
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        s.use_compression(ImageCompression::Idle, 3600);
        let old = s
            .import_for_test("old", vec![file_layer(&[("u", &patterned(1))])], 1)
            .unwrap();
        age(&s, &old.digest);
        let objects_tmp = s.objects_dir().join("tmp");
        std::fs::set_permissions(&objects_tmp, std::fs::Permissions::from_mode(0o500)).unwrap();
        let maintained = s.maintain(u64::MAX, &HashMap::new, "");
        let pruned = s.prune_dangling(&HashMap::new);
        std::fs::set_permissions(&objects_tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(maintained.unwrap(), Vec::<String>::new());
        assert!(pruned.is_ok());
        assert!(!all_cold(&s, &old));
    }

    /// An image that was idle long enough is frozen after a removal too.
    #[test]
    fn maintain_after_a_removal_freezes_idle_images() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        s.use_compression(ImageCompression::Idle, 3600);
        let old = s
            .import_for_test("old", vec![file_layer(&[("u", &patterned(1))])], 1)
            .unwrap();
        age(&s, &old.digest);
        s.import_for_test("gone", vec![file_layer(&[("u", &patterned(2))])], 2)
            .unwrap();
        s.remove_image("gone", &HashMap::new).unwrap();
        assert!(all_cold(&s, &old));
    }

    /// A key that an image (now removed) thawed is frozen again: garbage collection drops
    /// the `frozen` marks, so the images that use it are looked at again.
    #[cfg(unix)]
    #[test]
    fn an_object_a_removed_image_thawed_is_frozen_again() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let big = patterned(3);
        let a = s
            .import_for_test("a", vec![file_layer(&[("s", &big)])], 1)
            .unwrap();
        age(&s, &a.digest);
        s.freeze_images(&idle(3600), &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert!(s.image_dir(&a.digest).join(FROZEN).exists());
        assert!(all_cold(&s, &a));
        // B shares the object: building it thaws the object.
        s.import_for_test(
            "b",
            vec![file_layer(&[("s", &big), ("t", &patterned(4))])],
            2,
        )
        .unwrap();
        assert!(!all_cold(&s, &a));
        s.remove_image("b", &HashMap::new).unwrap();
        assert!(!s.image_dir(&a.digest).join(FROZEN).exists());
        s.freeze_images(&idle(3600), &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert!(all_cold(&s, &a));
        assert!(s.image_dir(&a.digest).join(FROZEN).exists());
    }

    /// `save` reads a compressed object like a plain one: the archive is the same.
    #[test]
    fn save_of_a_frozen_image_matches_save_of_the_warm_image() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("big", &[9u8; 20_000])])], 1)
            .unwrap();
        let names = ["a".to_string()];
        let mut warm = Vec::new();
        s.save(&names, &mut warm).unwrap();
        s.freeze_images(
            &FreezeSelect::Images(vec![rec.digest.clone()]),
            &HashMap::new,
            "",
            FreezeBudget::UNLIMITED,
        )
        .unwrap();
        assert!(all_cold(&s, &rec));
        let mut frozen = Vec::new();
        s.save(&names, &mut frozen).unwrap();
        assert_eq!(warm, frozen);
    }

    /// `save` checks a compressed object against its key while it reads it: a damaged
    /// copy that still unpacks to the right size stops the save.
    #[test]
    fn save_refuses_a_compressed_object_that_does_not_match_its_key() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let data = noise(3);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &data)])], 1)
            .unwrap();
        let names = ["a".to_string()];
        let mut warm = Vec::new();
        s.save(&names, &mut warm).unwrap();
        s.freeze_images(
            &FreezeSelect::Images(vec![rec.digest.clone()]),
            &HashMap::new,
            "",
            FreezeBudget::UNLIMITED,
        )
        .unwrap();
        assert!(all_cold(&s, &rec));
        let mut frozen = Vec::new();
        s.save(&names, &mut frozen).unwrap();
        assert!(warm == frozen, "the archives differ");
        // Noise does not compress: its frame holds the bytes as they are. Change one.
        let key = object_keys(&s, &rec).remove(0);
        let cold = s.objects.cold_path(&key);
        let mut bytes = std::fs::read(&cold).unwrap();
        let at = bytes.len() - 100;
        bytes[at] ^= 0xff;
        std::fs::write(&cold, &bytes).unwrap();
        let mut out = Vec::new();
        s.objects.open(&key).unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), data.len(), "it still unpacks to the same size");
        assert!(out != data);
        let mut archive = Vec::new();
        let e = s.save(&names, &mut archive).unwrap_err();
        assert!(e.why.contains(&key), "{}", e.why);
        assert!(e.next.contains("podbag system verify --fix"), "{}", e.next);
    }

    /// `disk_usage` reports the compressed objects: how many, their size on disk, and the
    /// size their plain files had.
    #[test]
    fn disk_usage_counts_compressed_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let dirs = Dirs {
            data: tmp.path().join("data"),
            runtime: tmp.path().join("run"),
            config: tmp.path().join("config"),
        };
        let rec = s
            .import_for_test("a", vec![file_layer(&[("big", &[9u8; 20_000])])], 1)
            .unwrap();
        let before = s.disk_usage(&dirs).unwrap();
        assert_eq!(before.compressed_objects, 0);
        s.freeze_images(
            &FreezeSelect::Images(vec![rec.digest.clone()]),
            &HashMap::new,
            "",
            FreezeBudget::UNLIMITED,
        )
        .unwrap();
        let after = s.disk_usage(&dirs).unwrap();
        assert!(after.compressed_objects >= 1);
        assert!(after.original_bytes > after.compressed_bytes);
        assert!(after.object_bytes < before.object_bytes);
    }

    /// `system df`, Docker's `/system/df` and CRI `ImageFsInfo` (every few seconds) read
    /// the objects once: with compression off they cost what they did before it.
    #[test]
    fn disk_usage_walks_the_objects_once() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let dirs = Dirs {
            data: tmp.path().join("data"),
            runtime: tmp.path().join("run"),
            config: tmp.path().join("config"),
        };
        s.import_for_test("a", vec![file_layer(&[("big", &[9u8; 20_000])])], 1)
            .unwrap();
        let walks = || object::WALKS.with(std::cell::Cell::get);
        let before = walks();
        s.disk_usage(&dirs).unwrap();
        assert_eq!(walks() - before, 1);
    }

    /// A build that makes an image it already has (the build cache) counts as a use, so
    /// an image built every day is never frozen.
    #[cfg(unix)]
    #[test]
    fn a_cached_build_notes_the_use_of_its_image() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(&tmp);
        let base = s
            .import_for_test("a", vec![file_layer(&[("u", b"x")])], 1)
            .unwrap();
        let build = |s: &Store| {
            s.create_image(ImageConfig::default(), base.diff_ids.clone(), 0, false)
                .unwrap()
        };
        let digest = build(&s);
        build(&s);
        let last_used = s.image_dir(&digest).join("last_used");
        assert!(!last_used.exists(), "compression off: nothing changes");
        s.use_compression(ImageCompression::Idle, 3600);
        build(&s);
        assert!(last_used.exists());
        // A time that cannot be written does not fail the build.
        std::fs::remove_file(&last_used).unwrap();
        std::fs::create_dir(&last_used).unwrap();
        assert_eq!(build(&s), digest);
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

    /// Every file of an image tree: path → (contents, mode, mtime).
    #[cfg(unix)]
    fn snapshot(root: &Path) -> BTreeMap<String, (Vec<u8>, u32, Option<u64>)> {
        use std::os::unix::fs::PermissionsExt;
        fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, (Vec<u8>, u32, Option<u64>)>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let path = e.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    walk(&path, root, out);
                } else if meta.is_file() {
                    let name = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    out.insert(
                        name,
                        (
                            std::fs::read(&path).unwrap(),
                            meta.permissions().mode() & 0o7777,
                            object::mtime_of(&meta),
                        ),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    /// The object keys of the files in an image's layers.
    fn object_keys(s: &Store, rec: &ImageRecord) -> Vec<String> {
        let mut keys = Vec::new();
        for id in &rec.diff_ids {
            for e in s.read_layer_index(id).unwrap().entries {
                if let EntryKind::File { object, .. } = e.kind {
                    keys.push(object);
                }
            }
        }
        keys
    }

    #[cfg(unix)]
    #[test]
    fn an_image_whose_objects_were_frozen_is_rebuilt_with_the_same_files() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[("big", &[3u8; 20_000]), ("small", b"x")])],
                1,
            )
            .unwrap();
        let before = snapshot(&s.rootfs_path(&rec.digest));
        remove_tree(&s.rootfs_path(&rec.digest)).unwrap();
        for key in object_keys(&s, &rec) {
            s.objects.freeze(&key).unwrap();
        }
        assert_eq!(s.objects.cold_stats().unwrap().objects, 1);
        assert!(!s.has_artifact(&rec.digest));
        s.ensure_artifact(&rec.digest).unwrap();
        assert!(s.has_artifact(&rec.digest));
        assert_eq!(snapshot(&s.rootfs_path(&rec.digest)), before);
        assert_eq!(s.objects.cold_stats().unwrap().objects, 0);
    }

    #[cfg(unix)]
    #[test]
    fn thaw_layers_makes_every_object_of_the_layers_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[("big", &[3u8; 20_000]), ("small", b"x")])],
                1,
            )
            .unwrap();
        remove_tree(&s.rootfs_path(&rec.digest)).unwrap();
        let keys = object_keys(&s, &rec);
        for key in &keys {
            s.objects.freeze(key).unwrap();
        }
        assert_eq!(s.objects.cold_stats().unwrap().objects, 1);
        s.thaw_layers(&rec.diff_ids).unwrap();
        for key in &keys {
            assert!(!s.objects.is_cold(key));
        }
        assert_eq!(s.objects.cold_stats().unwrap().objects, 0);
        // Only the objects: a build reads them, and builds no tree for the image.
        assert!(!s.has_artifact(&rec.digest));
    }

    /// A thaw that fails at its second object changes the cold generation all the same:
    /// the first object is plain now.
    #[cfg(unix)]
    #[test]
    fn a_thaw_that_fails_half_way_changes_the_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[("a", &[3u8; 20_000]), ("b", &[4u8; 20_000])])],
                1,
            )
            .unwrap();
        remove_tree(&s.rootfs_path(&rec.digest)).unwrap();
        let keys = object_keys(&s, &rec);
        for key in &keys {
            s.objects.freeze(key).unwrap();
        }
        damage_cold(&s, &keys[1]);
        let before = s.objects.cold_generation();
        assert!(s.thaw_layers(&rec.diff_ids).is_err());
        assert!(!s.objects.is_cold(&keys[0]), "the first object was thawed");
        assert!(s.objects.is_cold(&keys[1]));
        assert_ne!(s.objects.cold_generation(), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_image_with_frozen_layers_thaws_them() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let shared = file_layer(&[("big", &[3u8; 20_000])]);
        let a = s.import_for_test("a", vec![shared.clone()], 1).unwrap();
        remove_tree(&s.rootfs_path(&a.digest)).unwrap();
        for key in object_keys(&s, &a) {
            s.objects.freeze(&key).unwrap();
        }
        assert_eq!(s.objects.cold_stats().unwrap().objects, 1);
        // The layer is stored, so the import skips it and only builds the new image.
        let b = s
            .import_for_test("b", vec![shared, file_layer(&[("extra", b"y")])], 2)
            .unwrap();
        assert_ne!(a.digest, b.digest);
        assert_eq!(
            std::fs::read(s.rootfs_path(&b.digest).join("big")).unwrap(),
            vec![3u8; 20_000]
        );
        assert_eq!(s.objects.cold_stats().unwrap().objects, 0);
    }

    #[cfg(unix)]
    #[test]
    fn old_layers_without_times_keep_the_object_time() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![timed_layer(&[("big", &[3u8; 20_000], 1_000_000)])],
                1,
            )
            .unwrap();
        // A layer index of an older podbag has no times.
        let path = s.layer_path(&rec.diff_ids[0]);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for e in json["entries"].as_array_mut().unwrap() {
            e.as_object_mut().unwrap().remove("mtime");
        }
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let key = object_keys(&s, &rec).remove(0);
        let object_time = object::mtime_of(&std::fs::metadata(s.objects.path(&key)).unwrap());
        remove_tree(&s.rootfs_path(&rec.digest)).unwrap();
        s.objects.freeze(&key).unwrap();
        assert!(s.objects.is_cold(&key));
        s.ensure_artifact(&rec.digest).unwrap();
        let file = s.rootfs_path(&rec.digest).join("big");
        assert!(object_time.is_some());
        assert_eq!(
            object::mtime_of(&std::fs::metadata(file).unwrap()),
            object_time
        );
    }

    #[test]
    fn last_use_is_the_latest_of_use_names_and_pull() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("x", b"1")])], 5)
            .unwrap();
        let refs = s.read_refs().unwrap();
        let pulled = object::mtime_of(
            &std::fs::metadata(s.image_dir(&rec.digest).join("image.json")).unwrap(),
        )
        .unwrap();
        assert!(s.last_use(&rec.digest, &refs) >= pulled);
        s.note_use(&rec.digest).unwrap();
        assert!(s.last_use(&rec.digest, &refs) + 2 >= now());
        // A name used later counts too.
        let later = now() + 1000;
        let mut late = refs.clone();
        late.values_mut().for_each(|r| r.last_used = later);
        assert_eq!(s.last_use(&rec.digest, &late), later);
        // Nothing is known of an image that is not there.
        assert_eq!(s.last_use("sha256:nope", &Refs::new()), 0);
    }

    /// Make an image look unused for a long time: its pull time and its `last_used` file
    /// go back a day (its names keep the use time they were given).
    fn age(s: &Store, digest: &str) {
        age_by(s, digest, 86_400);
    }

    fn age_by(s: &Store, digest: &str, secs: u64) {
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        for name in ["image.json", "last_used"] {
            let path = s.image_dir(digest).join(name);
            if path.exists() {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(past)
                    .unwrap();
            }
        }
    }

    fn idle(after: u64) -> FreezeSelect {
        FreezeSelect::Idle { now: now(), after }
    }

    fn all_cold(s: &Store, rec: &ImageRecord) -> bool {
        object_keys(s, rec).iter().all(|k| s.objects.is_cold(k))
    }

    #[test]
    fn freeze_compresses_idle_images_and_keeps_recent_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let old = s
            .import_for_test("old", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        age(&s, &old.digest);
        let new = s
            .import_for_test("new", vec![file_layer(&[("f", &[6u8; 20_000])])], 1)
            .unwrap();
        s.note_use(&new.digest).unwrap();
        let report = s
            .freeze_images(&idle(3600), &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert!(!s.has_artifact(&old.digest));
        assert!(all_cold(&s, &old));
        assert!(s.has_artifact(&new.digest));
        assert!(object_keys(&s, &new).iter().all(|k| !s.objects.is_cold(k)));
        assert_eq!(report.images, 1);
        assert_eq!(report.objects, 1);
        assert!(report.finished);
    }

    #[test]
    fn freeze_never_touches_images_in_use_or_keep() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let a = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        let b = s
            .import_for_test("b", vec![file_layer(&[("f", &[6u8; 20_000])])], 1)
            .unwrap();
        age(&s, &a.digest);
        age(&s, &b.digest);
        let in_use = || HashMap::from([(a.digest.clone(), "c".repeat(64))]);
        let report = s
            .freeze_images(
                &FreezeSelect::Evictable,
                &in_use,
                &b.digest,
                FreezeBudget::UNLIMITED,
            )
            .unwrap();
        assert!(s.has_artifact(&a.digest));
        assert!(s.has_artifact(&b.digest));
        assert_eq!(report.images, 0);
        assert_eq!(report.objects, 0);
        assert_eq!(s.objects.cold_stats().unwrap().objects, 0);
    }

    #[test]
    fn an_object_shared_with_a_warm_image_stays_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let a = s
            .import_for_test(
                "a",
                vec![file_layer(&[
                    ("shared", &[9u8; 20_000]),
                    ("own", &[5u8; 20_000]),
                ])],
                1,
            )
            .unwrap();
        let b = s
            .import_for_test(
                "b",
                vec![file_layer(&[
                    ("shared", &[9u8; 20_000]),
                    ("own", &[6u8; 20_000]),
                ])],
                1,
            )
            .unwrap();
        let shared: Vec<String> = object_keys(&s, &a)
            .into_iter()
            .filter(|k| object_keys(&s, &b).contains(k))
            .collect();
        assert_eq!(shared.len(), 1);
        let report = s
            .freeze_images(
                &FreezeSelect::Images(vec![a.digest.clone()]),
                &HashMap::new,
                "",
                FreezeBudget::UNLIMITED,
            )
            .unwrap();
        assert!(!s.has_artifact(&a.digest));
        assert!(s.has_artifact(&b.digest));
        for key in object_keys(&s, &a) {
            assert_eq!(s.objects.is_cold(&key), !shared.contains(&key), "{key}");
        }
        assert_eq!(report.objects, 1);
        assert!(report.finished);
    }

    /// EROFS metadata links no object, so the link count does not protect what a
    /// composefs image names: the warm keys must.
    #[test]
    fn an_object_a_composefs_image_names_stays_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let shared = file_layer(&[("shared", &[9u8; 20_000])]);
        let a = s.import_for_test("a", vec![shared.clone()], 1).unwrap();
        let b = s
            .import_for_test("b", vec![shared, file_layer(&[("own", &[6u8; 20_000])])], 1)
            .unwrap();
        // b as a composefs pull leaves it: EROFS metadata and no tree.
        remove_tree(&s.rootfs_path(&b.digest)).unwrap();
        std::fs::write(s.erofs_path(&b.digest), b"metadata").unwrap();
        let key = object_keys(&s, &a).remove(0);
        let report = s
            .freeze_images(
                &FreezeSelect::Images(vec![a.digest.clone()]),
                &HashMap::new,
                "",
                FreezeBudget::UNLIMITED,
            )
            .unwrap();
        assert!(!s.has_artifact(&a.digest));
        assert!(!s.objects.is_cold(&key));
        assert_eq!(report.objects, 0);
        assert!(report.finished);
    }

    #[test]
    fn freeze_stops_at_the_budget_and_the_next_call_goes_on() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[
                    ("x", &[1u8; 20_480]),
                    ("y", &[2u8; 20_480]),
                    ("z", &[3u8; 20_480]),
                ])],
                1,
            )
            .unwrap();
        let select = FreezeSelect::Images(vec![rec.digest.clone()]);
        let budget = FreezeBudget {
            bytes: 20_000,
            time: std::time::Duration::MAX,
            ..FreezeBudget::UNLIMITED
        };
        let first = s.freeze_images(&select, &HashMap::new, "", budget).unwrap();
        assert!(!first.finished);
        assert_eq!(first.objects, 1);
        assert_eq!(first.images, 1);
        assert_eq!(s.objects.cold_stats().unwrap().objects, 1);
        let second = s
            .freeze_images(&select, &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert!(second.finished);
        assert_eq!(second.objects, 2);
        assert_eq!(second.images, 0);
        assert!(all_cold(&s, &rec));
    }

    /// A spent budget leaves the next images as they are: removing their artifacts would
    /// only make their next use build them again.
    #[test]
    fn the_budget_spares_the_artifacts_of_images_it_does_not_reach() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let a = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        let b = s
            .import_for_test("b", vec![file_layer(&[("f", &[6u8; 20_000])])], 1)
            .unwrap();
        let budget = FreezeBudget {
            bytes: 20_000,
            time: std::time::Duration::MAX,
            ..FreezeBudget::UNLIMITED
        };
        let select = FreezeSelect::Images(vec![a.digest.clone(), b.digest.clone()]);
        let report = s.freeze_images(&select, &HashMap::new, "", budget).unwrap();
        assert!(!report.finished);
        assert_eq!((report.images, report.objects), (1, 1));
        assert!(!s.has_artifact(&a.digest));
        assert!(s.has_artifact(&b.digest));
    }

    #[test]
    fn evictable_freezes_the_least_recently_used_image_first() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let a = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        let b = s
            .import_for_test("b", vec![file_layer(&[("f", &[6u8; 20_000])])], 1)
            .unwrap();
        age_by(&s, &a.digest, 86_400);
        age_by(&s, &b.digest, 2 * 86_400);
        let budget = FreezeBudget {
            bytes: 1,
            time: std::time::Duration::MAX,
            ..FreezeBudget::UNLIMITED
        };
        let report = s
            .freeze_images(&FreezeSelect::Evictable, &HashMap::new, "", budget)
            .unwrap();
        assert_eq!(report.objects, 1);
        assert!(s.has_artifact(&a.digest));
        assert!(!s.has_artifact(&b.digest));
        assert!(all_cold(&s, &b));
    }

    /// An automatic freeze leaves an object larger than its `largest` plain: compressing
    /// it in one go would hold the exclusive lock for seconds. The image is not marked, so
    /// `system compress` (no limit) compresses it later.
    #[test]
    fn an_automatic_freeze_leaves_large_objects_to_system_compress() {
        assert_eq!(FreezeBudget::AUTO.largest, 32 << 20);
        assert_eq!(FreezeBudget::UNLIMITED.largest, u64::MAX);
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[
                    ("big", &[5u8; 40_000]),
                    ("small", &[6u8; 8192]),
                ])],
                1,
            )
            .unwrap();
        let select = FreezeSelect::Images(vec![rec.digest.clone()]);
        let budget = FreezeBudget {
            largest: 20_000,
            ..FreezeBudget::AUTO
        };
        let report = s.freeze_images(&select, &HashMap::new, "", budget).unwrap();
        assert_eq!(report.objects, 1);
        assert_eq!(report.bytes_before, 8192);
        assert!(report.finished);
        let keys = object_keys(&s, &rec);
        let plain: Vec<u64> = keys
            .iter()
            .filter(|k| !s.objects.is_cold(k))
            .map(|k| std::fs::metadata(s.objects.path(k)).unwrap().len())
            .collect();
        assert_eq!(plain, [40_000]);
        assert!(!s.image_dir(&rec.digest).join(FROZEN).exists());
        let report = s
            .freeze_images(&select, &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert_eq!(report.objects, 1);
        assert!(all_cold(&s, &rec));
        assert!(s.image_dir(&rec.digest).join(FROZEN).exists());
    }

    /// Under a size limit, an image whose objects are all too large (or too small to
    /// compress) keeps its artifact: removing it would save nothing. `system compress`
    /// (no limit) compresses it.
    #[test]
    fn an_automatic_freeze_keeps_the_artifact_of_an_image_it_cannot_compress() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[
                    ("big", &[5u8; 40_000]),
                    ("small", &[1u8; 100]),
                ])],
                1,
            )
            .unwrap();
        let select = FreezeSelect::Images(vec![rec.digest.clone()]);
        let budget = FreezeBudget {
            largest: 20_000,
            ..FreezeBudget::AUTO
        };
        let report = s.freeze_images(&select, &HashMap::new, "", budget).unwrap();
        assert_eq!((report.images, report.objects), (0, 0));
        assert!(report.finished);
        assert!(s.has_artifact(&rec.digest));
        assert!(!s.image_dir(&rec.digest).join(FROZEN).exists());
        let report = s
            .freeze_images(&select, &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert_eq!((report.images, report.objects), (1, 1));
        assert!(!s.has_artifact(&rec.digest));
        // The big object is compressed; the small one stays plain.
        let cold = object_keys(&s, &rec)
            .iter()
            .filter(|k| s.objects.is_cold(k))
            .count();
        assert_eq!(cold, 1);
        assert!(s.image_dir(&rec.digest).join(FROZEN).exists());
    }

    fn new_run(budget: FreezeBudget) -> FreezeRun {
        FreezeRun {
            start: std::time::Instant::now(),
            budget,
            report: FreezeReport::default(),
            tried: HashSet::new(),
            linked: HashSet::new(),
            layer_keys: HashMap::new(),
            image_keys: HashMap::new(),
        }
    }

    /// An object an earlier batch of the same call handled counts for the `frozen` mark
    /// only while it is still compressed: a build may have made it plain in between.
    #[test]
    fn an_object_made_plain_since_an_earlier_batch_keeps_the_image_unmarked() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        remove_tree(&s.rootfs_path(&rec.digest)).unwrap();
        let key = object_keys(&s, &rec).remove(0);
        let select = FreezeSelect::Images(vec![rec.digest.clone()]);
        let mut run = new_run(FreezeBudget::UNLIMITED);
        run.tried.insert(key.clone());
        s.freeze_batch(&select, &HashMap::new, "", &mut run)
            .unwrap();
        assert!(!s.image_dir(&rec.digest).join(FROZEN).exists());
        assert!(!s.objects.is_cold(&key));
        // Compressed, as the earlier batch left it: the image is whole.
        s.objects.freeze(&key).unwrap();
        s.freeze_batch(&select, &HashMap::new, "", &mut run)
            .unwrap();
        assert!(s.image_dir(&rec.digest).join(FROZEN).exists());
    }

    /// Once its idle images are frozen, a store's next freeze reads no layer index and
    /// looks at no object: commands run it often.
    #[test]
    fn a_second_freeze_skips_frozen_images_without_reading_them() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let old = s
            .import_for_test("old", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        age(&s, &old.digest);
        let new = s
            .import_for_test("new", vec![file_layer(&[("f", &[6u8; 20_000])])], 1)
            .unwrap();
        s.note_use(&new.digest).unwrap();
        let first = s
            .freeze_images(&idle(3600), &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert_eq!((first.images, first.objects, first.finished), (1, 1, true));
        assert!(s.image_dir(&old.digest).join(FROZEN).exists());
        // Reading a layer index would now fail.
        for id in old.diff_ids.iter().chain(&new.diff_ids) {
            std::fs::remove_file(s.layer_path(id)).unwrap();
        }
        let second = s
            .freeze_images(&idle(3600), &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert_eq!(
            second,
            FreezeReport {
                finished: true,
                ..Default::default()
            }
        );
    }

    /// An image used again gets its artifact back and plain objects: its next freeze
    /// compresses them again.
    #[test]
    fn a_rebuilt_image_is_frozen_again() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        let select = FreezeSelect::Images(vec![rec.digest.clone()]);
        s.freeze_images(&select, &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert!(all_cold(&s, &rec));
        s.ensure_artifact(&rec.digest).unwrap();
        assert!(!all_cold(&s, &rec));
        let report = s
            .freeze_images(&select, &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert_eq!(
            (report.images, report.objects, report.finished),
            (1, 1, true)
        );
        assert!(!s.has_artifact(&rec.digest));
        assert!(all_cold(&s, &rec));
        assert!(s.image_dir(&rec.digest).join(FROZEN).exists());
    }

    /// A tree still links an object (another freeze is deleting it, or a crash left it):
    /// the object stays plain, and the image is not marked, so a later freeze goes on.
    #[cfg(unix)]
    #[test]
    fn an_object_a_leftover_tree_links_keeps_its_image_unmarked() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        let leftover = s.image_dir(&rec.digest).join("rootfs.9.freeze.tmp");
        std::fs::rename(s.rootfs_path(&rec.digest), &leftover).unwrap();
        let select = FreezeSelect::Images(vec![rec.digest.clone()]);
        let report = s
            .freeze_images(&select, &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert_eq!(report.objects, 0);
        assert!(report.finished);
        assert!(!s.image_dir(&rec.digest).join(FROZEN).exists());
        remove_tree(&leftover).unwrap();
        let report = s
            .freeze_images(&select, &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert_eq!(report.objects, 1);
        assert!(all_cold(&s, &rec));
        assert!(s.image_dir(&rec.digest).join(FROZEN).exists());
    }

    #[test]
    fn freezing_a_named_image_ignores_its_age() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        s.note_use(&rec.digest).unwrap();
        let report = s
            .freeze_images(
                &FreezeSelect::Images(vec![rec.digest.clone()]),
                &HashMap::new,
                "",
                FreezeBudget::UNLIMITED,
            )
            .unwrap();
        assert!(!s.has_artifact(&rec.digest));
        assert!(all_cold(&s, &rec));
        assert_eq!(report.images, 1);
        assert!(report.finished);
    }

    #[test]
    fn unnamed_images_wait_for_the_idle_period_too() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        s.update_refs(|refs| refs.clear()).unwrap();
        assert_eq!(s.unnamed_images().unwrap(), [rec.digest.as_str()]);
        let report = s
            .freeze_images(&idle(3600), &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        assert!(s.has_artifact(&rec.digest));
        assert!(!all_cold(&s, &rec));
        assert_eq!(
            report,
            FreezeReport {
                finished: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn freezing_lowers_the_store_size() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[("x", &[5u8; 20_000]), ("y", &[6u8; 50_000])])],
                1,
            )
            .unwrap();
        age(&s, &rec.digest);
        let before = s.objects.total_size().unwrap().1;
        let report = s
            .freeze_images(&idle(3600), &HashMap::new, "", FreezeBudget::UNLIMITED)
            .unwrap();
        let after = s.objects.total_size().unwrap().1;
        assert!(after < before, "{after} >= {before}");
        assert_eq!(report.bytes_before, 70_000);
        assert!(report.bytes_after < report.bytes_before);
        assert_eq!(before - after, report.bytes_before - report.bytes_after);
    }

    /// A freeze that stops between removing the artifact and compressing the objects
    /// leaves an image that is built again on its next use.
    #[test]
    fn a_freeze_cut_short_leaves_a_usable_image() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        let dir = s.image_dir(&rec.digest);
        let leftover = dir.join("rootfs.1.freeze.tmp");
        std::fs::rename(s.rootfs_path(&rec.digest), &leftover).unwrap();
        assert!(!s.has_artifact(&rec.digest));
        s.ensure_artifact(&rec.digest).unwrap();
        assert_eq!(
            std::fs::read(s.rootfs_path(&rec.digest).join("f")).unwrap(),
            vec![5u8; 20_000]
        );
        s.collect_garbage().unwrap();
        assert!(!leftover.exists());
        assert!(s.has_artifact(&rec.digest));
    }

    /// The artifact of the other mount method goes too: if the method changes back, that
    /// artifact would name objects that are no longer plain.
    #[test]
    fn freezing_removes_the_artifact_of_either_method() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        // An EROFS image left from when the store used composefs.
        std::fs::write(s.erofs_path(&rec.digest), b"metadata").unwrap();
        let report = s
            .freeze_images(
                &FreezeSelect::Images(vec![rec.digest.clone()]),
                &HashMap::new,
                "",
                FreezeBudget::UNLIMITED,
            )
            .unwrap();
        assert!(!s.rootfs_path(&rec.digest).exists());
        assert!(!s.erofs_path(&rec.digest).exists());
        assert!(all_cold(&s, &rec));
        assert_eq!(report.images, 1);
        let names: Vec<String> = std::fs::read_dir(s.image_dir(&rec.digest))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    }

    /// Objects that cannot be frozen (too small) do not keep the freeze going.
    #[test]
    fn small_objects_stay_plain_and_the_freeze_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[("small", b"x"), ("big", &[5u8; 20_000])])],
                1,
            )
            .unwrap();
        let report = s
            .freeze_images(
                &FreezeSelect::Images(vec![rec.digest.clone()]),
                &HashMap::new,
                "",
                FreezeBudget::UNLIMITED,
            )
            .unwrap();
        assert!(report.finished);
        assert_eq!(report.objects, 1);
        assert_eq!(s.objects.cold_stats().unwrap().objects, 1);
    }

    #[test]
    fn freezing_reads_in_use_while_the_store_is_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        s.import_for_test("a", vec![file_layer(&[("f", &[5u8; 20_000])])], 1)
            .unwrap();
        let lock_path = s.root.join("lock");
        let calls = std::cell::Cell::new(0);
        let locked = || {
            calls.set(calls.get() + 1);
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
        let report = s
            .freeze_images(
                &FreezeSelect::Evictable,
                &locked,
                "",
                FreezeBudget::UNLIMITED,
            )
            .unwrap();
        assert!(report.finished);
        assert!(calls.get() >= 1);
    }

    #[test]
    fn compression_of_reports_stored_and_original_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let big = vec![7u8; 20_000];
        let rec = s
            .import_for_test(
                "a",
                vec![file_layer(&[("big", &big), ("small", b"tiny")])],
                1,
            )
            .unwrap();
        assert_eq!(s.compression_of(&rec.digest).unwrap(), None);
        s.freeze_images(
            &FreezeSelect::Images(vec![rec.digest.clone()]),
            &HashMap::new,
            "",
            FreezeBudget::UNLIMITED,
        )
        .unwrap();
        let stats = s.compression_of(&rec.digest).unwrap().unwrap();
        assert_eq!(stats.original, 20_000 + 4);
        assert!(stats.stored < stats.original, "{stats:?}");
        // The big file counts at its compressed size, the small one stays plain.
        assert_eq!(stats.stored, s.objects.cold_stats().unwrap().bytes + 4);
        s.ensure_artifact(&rec.digest).unwrap();
        assert_eq!(s.compression_of(&rec.digest).unwrap(), None);
    }

    /// An image with one big file, compressed.
    fn frozen_image(s: &Store, name: &str, layers: Vec<Vec<u8>>) -> ImageRecord {
        let rec = s.import_for_test(name, layers, 1).unwrap();
        s.freeze_images(
            &FreezeSelect::Images(vec![rec.digest.clone()]),
            &HashMap::new,
            "",
            FreezeBudget::UNLIMITED,
        )
        .unwrap();
        rec
    }

    #[test]
    fn compression_numbers_come_from_the_cache_while_the_generation_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = frozen_image(&s, "a", vec![file_layer(&[("big", &[7u8; 20_000])])]);
        let stats = s.compression_of(&rec.digest).unwrap().unwrap();
        assert!(s.image_dir(&rec.digest).join("compression.json").exists());
        // The objects are not read again: the layer index is not even needed.
        for id in &rec.diff_ids {
            std::fs::remove_file(s.layer_path(id)).unwrap();
        }
        assert_eq!(s.compression_of(&rec.digest).unwrap(), Some(stats));
    }

    #[test]
    fn a_stale_or_damaged_cache_is_not_used() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let rec = frozen_image(&s, "a", vec![file_layer(&[("big", &[7u8; 20_000])])]);
        let stats = s.compression_of(&rec.digest).unwrap().unwrap();
        let cache = s.image_dir(&rec.digest).join("compression.json");
        let now = s.objects.cold_generation();
        assert!(!now.is_empty(), "the freeze gave the generation a value");
        std::fs::write(&cache, r#"{"generation":"old","stored":1,"original":2}"#).unwrap();
        assert_eq!(s.compression_of(&rec.digest).unwrap(), Some(stats));
        std::fs::write(&cache, "not json").unwrap();
        assert_eq!(s.compression_of(&rec.digest).unwrap(), Some(stats));
        std::fs::write(&cache, format!(r#"{{"generation":"{now}"}}"#)).unwrap();
        assert_eq!(s.compression_of(&rec.digest).unwrap(), Some(stats));
        // With the current generation, the cache is what counts.
        std::fs::write(&cache, format!(r#"{{"generation":"{now}","none":true}}"#)).unwrap();
        assert_eq!(s.compression_of(&rec.digest).unwrap(), None);
    }

    #[test]
    fn a_thaw_for_another_image_changes_the_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let shared = file_layer(&[("big", &[7u8; 20_000])]);
        let a = frozen_image(&s, "a", vec![shared.clone()]);
        assert!(s.compression_of(&a.digest).unwrap().is_some());
        let before = s.objects.cold_generation();
        // b uses the same layer: building its tree makes the object plain again.
        let b = s
            .import_for_test("b", vec![shared, file_layer(&[("x", b"x")])], 1)
            .unwrap();
        assert_ne!(s.objects.cold_generation(), before);
        assert_eq!(s.compression_of(&a.digest).unwrap(), None);
        // Freezing b compresses the object again: a's numbers come back.
        let before = s.objects.cold_generation();
        s.freeze_images(
            &FreezeSelect::Images(vec![b.digest.clone()]),
            &HashMap::new,
            "",
            FreezeBudget::UNLIMITED,
        )
        .unwrap();
        assert_ne!(s.objects.cold_generation(), before);
        assert!(s.compression_of(&a.digest).unwrap().is_some());
    }

    #[test]
    fn an_object_in_two_layers_counts_once_and_a_plain_file_beats_its_zst() {
        let tmp = tempfile::tempdir().unwrap();
        let s = open(&tmp);
        let big = [7u8; 20_000];
        let rec = frozen_image(
            &s,
            "a",
            vec![file_layer(&[("one", &big)]), file_layer(&[("two", &big)])],
        );
        let stats = s.compression_of(&rec.digest).unwrap().unwrap();
        assert_eq!(stats.original, 20_000);
        // A crash between writing the plain file and deleting the .zst leaves both: the
        // plain file is the object (rule 2), so nothing is compressed.
        let key = object_keys(&s, &rec)[0].clone();
        let mut plain = Vec::new();
        s.objects
            .open(&key)
            .unwrap()
            .read_to_end(&mut plain)
            .unwrap();
        std::fs::write(s.objects.path(&key), plain).unwrap();
        assert!(s.objects.cold_path(&key).exists());
        s.objects.new_cold_generation();
        assert_eq!(s.compression_of(&rec.digest).unwrap(), None);
    }

    #[test]
    fn a_freeze_report_crosses_a_process_boundary() {
        let report = FreezeReport {
            images: 1,
            objects: 2,
            bytes_before: 3,
            bytes_after: 4,
            finished: true,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(serde_json::from_str::<FreezeReport>(&json).unwrap(), report);
    }
}
