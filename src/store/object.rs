//! Objects: file contents stored once, keyed by content hash and file metadata.

use crate::{Error, ErrorKind, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The metadata that is part of an object's identity: hard links share one inode,
/// so files that differ in owner, mode or xattrs need separate objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub xattrs: Vec<(String, Vec<u8>)>,
}

/// What the compressed objects of a store hold.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ColdStats {
    /// How many compressed objects there are.
    pub objects: u64,
    /// Their size on disk.
    pub bytes: u64,
    /// The size their plain files had.
    pub original_bytes: u64,
}

/// What the objects of a store take: `total_size` and `cold_stats` together.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Usage {
    /// How many objects there are (time copies included), and their size on disk.
    pub objects: u64,
    pub bytes: u64,
    pub cold: ColdStats,
}

pub struct ObjectStore {
    dir: PathBuf,
    /// Set owner and xattrs on new objects. False only in unit tests that run unprivileged.
    set_owner: bool,
}

impl ObjectStore {
    pub fn new(dir: PathBuf, set_owner: bool) -> Result<ObjectStore> {
        std::fs::create_dir_all(dir.join("tmp")).map_err(|e| io_error(&dir, e))?;
        Ok(ObjectStore { dir, set_owner })
    }

    /// `<sha256>.<uid>.<gid>.<octal mode>`, plus `.x<16 hex>` when there are xattrs.
    pub fn key(sha256: &str, meta: &Meta) -> String {
        let mut key = format!("{sha256}.{}.{}.{:o}", meta.uid, meta.gid, meta.mode);
        if !meta.xattrs.is_empty() {
            let mut sorted = meta.xattrs.clone();
            sorted.sort();
            let mut h = Sha256::new();
            for (k, v) in &sorted {
                h.update(k.as_bytes());
                h.update([0]);
                h.update(v);
                h.update([0]);
            }
            let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
            key.push_str(&format!(".x{}", &hex[..16]));
        }
        key
    }

    /// `<store>/cold-generation`: a token that changes each time an object changes between
    /// plain and compressed. What is counted from the objects stays true while it is the same.
    fn generation_path(&self) -> PathBuf {
        self.dir.with_file_name("cold-generation")
    }

    /// The current token; "" when there is none yet.
    pub fn cold_generation(&self) -> String {
        std::fs::read_to_string(self.generation_path()).unwrap_or_default()
    }

