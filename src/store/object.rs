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
        if self.set_owner {
            apply_owner(&file, meta).map_err(fail)?;
        }
        // After chown: chown clears set-user-ID and set-group-ID bits.
        super::perm::set_file_mode(&file, meta.mode).map_err(fail)?;
        if let Some(t) = mtime {
            set_mtime(&file, t).map_err(fail)?;
        }
        drop(file);
        std::fs::create_dir_all(dest.parent().expect("object path has a parent")).map_err(fail)?;
        std::fs::rename(tmp, &dest).map_err(fail)?;
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

    /// Number of objects and their total content size.
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

    /// Delete every object whose key is not in `keep`. Returns the bytes freed.
    pub fn sweep(&self, keep: &HashSet<String>) -> Result<u64> {
        let mut freed = 0;
        let mut doomed = Vec::new();
        self.for_each(|key, size| {
            if !keep.contains(key) {
                doomed.push(key.to_string());
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

/// An object's key, or the name of its time copy (`<key>.t<seconds>`).
fn is_object_name(name: &str) -> bool {
    variant_base(name).is_some() || ObjectStore::is_valid_key(name)
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
}
