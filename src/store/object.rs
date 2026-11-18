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

    /// Where an object lives below the objects directory: `ab/ab…`.
    pub fn relative_path(key: &str) -> String {
        format!("{}/{key}", &key[..2])
    }

    /// Store `data` with `meta`. Returns the object key and the content size.
    /// Content that is already stored with the same metadata is not written again.
    pub fn insert(&self, data: &mut dyn Read, meta: &Meta) -> Result<(String, u64)> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let tmp = self.dir.join("tmp").join(format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = self.write_new(data, meta, &tmp);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    fn write_new(&self, data: &mut dyn Read, meta: &Meta, tmp: &Path) -> Result<(String, u64)> {
        let fail = |e: std::io::Error| io_error(tmp, e);
        let mut file = File::create(tmp).map_err(fail)?;
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
        if dest.exists() {
            drop(file);
            let _ = std::fs::remove_file(tmp);
            return Ok((key, size));
        }
        if self.set_owner {
            apply_owner(&file, meta).map_err(fail)?;
        }
        // After chown: chown clears set-user-ID and set-group-ID bits.
        super::perm::set_file_mode(&file, meta.mode).map_err(fail)?;
        drop(file);
        std::fs::create_dir_all(dest.parent().expect("object path has a parent")).map_err(fail)?;
        std::fs::rename(tmp, &dest).map_err(fail)?;
        Ok((key, size))
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

pub(crate) fn io_error(path: &Path, e: std::io::Error) -> Error {
    Error::new(
        ErrorKind::Runtime,
        "cannot write to the image store",
        format!("{}: {e}", path.display()),
        "check free disk space and the permissions of the openpod data directory",
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
        let (k1, n1) = store.insert(&mut &b"same"[..], &meta(0, 0o644)).unwrap();
        let (k2, _) = store.insert(&mut &b"same"[..], &meta(0, 0o644)).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(n1, 4);
        assert_eq!(store.total_size().unwrap(), (1, 4));
        assert_eq!(std::fs::read(store.path(&k1)).unwrap(), b"same");
    }

    #[test]
    fn different_owner_makes_a_different_object() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let (k1, _) = store.insert(&mut &b"x"[..], &meta(0, 0o644)).unwrap();
        let (k2, _) = store.insert(&mut &b"x"[..], &meta(101, 0o644)).unwrap();
        assert_ne!(k1, k2);
        assert_eq!(store.total_size().unwrap().0, 2);
    }

    #[test]
    fn sweep_removes_unreferenced_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let (a, _) = store.insert(&mut &b"aaaa"[..], &meta(0, 0o644)).unwrap();
        store.insert(&mut &b"bb"[..], &meta(0, 0o644)).unwrap();
        store.insert(&mut &b"c"[..], &meta(0, 0o644)).unwrap();
        let keep = std::collections::HashSet::from([a.clone()]);
        assert_eq!(store.sweep(&keep).unwrap(), 3);
        assert_eq!(store.total_size().unwrap(), (1, 4));
        assert!(store.path(&a).exists());
    }
}