    /// Give the token a new value. Call it after objects changed between plain and
    /// compressed, once they are on disk. Best effort: a token that stays the same only
    /// keeps old compression numbers in use; the objects are not affected.
    pub fn new_cold_generation(&self) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let value = format!(
            "{nanos}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = self.tmp_path();
        let written = File::create(&tmp)
            .and_then(|mut f| f.write_all(value.as_bytes()).and_then(|()| f.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, self.generation_path()));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("podbag: warning: cannot note a change of compressed objects: {e}");
        }
    }

    /// True when new objects and trees get their recorded owner and xattrs.
    pub fn sets_owner(&self) -> bool {
        self.set_owner
    }

    /// True for a key this store could have made: `<64 hex>.<uid>.<gid>.<octal>[.x<16 hex>]`.
    pub fn is_valid_key(key: &str) -> bool {
        let hex = |s: &str, n: usize| {
            s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        let parts: Vec<&str> = key.split('.').collect();
        let base = parts.len() >= 4
            && hex(parts[0], 64)
            && parts[1].parse::<u32>().is_ok()
            && parts[2].parse::<u32>().is_ok()
            && !parts[3].is_empty()
            && parts[3].bytes().all(|b| (b'0'..=b'7').contains(&b));
        match parts.len() {
            4 => base,
            5 => base && parts[4].strip_prefix('x').is_some_and(|x| hex(x, 16)),
            _ => false,
        }
    }

    pub fn path(&self, key: &str) -> PathBuf {
        self.dir.join(ObjectStore::relative_path(key))
    }

    /// The name of the object's copy with modification time `mtime`. It is no valid key,
    /// so a layer index cannot name it.
    pub fn variant_key(key: &str, mtime: u64) -> String {
        format!("{key}.t{mtime}")
    }

    /// The object as a file whose modification time is `mtime`: the object itself when its
    /// time matches, else a copy of it with that time, made on first use. Hard links share
    /// their inode's time, so paths whose times differ cannot share one inode. `meta` is
    /// the object's metadata, given to the copy.
    pub fn with_mtime(&self, key: &str, meta: &Meta, mtime: u64) -> Result<PathBuf> {
        let object = self.path(key);
        let stored = std::fs::metadata(&object).map_err(|e| io_error(&object, e))?;
        if mtime_of(&stored) == Some(mtime) {
            return Ok(object);
        }
        let variant = self.path(&ObjectStore::variant_key(key, mtime));
        // One cut short by a crash is made again (only unfinished trees can link it: the
        // store syncs before an image's tree is put in place).
        if std::fs::metadata(&variant).is_ok_and(|v| v.len() == stored.len()) {
            return Ok(variant);
        }
        let tmp = self.tmp_path();
        let made = (|| {
            std::fs::copy(&object, &tmp)?;
            let copy = File::open(&tmp)?;
            if self.set_owner {
                apply_owner(&copy, meta)?;
            }
            // After chown: chown clears set-user-ID and set-group-ID bits.
            super::perm::set_file_mode(&copy, meta.mode)?;
            set_mtime(&copy, mtime)?;
            std::fs::rename(&tmp, &variant)
        })();
        if let Err(e) = made {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_error(&variant, e));
        }
        Ok(variant)
    }

    /// A new name in the objects' temporary directory.
    fn tmp_path(&self) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        self.dir.join("tmp").join(format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Where an object lives below the objects directory: `ab/ab…`.
    pub fn relative_path(key: &str) -> String {
        format!("{}/{key}", &key[..2])
    }

    /// Store `data` with `meta`. Returns the object key and the content size.
    /// Content that is already stored with the same metadata is not written again, and
    /// keeps its time; a new object gets `mtime`.
    pub fn insert(
        &self,
        data: &mut dyn Read,
        meta: &Meta,
        mtime: Option<u64>,
    ) -> Result<(String, u64)> {
        let tmp = self.tmp_path();
        let result = self.write_new(data, meta, mtime, &tmp);
        let _ = std::fs::remove_file(&tmp);
        result
    }

    fn write_new(
        &self,
        data: &mut dyn Read,
        meta: &Meta,
        mtime: Option<u64>,
        tmp: &Path,
    ) -> Result<(String, u64)> {
        let fail = |e: std::io::Error| io_error(tmp, e);
        // Read back by a repair. A file of the same name is a crash's leftover.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)
            .map_err(fail)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = data.read(&mut buf).map_err(|e| {
                Error::new(
                    ErrorKind::Runtime,
                    "cannot read image layer",
                    e.to_string(),
                    "try again",
                )
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(fail)?;
            size += n as u64;
        }
        let sha: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let key = ObjectStore::key(&sha, meta);
        let dest = self.path(&key);
        match std::fs::metadata(&dest) {
            Ok(stored) if stored.len() == size => return Ok((key, size)),
            Ok(stored) => {
                self.repair(&dest, &mut file, size, meta, &stored)
                    .map_err(|e| io_error(&dest, e))?;
                return Ok((key, size));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(&dest, e)),
        }
        // The object is frozen: it comes back with the time it had. A header that cannot be
        // read does not matter, the contents just hashed are the object.
        let cold = self.cold_path(&key);
        let frozen = cold.exists();
        let mtime = if frozen {
            File::open(&cold)
                .and_then(|mut f| super::zstd::read_cold_header(&mut f))
                .map_or(mtime, |header| header.mtime)
        } else {
            mtime
        };
        if self.set_owner {
            apply_owner(&file, meta).map_err(fail)?;
        }
        // After chown: chown clears set-user-ID and set-group-ID bits.
        super::perm::set_file_mode(&file, meta.mode).map_err(fail)?;
        if let Some(t) = mtime {
            set_mtime(&file, t).map_err(fail)?;
        }
        if frozen {
            // The compressed file goes next: the plain one must be on disk first.
            file.sync_all().map_err(fail)?;
        }
        drop(file);
        std::fs::create_dir_all(dest.parent().expect("object path has a parent")).map_err(fail)?;
        if frozen {
            // Never over a file that is there: trees may link it.
            match std::fs::hard_link(tmp, &dest) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(fail(e)),
                _ => {}
            }
            match std::fs::remove_file(&cold) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(io_error(&cold, e));
                }
                _ => {}
            }
            self.new_cold_generation();
        } else {
            std::fs::rename(tmp, &dest).map_err(fail)?;
        }
        Ok((key, size))
    }

    /// A crash can leave an object's name on disk without all of its data: the object is
    /// shorter than its contents. Write `contents` (all `size` bytes, checked by their
    /// hash) into the object's inode, so every tree that links it heals too. A write
    /// drops set-user-ID bits and file capabilities: put the metadata and time back.
    fn repair(
        &self,
        object: &Path,
        contents: &mut File,
        size: u64,
        meta: &Meta,
        stored: &std::fs::Metadata,
    ) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        contents.seek(SeekFrom::Start(0))?;
        let open = || std::fs::OpenOptions::new().write(true).open(object);
        let mut target = match open() {
            // Not root (unit tests), and the image's mode has no write bit for the owner.
            #[cfg(unix)]
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(object, std::fs::Permissions::from_mode(0o600))?;
                open()?
            }
            other => other?,
        };
        std::io::copy(contents, &mut target)?;
        target.set_len(size)?;
        if self.set_owner {
            apply_owner(&target, meta)?;
        }
        super::perm::set_file_mode(&target, meta.mode)?;
        if let Some(t) = mtime_of(stored) {
            set_mtime(&target, t)?;
        }
        target.sync_all()
    }

    /// Where the compressed form of an object lives: `<key>.zst` beside the object.
    pub fn cold_path(&self, key: &str) -> PathBuf {
        let mut path = self.path(key).into_os_string();
        path.push(".zst");
        PathBuf::from(path)
    }

    /// True when only the compressed form of the object is stored.
    pub fn is_cold(&self, key: &str) -> bool {
        !self.path(key).exists() && self.cold_path(key).exists()
    }

    /// Compress the object and delete its plain file. Returns the plain size and the
    /// compressed size. Does nothing and returns `(0, 0)` when there is no plain file, when
    /// it is smaller than a file system block, or when a tree links its inode: freezing
    /// would then save no space. Its time copies that no tree links go with it.
    ///
    /// The order keeps the object whole: the compressed file is written and synced under a
    /// temporary name, put in place, and only then is the plain file deleted. A crash in
    /// between leaves both, and the plain file is the one that counts.
    pub fn freeze(&self, key: &str) -> Result<(u64, u64)> {
        let object = self.path(key);
        let stored = match std::fs::metadata(&object) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(e) => return Err(io_error(&object, e)),
        };
        if stored.len() < MIN_COLD_SIZE || link_count(&stored) > 1 {
            return Ok((0, 0));
        }
        let cold = self.cold_path(key);
        let tmp = self.tmp_path();
        let packed = (|| {
            let mut src = File::open(&object)?;
            let mut out = std::io::BufWriter::new(File::create(&tmp)?);
            let packed =
                super::zstd::write_cold(&mut src, stored.len(), mtime_of(&stored), &mut out)?;
            out.flush()?;
            out.get_ref().sync_all()?;
            std::fs::rename(&tmp, &cold)?;
            Ok::<u64, std::io::Error>(packed)
        })();
        let packed = match packed {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(io_error(&cold, e));
            }
        };
        std::fs::remove_file(&object).map_err(|e| io_error(&object, e))?;
        self.remove_free_copies(key)?;
        Ok((stored.len(), packed))
    }

    /// Delete the time copies of `key` that no tree links.
    fn remove_free_copies(&self, key: &str) -> Result<()> {
        let shard = self.path(key).with_file_name("");
        let entries = match std::fs::read_dir(&shard) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_error(&shard, e)),
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if variant_base(&name.to_string_lossy()) != Some(key) {
                continue;
            }
            let free = entry.metadata().is_ok_and(|m| link_count(&m) <= 1);
            if free {
                match std::fs::remove_file(entry.path()) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(io_error(&entry.path(), e));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Make the plain file of a frozen object again, with the owner, mode and time it had.
    /// `meta` is the object's metadata. The contents must hash to the start of the key: a
    /// damaged compressed file never becomes an object. A plain file that is already there
    /// stays as it is (it is the one trees may link), and only the compressed file goes.
    pub fn thaw(&self, key: &str, meta: &Meta) -> Result<()> {
        let (object, cold) = (self.path(key), self.cold_path(key));
        if !object.exists() {
            race_point("thaw");
            let tmp = self.tmp_path();
            let made = self.thaw_into(key, meta, &cold, &tmp, &object);
            let _ = std::fs::remove_file(&tmp);
            match made {
                // Another thaw, or an insert of the same contents, made the plain file and
                // deleted the compressed one after the check: the plain file is the object.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && object.exists() => {}
                other => other.map_err(|e| io_error(&cold, e))?,
            }
        }
        match std::fs::remove_file(&cold) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_error(&cold, e)),
            _ => Ok(()),
        }
    }

    fn thaw_into(
        &self,
        key: &str,
        meta: &Meta,
        cold: &Path,
        tmp: &Path,
        object: &Path,
    ) -> std::io::Result<()> {
        let invalid =
            |msg: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string());
        // Opened once: a thaw elsewhere can delete the file, not the contents held open.
        let mut source = File::open(cold)?;
        let header = super::zstd::read_cold_header(&mut source)?;
        let mut frames = cold_frames(source)?;
        let mut file = File::create(tmp)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = frames.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            size += n as u64;
        }
        let sha: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if size != header.size || key.get(..64) != Some(sha.as_str()) {
            return Err(invalid("the compressed object does not match its name"));
        }
        if self.set_owner {
            apply_owner(&file, meta)?;
        }
        // After chown: chown clears set-user-ID and set-group-ID bits.
        super::perm::set_file_mode(&file, meta.mode)?;
        if let Some(t) = header.mtime {
            set_mtime(&file, t)?;
        }
        file.sync_all()?;
        drop(file);
        race_point("thaw-link");
        // Never over a file that is there: trees may link it.
        match std::fs::hard_link(tmp, object) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
            _ => Ok(()),
        }
    }

    /// The contents of an object: the plain file, or else the compressed file, unpacked
    /// while it is read. The compressed contents are not checked against the name.
    pub fn open(&self, key: &str) -> Result<Box<dyn Read + Send>> {
        let object = self.path(key);
        match File::open(&object) {
            Ok(file) => Ok(Box::new(file)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                race_point("open");
                let cold = self.cold_path(key);
                match File::open(&cold) {
                    Ok(file) => Ok(Box::new(super::zstd::ZstdFrames::new(
                        std::io::BufReader::new(file),
                    ))),
                    // A thaw made the plain file and deleted the compressed one in between.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => match File::open(&object)
                    {
                        Ok(file) => Ok(Box::new(file)),
                        Err(_) => Err(io_error(&cold, e)),
                    },
                    Err(e) => Err(io_error(&cold, e)),
                }
            }
            Err(e) => Err(io_error(&object, e)),
        }
    }

    /// Write what the file system of the store holds in memory to disk (syncfs). The store
    /// calls it before it writes a name for new data, so a crash cannot leave a layer index
    /// or an image naming objects whose contents never reached the disk.
    #[cfg(target_os = "linux")]
    pub fn sync(&self) -> Result<()> {
        use std::os::fd::AsRawFd;
        let dir = File::open(&self.dir).map_err(|e| io_error(&self.dir, e))?;
        // SAFETY: syncfs takes an open descriptor and nothing else.
        if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
            return Err(io_error(&self.dir, std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// The store runs on Linux; elsewhere only its unit tests use it.
    #[cfg(not(target_os = "linux"))]
    pub fn sync(&self) -> Result<()> {
        Ok(())
    }

    /// Read every object, time copies included, and hash its contents. Returns how many
    /// there are and the names of those whose contents do not match their names.
    pub fn verify(&self) -> Result<(u64, Vec<String>)> {
        let mut names = Vec::new();
        self.for_each(|name, _| {
            if is_object_name(name) {
                names.push(name.to_string());
            }
            true
        })?;
        let mut damaged = Vec::new();
        for name in &names {
            if self.is_damaged(name)? {
                damaged.push(name.clone());
            }
        }
        damaged.sort();
        Ok((names.len() as u64, damaged))
    }

    /// True when the object (or time copy) `name` does not have the contents its name
    /// says. False when it is gone.
    pub fn is_damaged(&self, name: &str) -> Result<bool> {
        let path = self.path(name);
        if let Some(key) = cold_key(name) {
            return match File::open(&path) {
                // A file that does not unpack is damaged.
                Ok(file) => {
                    race_point("damaged");
                    Ok(!cold_matches(file, key).unwrap_or(false))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(io_error(&path, e)),
            };
        }
        match hash_file(&path) {
            Ok(sha) => Ok(name.get(..64) != Some(sha.as_str())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error(&path, e)),
        }
    }

    /// Remove what unfinished inserts left in the temporary directory. Call it only while
    /// no insert runs (under the exclusive store lock).
    pub fn clear_tmp(&self) {
        for entry in std::fs::read_dir(self.dir.join("tmp"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    /// For each time copy in `keep` that a tree links: when no tree links its object's
    /// own inode any more (only the object's name does), the copy takes the object's
    /// place, so the store keeps one inode for those contents, not two. The time of
    /// the object does not matter to composefs, which reads only its contents.
    #[cfg(unix)]
    pub fn adopt_copies(&self, keep: &HashSet<String>) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let mut copies = Vec::new();
        self.for_each(|name, _| {
            if keep.contains(name) && variant_base(name).is_some() {
                copies.push(name.to_string());
            }
            true
        })?;
        for name in copies {
            let Some(key) = variant_base(&name) else {
                continue;
            };
            let (copy, object) = (self.path(&name), self.path(key));
            let (Ok(c), Ok(o)) = (std::fs::metadata(&copy), std::fs::metadata(&object)) else {
                continue;
            };
            if o.nlink() == 1 && c.nlink() > 1 && c.ino() != o.ino() {
                std::fs::rename(&copy, &object).map_err(|e| io_error(&object, e))?;
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn adopt_copies(&self, _keep: &HashSet<String>) -> Result<()> {
        Ok(())
    }

    /// How many objects are compressed, the disk space they take, and the space their plain
    /// files took. A compressed file whose header cannot be read is not counted: `verify`
    /// reports it.
    pub fn cold_stats(&self) -> Result<ColdStats> {
        Ok(self.usage()?.cold)
    }

    /// `total_size` and `cold_stats` from one walk of the objects. Only the headers of the
    /// compressed files it finds are read, so without any it costs what `total_size` does.
    pub fn usage(&self) -> Result<Usage> {
        let mut names = Vec::new();
        let (mut objects, mut bytes) = (0, 0);
        self.for_each(|name, size| {
            objects += 1;
            bytes += size;
            if cold_key(name).is_some() {
                names.push((name.to_string(), size));
            }
            true
        })?;
        let mut stats = ColdStats::default();
        for (name, size) in names {
            let header = File::open(self.path(&name))
                .and_then(|mut f| super::zstd::read_cold_header(&mut f));
            if let Ok(header) = header {
                stats.objects += 1;
                stats.bytes += size;
                stats.original_bytes += header.size;
            }
        }
        Ok(Usage {
            objects,
            bytes,
            cold: stats,
        })
    }

    /// Number of objects and their size on disk. A compressed object counts once, at the
    /// size of its compressed file.
    pub fn total_size(&self) -> Result<(u64, u64)> {
        let mut count = 0;
        let mut bytes = 0;
        self.for_each(|_, size| {
            count += 1;
            bytes += size;
            true
        })?;
        Ok((count, bytes))
    }

    /// Delete every object whose key is not in `keep`, with its compressed file. A
    /// compressed file also goes when the plain file is there: the plain file is the one
    /// that counts. Returns the bytes freed.
    pub fn sweep(&self, keep: &HashSet<String>) -> Result<u64> {
        let mut freed = 0;
        let mut doomed = Vec::new();
        self.for_each(|name, size| {
            let gone = match cold_key(name) {
                Some(key) => !keep.contains(key) || self.path(key).exists(),
                None => !keep.contains(name),
            };
            if gone {
                doomed.push(name.to_string());
                freed += size;
            }
            true
        })?;
        for key in doomed {
            let path = self.path(&key);
            std::fs::remove_file(&path).map_err(|e| io_error(&path, e))?;
        }
        Ok(freed)
    }

    fn for_each(&self, mut f: impl FnMut(&str, u64) -> bool) -> Result<()> {
        #[cfg(test)]
        WALKS.with(|w| w.set(w.get() + 1));
        let read = |p: &Path| std::fs::read_dir(p).map_err(|e| io_error(p, e));
        for shard in read(&self.dir)? {
            let shard = shard.map_err(|e| io_error(&self.dir, e))?;
            if shard.file_name().len() != 2 {
                continue; // "tmp"
            }
            for obj in read(&shard.path())? {
                let obj = obj.map_err(|e| io_error(&shard.path(), e))?;
                let size = obj.metadata().map(|m| m.len()).unwrap_or(0);
                if !f(&obj.file_name().to_string_lossy(), size) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn apply_owner(file: &File, meta: &Meta) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    std::os::unix::fs::fchown(file, Some(meta.uid), Some(meta.gid))?;
    for (name, value) in &meta.xattrs {
        let c_name = std::ffi::CString::new(name.as_str()).map_err(std::io::Error::other)?;
        // SAFETY: valid descriptor, NUL-terminated name, and a value buffer of the given length.
        let rc = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                c_name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn apply_owner(file: &File, meta: &Meta) -> std::io::Result<()> {
    std::os::unix::fs::fchown(file, Some(meta.uid), Some(meta.gid))
}

#[cfg(not(unix))]
fn apply_owner(_file: &File, _meta: &Meta) -> std::io::Result<()> {
    Ok(())
}

/// Objects smaller than this are not frozen: a file system block is the least a file takes.
const MIN_COLD_SIZE: u64 = 4096;

/// How many names a file has. Non-unix systems have no trees that link objects.
fn link_count(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        1
    }
}

/// An object's key, the name of its time copy (`<key>.t<seconds>`), or the name of its
/// compressed file (`<key>.zst`).
fn is_object_name(name: &str) -> bool {
    variant_base(name).is_some() || cold_key(name).is_some() || ObjectStore::is_valid_key(name)
}

/// The object key of a compressed file's name; None for any other name.
fn cold_key(name: &str) -> Option<&str> {
    name.strip_suffix(".zst")
        .filter(|key| ObjectStore::is_valid_key(key))
}

/// The contents of an open compressed file, from its start (the header frame is skipped).
fn cold_frames(
    mut file: File,
) -> std::io::Result<super::zstd::ZstdFrames<std::io::BufReader<File>>> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    Ok(super::zstd::ZstdFrames::new(std::io::BufReader::new(file)))
}

/// True when the open compressed file unpacks to as many bytes as its header says, and
/// the bytes hash to the start of `key`.
fn cold_matches(mut file: File, key: &str) -> std::io::Result<bool> {
    let header = super::zstd::read_cold_header(&mut file)?;
    let mut frames = cold_frames(file)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = frames.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let sha: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(size == header.size && key.get(..64) == Some(sha.as_str()))
}

/// The object key of a time copy's name; None for any other name.
fn variant_base(name: &str) -> Option<&str> {
    let (key, t) = name.rsplit_once(".t")?;
    (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) && ObjectStore::is_valid_key(key))
        .then_some(key)
}

/// The SHA-256 of a file's contents, in hex.
fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// A file's modification time in whole seconds since 1970 (None before 1970).
pub(crate) fn mtime_of(meta: &std::fs::Metadata) -> Option<u64> {
    let t = meta.modified().ok()?;
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Set a file's access and modification times to `secs` since 1970.
pub(crate) fn set_mtime(file: &File, secs: u64) -> std::io::Result<()> {
    let t = std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(secs))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("the time {secs} is out of range"),
            )
        })?;
    file.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
}

#[cfg(test)]
thread_local! {
    /// How many times this thread walked an objects directory.
    pub(crate) static WALKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A race point's name, and the step to run there.
#[cfg(test)]
type RaceStep = (&'static str, Box<dyn FnOnce()>);

#[cfg(test)]
thread_local! {
    /// A step a test runs at the named point, once: there it plays another process.
    static RACE: std::cell::RefCell<Option<RaceStep>> = const { std::cell::RefCell::new(None) };
}

/// Where another process can change an object between two steps of this one. Tests use it
/// to run that change at that moment; elsewhere it does nothing.
fn race_point(at: &'static str) {
    #[cfg(test)]
    {
        let hook = RACE.with(|r| {
            let mut r = r.borrow_mut();
            if r.as_ref().is_some_and(|(point, _)| *point == at) {
                r.take()
            } else {
                None
            }
        });
        if let Some((_, step)) = hook {
            step();
        }
    }
    #[cfg(not(test))]
    let _ = at;
}

pub(crate) fn io_error(path: &Path, e: std::io::Error) -> Error {
    Error::new(
        ErrorKind::Runtime,
        "cannot write to the image store",
        format!("{}: {e}", path.display()),
        "check free disk space and the permissions of the podbag data directory",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(uid: u32, mode: u32) -> Meta {
        Meta {
            uid,
            gid: uid,
            mode,
            xattrs: vec![],
        }
    }

    #[test]
    fn key_includes_metadata() {
        let sha = "ab".repeat(32);
        assert_eq!(
            ObjectStore::key(&sha, &meta(0, 0o644)),
            format!("{sha}.0.0.644")
        );
        let with_x = Meta {
            xattrs: vec![("user.a".into(), b"1".to_vec())],
            ..meta(101, 0o4755)
        };
        let k = ObjectStore::key(&sha, &with_x);
        assert!(k.starts_with(&format!("{sha}.101.101.4755.x")), "{k}");
        assert_eq!(k.len(), sha.len() + ".101.101.4755.x".len() + 16);
    }

    #[test]
    fn identical_content_and_metadata_share_one_object() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let (k1, n1) = store
            .insert(&mut &b"same"[..], &meta(0, 0o644), None)
            .unwrap();
        let (k2, _) = store
            .insert(&mut &b"same"[..], &meta(0, 0o644), None)
            .unwrap();
        assert_eq!(k1, k2);
        assert_eq!(n1, 4);
        assert_eq!(store.total_size().unwrap(), (1, 4));
        assert_eq!(std::fs::read(store.path(&k1)).unwrap(), b"same");
    }

    #[test]
    fn different_owner_makes_a_different_object() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let (k1, _) = store.insert(&mut &b"x"[..], &meta(0, 0o644), None).unwrap();
        let (k2, _) = store
            .insert(&mut &b"x"[..], &meta(101, 0o644), None)
            .unwrap();
        assert_ne!(k1, k2);
        assert_eq!(store.total_size().unwrap().0, 2);
    }

    fn mtime(path: &Path) -> u64 {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn a_new_object_gets_its_time() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let (k, _) = store
            .insert(&mut &b"x"[..], &meta(0, 0o644), Some(1_600_000_000))
            .unwrap();
        assert_eq!(mtime(&store.path(&k)), 1_600_000_000);
    }

    /// Trees link the object: its time is theirs. A later file with the same contents but
    /// another time gets a copy of its own (`with_mtime`), never a new time on the object.
    #[test]
    fn a_stored_object_keeps_its_time() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let (k, _) = store
            .insert(&mut &b"x"[..], &meta(0, 0o644), Some(1_600_000_000))
            .unwrap();
        store
            .insert(&mut &b"x"[..], &meta(0, 0o644), Some(1_700_000_000))
            .unwrap();
        assert_eq!(mtime(&store.path(&k)), 1_600_000_000);
    }

    /// After a crash the object's name can be on disk without all of its data. The next
    /// insert of the same contents writes them into the same inode, so every tree that
    /// links it heals too, with its mode and time as before.
    #[test]
    fn an_object_cut_short_is_written_again_in_place() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let m = meta(0, 0o4755);
        let (k, _) = store
            .insert(&mut &b"#!/bin/sh\necho hi\n"[..], &m, Some(1_600_000_000))
            .unwrap();
        let object = store.path(&k);
        let tree_file = tmp.path().join("tree-file");
        std::fs::hard_link(&object, &tree_file).unwrap();
        let ino = std::fs::metadata(&object).unwrap().ino();
        // What a crash leaves: the data cut short, the time set before the rename.
        let cut = std::fs::OpenOptions::new()
            .write(true)
            .open(&object)
            .unwrap();
        cut.set_len(3).unwrap();
        set_mtime(&cut, 1_600_000_000).unwrap();
        drop(cut);

        let (again, size) = store
            .insert(&mut &b"#!/bin/sh\necho hi\n"[..], &m, Some(1_700_000_000))
            .unwrap();
        assert_eq!((again.as_str(), size), (k.as_str(), 18));
        assert_eq!(std::fs::read(&tree_file).unwrap(), b"#!/bin/sh\necho hi\n");
        let after = std::fs::metadata(&object).unwrap();
        assert_eq!(after.ino(), ino);
        assert_eq!(after.permissions().mode() & 0o7777, 0o4755);
        assert_eq!(mtime(&object), 1_600_000_000);
    }

    /// As root the repair puts back what the write took: owner, set-user-ID bit and file
    /// capabilities (the kernel drops capabilities on every write).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn a_repaired_object_keeps_its_owner_and_capabilities_as_root() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), true).unwrap();
        let net_raw = vec![
            1, 0, 0, 2, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let m = Meta {
            uid: 101,
            gid: 102,
            mode: 0o4755,
            xattrs: vec![("security.capability".into(), net_raw.clone())],
        };
        let data = b"#!/bin/sh\necho hi\n";
        let (k, _) = store
            .insert(&mut &data[..], &m, Some(1_600_000_000))
            .unwrap();
        let object = store.path(&k);
        let cut = std::fs::OpenOptions::new()
            .write(true)
            .open(&object)
            .unwrap();
        cut.set_len(3).unwrap();
        set_mtime(&cut, 1_600_000_000).unwrap();
        drop(cut);
        store.insert(&mut &data[..], &m, Some(1)).unwrap();
        let after = std::fs::metadata(&object).unwrap();
        assert_eq!(std::fs::read(&object).unwrap(), data);
        assert_eq!((after.uid(), after.gid()), (101, 102));
        assert_eq!(after.permissions().mode() & 0o7777, 0o4755);
        assert_eq!(mtime(&object), 1_600_000_000);
        let c_path = std::ffi::CString::new(object.as_os_str().as_encoded_bytes()).unwrap();
        let mut value = vec![0u8; 64];
        // SAFETY: a NUL-terminated path and name, and a buffer of the given length.
        let n = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c"security.capability".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert!(n > 0, "the file capability is gone");
        value.truncate(n as usize);
        assert_eq!(value, net_raw);
    }

    /// Read-only files (0444) are repaired too, and keep their mode.
    #[test]
    fn an_object_without_a_write_bit_is_repaired() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let m = meta(0, 0o444);
        let (k, _) = store.insert(&mut &b"read only\n"[..], &m, Some(5)).unwrap();
        let object = store.path(&k);
        std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o644)).unwrap();
        let cut = std::fs::OpenOptions::new()
            .write(true)
            .open(&object)
            .unwrap();
        cut.set_len(4).unwrap();
        set_mtime(&cut, 5).unwrap();
        drop(cut);
        std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o444)).unwrap();
        store.insert(&mut &b"read only\n"[..], &m, Some(5)).unwrap();
        assert_eq!(std::fs::read(&object).unwrap(), b"read only\n");
        let mode = std::fs::metadata(&object).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o444);
    }

    /// A time no clock can hold is an error, never a panic (a panic aborts podbag).
    #[test]
    fn an_impossible_time_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let file = File::create(tmp.path().join("f")).unwrap();
        assert!(set_mtime(&file, u64::MAX).is_err());
    }

    /// A crash leaves temporary files behind, and process IDs repeat after a reboot: a
    /// temporary file of the same name must not stop an insert.
    #[test]
    fn a_leftover_temporary_file_does_not_stop_an_insert() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let leftover = store.dir.join("tmp").join("12345-0");
        std::fs::write(&leftover, b"from before the crash").unwrap();
        let (k, size) = store
            .write_new(&mut &b"new"[..], &meta(0, 0o644), Some(1), &leftover)
            .unwrap();
        assert_eq!(size, 3);
        assert_eq!(std::fs::read(store.path(&k)).unwrap(), b"new");
    }

    #[test]
    fn with_mtime_gives_the_object_when_the_time_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let m = meta(0, 0o644);
        let (k, _) = store.insert(&mut &b"x"[..], &m, Some(100)).unwrap();
        assert_eq!(store.with_mtime(&k, &m, 100).unwrap(), store.path(&k));
    }

    #[test]
    fn with_mtime_copies_the_object_once_for_another_time() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let m = meta(0, 0o750);
        let (k, _) = store.insert(&mut &b"data"[..], &m, Some(100)).unwrap();
        let copy = store.with_mtime(&k, &m, 200).unwrap();
        assert_ne!(copy, store.path(&k));
        assert_eq!(std::fs::read(&copy).unwrap(), b"data");
        assert_eq!(mtime(&copy), 200);
        assert_eq!(mtime(&store.path(&k)), 100);
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        assert_ne!(ino(&copy), ino(&store.path(&k)));
        assert_eq!(
            std::fs::metadata(&copy).unwrap().permissions().mode() & 0o7777,
            0o750
        );
        // The second time, the same copy.
        assert_eq!(store.with_mtime(&k, &m, 200).unwrap(), copy);
        assert_eq!(ino(&store.with_mtime(&k, &m, 200).unwrap()), ino(&copy));
        // Kept by name like the object: a sweep that keeps it does not count it as garbage.
        let keep = HashSet::from([k.clone(), ObjectStore::variant_key(&k, 200)]);
        assert_eq!(store.sweep(&keep).unwrap(), 0);
        assert!(copy.exists());
        let keep = HashSet::from([k.clone()]);
        assert_eq!(store.sweep(&keep).unwrap(), 4);
        assert!(!copy.exists());
    }

    /// A crash can leave a time copy's name on disk without its data: it is made again.
    #[test]
    fn a_time_copy_cut_short_is_made_again() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let m = meta(0, 0o644);
        let (k, _) = store.insert(&mut &b"data"[..], &m, Some(100)).unwrap();
        let copy = store.with_mtime(&k, &m, 200).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_len(0)
            .unwrap();
        let again = store.with_mtime(&k, &m, 200).unwrap();
        assert_eq!(std::fs::read(&again).unwrap(), b"data");
        assert_eq!(mtime(&again), 200);
    }

    /// Damage of the same size (or a copy made from a damaged object) passes the size check:
    /// reading every object finds it. Files that are no objects are not counted.
    #[test]
    fn verify_finds_objects_whose_contents_do_not_match_their_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let m = meta(0, 0o644);
        let (a, _) = store.insert(&mut &b"aaaa"[..], &m, Some(1)).unwrap();
        let (b, _) = store.insert(&mut &b"bb"[..], &m, Some(1)).unwrap();
        let copy = store.with_mtime(&b, &m, 2).unwrap();
        std::fs::write(store.path(&a), b"xxxx").unwrap();
        std::fs::write(&copy, b"yy").unwrap();
        std::fs::write(store.path(&a).with_file_name("stray"), b"?").unwrap();
        let (count, mut damaged) = store.verify().unwrap();
        damaged.sort();
        let mut want = vec![a, ObjectStore::variant_key(&b, 2)];
        want.sort();
        assert_eq!((count, damaged), (3, want));
    }

    #[test]
    fn sweep_removes_unreferenced_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let (a, _) = store
            .insert(&mut &b"aaaa"[..], &meta(0, 0o644), None)
            .unwrap();
        store
            .insert(&mut &b"bb"[..], &meta(0, 0o644), None)
            .unwrap();
        store.insert(&mut &b"c"[..], &meta(0, 0o644), None).unwrap();
        let keep = std::collections::HashSet::from([a.clone()]);
        assert_eq!(store.sweep(&keep).unwrap(), 3);
        assert_eq!(store.total_size().unwrap(), (1, 4));
        assert!(store.path(&a).exists());
    }

    mod cold {
        use super::*;
        use std::os::unix::fs::MetadataExt;

        fn store(tmp: &tempfile::TempDir) -> ObjectStore {
            ObjectStore::new(tmp.path().join("objects"), false).unwrap()
        }

        fn meta(mode: u32) -> Meta {
            Meta {
                uid: 0,
                gid: 0,
                mode,
                xattrs: vec![],
            }
        }

        fn compressible() -> Vec<u8> {
            b"podbag ".repeat(30_000)
        }

        #[test]
        fn freeze_then_thaw_gives_back_the_same_file() {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, size) = s
                .insert(&mut &compressible()[..], &meta(0o640), Some(1234))
                .unwrap();
            let (before, after) = s.freeze(&key).unwrap();
            assert_eq!(before, size);
            assert!(after < before / 4);
            assert!(!s.path(&key).exists() && s.is_cold(&key));
            s.thaw(&key, &meta(0o640)).unwrap();
            assert_eq!(std::fs::read(s.path(&key)).unwrap(), compressible());
            let m = std::fs::metadata(s.path(&key)).unwrap();
            assert_eq!(m.permissions().mode() & 0o7777, 0o640);
            assert_eq!(mtime_of(&m), Some(1234));
            assert!(!s.cold_path(&key).exists());
        }

        #[test]
        fn freeze_skips_an_object_a_tree_links() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            std::fs::hard_link(s.path(&key), tmp.path().join("objects/tmp/x")).unwrap();
            assert_eq!(s.freeze(&key).unwrap(), (0, 0));
            assert_eq!(std::fs::read(s.path(&key)).unwrap(), compressible());
            assert!(!s.cold_path(&key).exists());
        }

        #[test]
        fn freeze_skips_small_objects() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &[7u8; 100][..], &meta(0o644), Some(1))
                .unwrap();
            assert_eq!(s.freeze(&key).unwrap(), (0, 0));
            assert!(s.path(&key).exists() && !s.cold_path(&key).exists());
        }

        #[test]
        fn freeze_skips_a_missing_object() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let key = ObjectStore::key(&"ab".repeat(32), &meta(0o644));
            assert_eq!(s.freeze(&key).unwrap(), (0, 0));
        }

        #[test]
        fn freeze_removes_time_copies() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let m = meta(0o644);
            let (key, _) = s.insert(&mut &compressible()[..], &m, Some(1234)).unwrap();
            let copy = s.with_mtime(&key, &m, 99).unwrap();
            assert!(copy.exists());
            s.freeze(&key).unwrap();
            assert!(!s.path(&ObjectStore::variant_key(&key, 99)).exists());
            assert!(s.is_cold(&key));
        }

        #[test]
        fn freeze_keeps_a_time_copy_a_tree_links() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let m = meta(0o644);
            let (key, _) = s.insert(&mut &compressible()[..], &m, Some(1234)).unwrap();
            let copy = s.with_mtime(&key, &m, 99).unwrap();
            std::fs::hard_link(&copy, tmp.path().join("objects/tmp/x")).unwrap();
            s.freeze(&key).unwrap();
            assert!(copy.exists());
        }

        #[test]
        fn thaw_refuses_a_damaged_copy() {
            use std::io::{Seek, SeekFrom};
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            s.freeze(&key).unwrap();
            let mut cold = std::fs::OpenOptions::new()
                .write(true)
                .open(s.cold_path(&key))
                .unwrap();
            cold.seek(SeekFrom::End(-16)).unwrap();
            cold.write_all(&[0u8; 16]).unwrap();
            drop(cold);
            assert!(s.thaw(&key, &meta(0o644)).is_err());
            assert!(!s.path(&key).exists());
            assert!(s.cold_path(&key).exists());
            // Nothing is left in the temporary directory.
            assert_eq!(
                std::fs::read_dir(tmp.path().join("objects/tmp"))
                    .unwrap()
                    .count(),
                0
            );
        }

        #[test]
        fn thaw_keeps_a_file_that_is_already_there() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            s.freeze(&key).unwrap();
            let (again, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(2))
                .unwrap();
            assert_eq!(again, key);
            let ino = std::fs::metadata(s.path(&key)).unwrap().ino();
            s.thaw(&key, &meta(0o644)).unwrap();
            assert_eq!(std::fs::metadata(s.path(&key)).unwrap().ino(), ino);
            assert!(!s.cold_path(&key).exists());
        }

        #[test]
        fn open_reads_plain_and_compressed_objects() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            let read = |s: &ObjectStore| {
                let mut out = Vec::new();
                s.open(&key).unwrap().read_to_end(&mut out).unwrap();
                out
            };
            assert_eq!(read(&s), compressible());
            s.freeze(&key).unwrap();
            assert!(s.is_cold(&key));
            assert_eq!(read(&s), compressible());
        }

        #[test]
        fn a_failed_freeze_leaves_the_object_plain() {
            use std::os::unix::fs::PermissionsExt;
            // Root ignores directory permissions.
            // SAFETY: geteuid takes no arguments and cannot fail.
            if unsafe { libc::geteuid() } == 0 {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            let tmp_dir = tmp.path().join("objects/tmp");
            std::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            let result = s.freeze(&key);
            std::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(result.is_err());
            assert_eq!(std::fs::read(s.path(&key)).unwrap(), compressible());
            assert!(!s.cold_path(&key).exists());
        }

        fn damage_cold(s: &ObjectStore, key: &str) {
            use std::io::{Seek, SeekFrom};
            let mut cold = std::fs::OpenOptions::new()
                .write(true)
                .open(s.cold_path(key))
                .unwrap();
            cold.seek(SeekFrom::End(-16)).unwrap();
            cold.write_all(&[0u8; 16]).unwrap();
        }

        #[test]
        fn total_size_counts_compressed_bytes() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            s.freeze(&key).unwrap();
            let on_disk = std::fs::metadata(s.cold_path(&key)).unwrap().len();
            assert_eq!(s.total_size().unwrap(), (1, on_disk));
        }

        #[test]
        fn sweep_removes_unreferenced_and_stale_compressed_copies() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (a, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            let other = b"other ".repeat(30_000);
            let (b, _) = s.insert(&mut &other[..], &meta(0o644), Some(1)).unwrap();
            s.freeze(&a).unwrap();
            s.freeze(&b).unwrap();
            // b is plain again, and its compressed copy is stale.
            s.insert(&mut &other[..], &meta(0o644), Some(2)).unwrap();
            std::fs::write(s.cold_path(&b), b"stale").unwrap();
            let keep = HashSet::from([b.clone()]);
            s.sweep(&keep).unwrap();
            assert!(!s.cold_path(&a).exists() && !s.path(&a).exists());
            assert!(s.path(&b).exists());
            assert!(!s.cold_path(&b).exists());
        }

        #[test]
        fn sweep_keeps_a_referenced_compressed_object() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            s.freeze(&key).unwrap();
            s.sweep(&HashSet::from([key.clone()])).unwrap();
            assert!(s.is_cold(&key));
        }

        #[test]
        fn verify_finds_a_damaged_compressed_object() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            s.freeze(&key).unwrap();
            assert_eq!(s.verify().unwrap(), (1, vec![]));
            damage_cold(&s, &key);
            assert_eq!(s.verify().unwrap(), (1, vec![format!("{key}.zst")]));
        }

        #[test]
        fn inserting_frozen_contents_brings_the_object_back() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1234))
                .unwrap();
            s.freeze(&key).unwrap();
            let (again, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(9999))
                .unwrap();
            assert_eq!(again, key);
            let m = std::fs::metadata(s.path(&key)).unwrap();
            assert_eq!(mtime_of(&m), Some(1234));
            assert_eq!(std::fs::read(s.path(&key)).unwrap(), compressible());
            assert!(!s.cold_path(&key).exists());
        }

        /// Run `step` once when this thread reaches the race point `at`.
        fn at_race_point(at: &'static str, step: impl FnOnce() + 'static) {
            RACE.with(|r| *r.borrow_mut() = Some((at, Box::new(step))));
        }

        /// A frozen object in a store that the returned closure can thaw "from another
        /// process" (a second handle on the same directory).
        fn frozen(tmp: &tempfile::TempDir) -> (ObjectStore, String, impl FnOnce() + 'static) {
            let s = store(tmp);
            let (key, _) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            s.freeze(&key).unwrap();
            let other = store(tmp);
            let k = key.clone();
            (s, key, move || other.thaw(&k, &meta(0o644)).unwrap())
        }

        /// Two container starts thaw the same object: the other one links the plain file
        /// and deletes the compressed one between this thaw's check and its open.
        #[test]
        fn a_thaw_that_another_thaw_finished_first_succeeds() {
            let tmp = tempfile::tempdir().unwrap();
            let (s, key, other) = frozen(&tmp);
            at_race_point("thaw", other);
            s.thaw(&key, &meta(0o644)).unwrap();
            assert_eq!(std::fs::read(s.path(&key)).unwrap(), compressible());
            assert!(!s.cold_path(&key).exists());
        }

        /// The other thaw links the plain file while this one has its own copy ready: the
        /// file that is there stays (trees may link it), and the copy goes.
        #[test]
        fn a_thaw_keeps_the_plain_file_another_thaw_linked_first() {
            let tmp = tempfile::tempdir().unwrap();
            let (s, key, other) = frozen(&tmp);
            let path = s.path(&key);
            let ino = std::rc::Rc::new(std::cell::Cell::new(0));
            let seen = ino.clone();
            at_race_point("thaw-link", move || {
                other();
                seen.set(std::fs::metadata(&path).unwrap().ino());
            });
            s.thaw(&key, &meta(0o644)).unwrap();
            assert_eq!(std::fs::metadata(s.path(&key)).unwrap().ino(), ino.get());
            assert!(!s.cold_path(&key).exists());
            assert_eq!(
                std::fs::read_dir(tmp.path().join("objects/tmp"))
                    .unwrap()
                    .count(),
                0
            );
        }

        /// `save` reads an object while a container start thaws it.
        #[test]
        fn open_reads_an_object_another_thaw_made_plain() {
            let tmp = tempfile::tempdir().unwrap();
            let (s, key, other) = frozen(&tmp);
            at_race_point("open", other);
            let mut out = Vec::new();
            s.open(&key).unwrap().read_to_end(&mut out).unwrap();
            assert_eq!(out, compressible());
        }

        /// `system verify` (shared lock) checks a compressed file while a container start
        /// thaws it: the file is not damaged.
        #[test]
        fn a_compressed_file_another_thaw_deletes_is_not_damaged() {
            let tmp = tempfile::tempdir().unwrap();
            let (s, key, other) = frozen(&tmp);
            at_race_point("damaged", other);
            assert!(!s.is_damaged(&format!("{key}.zst")).unwrap());
        }

        /// The same race with real threads: many thaws of one object at once all succeed.
        #[test]
        fn concurrent_thaws_of_one_object_all_succeed() {
            for _ in 0..20 {
                let tmp = tempfile::tempdir().unwrap();
                let (s, key, _) = frozen(&tmp);
                let s = std::sync::Arc::new(s);
                let threads: Vec<_> = (0..4)
                    .map(|_| {
                        let (s, key) = (s.clone(), key.clone());
                        std::thread::spawn(move || s.thaw(&key, &meta(0o644)))
                    })
                    .collect();
                for t in threads {
                    t.join().unwrap().unwrap();
                }
                assert_eq!(std::fs::read(s.path(&key)).unwrap(), compressible());
                assert!(!s.cold_path(&key).exists());
            }
        }

        #[test]
        fn cold_stats_reports_original_and_stored_bytes() {
            let tmp = tempfile::tempdir().unwrap();
            let s = store(&tmp);
            assert_eq!(s.cold_stats().unwrap(), ColdStats::default());
            let (key, size) = s
                .insert(&mut &compressible()[..], &meta(0o644), Some(1))
                .unwrap();
            s.freeze(&key).unwrap();
            let on_disk = std::fs::metadata(s.cold_path(&key)).unwrap().len();
            assert_eq!(
                s.cold_stats().unwrap(),
                ColdStats {
                    objects: 1,
                    bytes: on_disk,
                    original_bytes: size
                }
            );
        }
    }
}
