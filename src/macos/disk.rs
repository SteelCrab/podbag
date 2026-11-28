//! Offline storage changes for the macOS VM. The host lock excludes boot and other edits.

use super::host::Paths;
use crate::vm::DiskStorage as Status;
use crate::vm::ext4;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const LOCATION: &str = "vm-disk-location.json";
const GIB: u64 = 1 << 30;
const MAX_GIB: u64 = 1024;

fn explain(path: &Path, error: impl std::fmt::Display) -> String {
    format!("{}: {error}", path.display())
}

fn configured(home: &Path) -> Result<Option<PathBuf>, String> {
    let file = home.join(LOCATION);
    let data = match fs::read(&file) {
        Ok(data) => data,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(explain(&file, error)),
    };
    let path: PathBuf = serde_json::from_slice(&data).map_err(|e| explain(&file, e))?;
    if !path.is_absolute() {
        return Err(explain(&file, "the configured disk path is not absolute"));
    }
    Ok(Some(path))
}

// Older installs may have moved the default image by making it a symlink. Resolve an
// existing target before opening it. A broken link is never a fresh installation.
fn default_image(p: &Paths) -> Result<PathBuf, String> {
    match fs::symlink_metadata(&p.disk) {
        Ok(meta) if meta.file_type().is_symlink() => {
            p.disk.canonicalize().map_err(|e| explain(&p.disk, e))
        }
        Ok(_) => Ok(p.disk.clone()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(p.disk.clone()),
        Err(error) => Err(explain(&p.disk, error)),
    }
}

fn open_disk(path: &Path, write: bool) -> io::Result<File> {
    let disk = OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !disk.metadata()?.is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "expected a regular disk image file",
        ));
    }
    Ok(disk)
}

pub(super) fn status(p: &Paths) -> Result<Status, String> {
    let saved = configured(&p.home)?;
    let configured = saved.is_some();
    let path = match saved {
        Some(path) => path,
        None => default_image(p)?,
    };
    let (exists, size_bytes, allocated_bytes, filesystem_bytes) = match open_disk(&path, false) {
        Ok(disk) => {
            let meta = disk.metadata().map_err(|e| explain(&path, e))?;
            let fs_bytes = ext4::geometry(&disk)
                .ok()
                .filter(|geometry| {
                    geometry.bytes <= meta.len() && meta.len() % geometry.block_size == 0
                })
                .map(|geometry| geometry.bytes);
            (
                true,
                meta.len(),
                meta.blocks().saturating_mul(512),
                fs_bytes,
            )
        }
        Err(error) if error.kind() == ErrorKind::NotFound => (false, 0, 0, None),
        Err(error) => return Err(explain(&path, error)),
    };
    Ok(Status {
        path,
        exists,
        configured,
        editable: false,
        size_bytes,
        allocated_bytes,
        filesystem_bytes,
        growth_pending: filesystem_bytes.is_some_and(|size| size < size_bytes),
        max_size_gib: MAX_GIB,
        backup_path: None,
    })
}

fn verify_location(p: &Paths, path: &Path) -> Result<(), String> {
    if configured(&p.home)?.is_some() && resolve_destination(path)? != path {
        return Err(explain(path, "the configured disk's directory has moved"));
    }
    Ok(())
}

/// Resolve the data disk only after the host takes its lock. An unavailable configured
/// disk must never silently become a new empty default disk.
pub(super) fn for_boot(p: &Paths) -> Result<PathBuf, String> {
    if let Some(path) = configured(&p.home)? {
        if resolve_destination(&path)? != path {
            return Err(explain(&path, "the configured disk's directory has moved"));
        }
        open_disk(&path, false).map_err(|e| explain(&path, e))?;
        return Ok(path);
    }
    let path = default_image(p)?;
    match open_disk(&path, false) {
        Ok(_) => return Ok(path),
        Err(error) if error.kind() == ErrorKind::NotFound => (),
        Err(error) => return Err(explain(&path, error)),
    }
    let staging = tempfile::tempdir_in(&p.home).map_err(|e| explain(&p.home, e))?;
    let prepared = staging.path().join("disk.img");
    let template = p.guest.join("disk.zst");
    let mut from = File::open(&template).map_err(|e| explain(&template, e))?;
    crate::vm::template::expand(&mut from, &prepared).map_err(|e| e.to_string())?;
    publish(&prepared, &path).map_err(|e| explain(&path, e))?;
    Ok(path)
}

pub(super) fn open_host_lock(p: &Paths) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&p.lock)
        .map_err(|e| explain(&p.lock, e))
}

fn stopped_lock(p: &Paths) -> Result<File, String> {
    super::host::make_run_dir(p)?;
    let held = open_host_lock(p)?;
    // SAFETY: flock on our own open descriptor. The host uses the same lock during boot.
    if unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == ErrorKind::WouldBlock {
            return Err("stop the VM before changing its storage (podbag vm stop)".into());
        }
        return Err(explain(&p.lock, error));
    }
    Ok(held)
}

pub(super) fn grow(p: &Paths, gib: u64) -> Result<Status, String> {
    let _lock = stopped_lock(p)?;
    let before = status(p)?;
    verify_location(p, &before.path)?;
    let target = gib
        .checked_mul(GIB)
        .filter(|_| gib <= MAX_GIB)
        .ok_or_else(|| format!("the capacity must be at most {MAX_GIB} GiB"))?;
    if target < before.size_bytes {
        return Err("the VM disk can only grow".into());
    }
    if before.filesystem_bytes.is_none() {
        return Err("start the VM to create its ext4 disk before changing capacity".into());
    }
    if target == before.size_bytes {
        return Ok(before);
    }
    let disk = open_disk(&before.path, true).map_err(|e| explain(&before.path, e))?;
    let directory = before
        .path
        .parent()
        .ok_or("the VM disk has no parent directory")?;
    let backup = tempfile::Builder::new()
        .prefix("podbag-disk-backup-")
        .tempdir_in(directory)
        .map_err(|e| explain(directory, e))?;
    let backup_file = backup.path().join("disk.img");
    copy_disk(&disk, backup.path()).map_err(|e| explain(&backup_file, e))?;
    sync_dir(backup.path())
        .and_then(|()| sync_dir(directory))
        .map_err(|e| explain(&backup_file, e))?;
    let backup_path = backup.keep().join("disk.img");
    disk.set_len(target)
        .and_then(|()| disk.sync_all())
        .map_err(|e| {
            format!(
                "{} (backup kept at {})",
                explain(&before.path, e),
                backup_path.display()
            )
        })?;
    let mut after = status(p)?;
    after.backup_path = Some(backup_path);
    Ok(after)
}

fn resolve_destination(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("give an absolute path for the new VM disk image".into());
    }
    let name = path
        .file_name()
        .ok_or("give a filename for the new VM disk image")?;
    let parent = path
        .parent()
        .ok_or("the new disk has no parent directory")?;
    let directory = parent.canonicalize().map_err(|e| explain(parent, e))?;
    if !directory.is_dir() {
        return Err(explain(
            &directory,
            "the destination parent is not a directory",
        ));
    }
    Ok(directory.join(name))
}

pub(super) fn relocate(p: &Paths, path: &Path) -> Result<Status, String> {
    let _lock = stopped_lock(p)?;
    let before = status(p)?;
    verify_location(p, &before.path)?;
    if before.filesystem_bytes.is_none() {
        return Err("start the VM to create its ext4 disk before relocating it".into());
    }
    let target = resolve_destination(path)?;
    // Keep the same no-overwrite rule even when the destination is a dangling symlink.
    match fs::symlink_metadata(&target) {
        Ok(_) => return Err(explain(&target, "the destination already exists")),
        Err(error) if error.kind() == ErrorKind::NotFound => (),
        Err(error) => return Err(explain(&target, error)),
    }
    let directory = target
        .parent()
        .ok_or("the new disk has no parent directory")?;
    let source = open_disk(&before.path, false).map_err(|e| explain(&before.path, e))?;
    let staging = tempfile::tempdir_in(directory).map_err(|e| explain(directory, e))?;
    copy_disk(&source, staging.path()).map_err(|e| explain(&target, e))?;
    publish(&staging.path().join("disk.img"), &target).map_err(|e| explain(&target, e))?;

    let setting = p.home.join(LOCATION);
    let save = || -> io::Result<()> {
        let mut out = tempfile::NamedTempFile::new_in(&p.home)?;
        serde_json::to_writer(&mut out, &target)?;
        out.write_all(b"\n")?;
        out.as_file().sync_all()?;
        out.persist(&setting)?;
        sync_dir(&p.home)
    };
    // Both complete copies are kept even if publishing the new selection fails.
    save().map_err(|e| explain(&setting, e))?;
    let mut after = status(p)?;
    after.backup_path = Some(before.path);
    Ok(after)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn publish(staged: &Path, target: &Path) -> io::Result<()> {
    let file = File::open(staged)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.sync_all()?;
    fs::hard_link(staged, target)?;
    sync_dir(target.parent().ok_or(ErrorKind::InvalidInput)?)
}

/// APFS clones share existing blocks. On another volume, copy and verify just the
/// allocated extents. The destination is always in a private temporary directory.
fn copy_disk(source: &File, directory: &Path) -> io::Result<()> {
    let dir = File::open(directory)?;
    let path = directory.join("disk.img");
    let original = source.metadata()?;
    // SAFETY: both descriptors are open and the destination is a NUL-terminated
    // filename relative to our private directory. clonefile does not replace a file.
    if unsafe { libc::fclonefileat(source.as_raw_fd(), dir.as_raw_fd(), c"disk.img".as_ptr(), 0) }
        == 0
    {
        let cloned = File::open(&path)?;
        // clonefile can preserve the original's mode. Recovery copies are private.
        cloned.set_permissions(fs::Permissions::from_mode(0o600))?;
        if cloned.metadata()?.len() != original.len() {
            return Err(io::Error::other("the cloned disk has an unexpected size"));
        }
        return cloned.sync_all();
    }
    let target = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    target.set_len(original.len())?;
    copy_sparse(source, &target, &original)
}

fn copy_sparse(source: &File, target: &File, original: &fs::Metadata) -> io::Result<()> {
    let mut data = vec![0; 1024 * 1024];
    extents(source, original.len(), |offset, n| {
        source.read_exact_at(&mut data[..n], offset)?;
        target.write_all_at(&data[..n], offset)
    })?;
    target.sync_all()?;
    let mut verify = vec![0; data.len()];
    extents(source, original.len(), |offset, n| {
        source.read_exact_at(&mut data[..n], offset)?;
        target.read_exact_at(&mut verify[..n], offset)?;
        if data[..n] == verify[..n] {
            Ok(())
        } else {
            Err(io::Error::other(
                "the copied disk does not match the original",
            ))
        }
    })?;
    let current = source.metadata()?;
    if current.len() != original.len() || current.modified()? != original.modified()? {
        return Err(io::Error::other("the source disk changed during the copy"));
    }
    Ok(())
}

fn extents(
    source: &File,
    len: u64,
    mut visit: impl FnMut(u64, usize) -> io::Result<()>,
) -> io::Result<()> {
    let seek = |offset: u64, kind| {
        let offset = i64::try_from(offset).map_err(|_| ErrorKind::InvalidInput)?;
        // SAFETY: lseek changes only the current offset of this open descriptor.
        let result = unsafe { libc::lseek(source.as_raw_fd(), offset, kind) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as u64)
        }
    };
    let mut offset = 0;
    while offset < len {
        let start = match seek(offset, libc::SEEK_DATA) {
            Ok(start) if start >= offset && start < len => start,
            Ok(start) if start == len => break,
            Ok(_) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "invalid data extent",
                ));
            }
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => break,
            Err(error) => return Err(error),
        };
        let end = seek(start, libc::SEEK_HOLE)?.min(len);
        if end <= start {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid hole offset",
            ));
        }
        offset = start;
        while offset < end {
            let n = (end - offset).min(1024 * 1024) as usize;
            visit(offset, n)?;
            offset += n as u64;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Paths, File) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let p = super::super::host::paths_in(home);
        let disk = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(p.home.join("disk.img"))
            .unwrap();
        disk.set_len(128 << 20).unwrap();
        let mut superblock = [0u8; 1024];
        superblock[4..8].copy_from_slice(&(32_768u32).to_le_bytes());
        superblock[0x18..0x1c].copy_from_slice(&2u32.to_le_bytes());
        superblock[0x38..0x3a].copy_from_slice(&[0x53, 0xef]);
        disk.write_all_at(&superblock, 1024).unwrap();
        disk.write_all_at(b"kept data", 100 << 20).unwrap();
        (root, p, disk)
    }

    #[test]
    fn the_sparse_fallback_copies_data_and_holes() {
        let (root, _, source) = fixture();
        let target = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(root.path().join("copy"))
            .unwrap();
        let original = source.metadata().unwrap();
        target.set_len(original.len()).unwrap();
        copy_sparse(&source, &target, &original).unwrap();
        let mut bytes = [0; 9];
        target.read_exact_at(&mut bytes, 100 << 20).unwrap();
        assert_eq!(&bytes, b"kept data");
        target.read_exact_at(&mut bytes, 32 << 20).unwrap();
        assert_eq!(bytes, [0; 9]);
        assert_eq!(target.metadata().unwrap().len(), original.len());
        assert!(target.metadata().unwrap().blocks() * 512 < original.len() / 4);
    }

    #[test]
    fn an_existing_default_symlink_still_boots_its_original_data() {
        use std::os::unix::fs::symlink;
        let (root, p, _) = fixture();
        let old = root.path().join("old.img");
        fs::rename(&p.disk, &old).unwrap();
        symlink(&old, &p.disk).unwrap();
        assert_eq!(for_boot(&p).unwrap(), old.canonicalize().unwrap());
        fs::rename(&old, root.path().join("unavailable")).unwrap();
        assert!(for_boot(&p).is_err());
        assert!(
            fs::symlink_metadata(p.disk)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn failure_to_save_the_new_location_leaves_both_complete_disks() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (root, p, source) = fixture();
        super::super::host::make_run_dir(&p).unwrap();
        let original_mode = fs::metadata(&p.home).unwrap().permissions();
        fs::set_permissions(&p.home, fs::Permissions::from_mode(0o500)).unwrap();
        let target = root.path().join("copied.img");
        let result = relocate(&p, &target);
        fs::set_permissions(&p.home, original_mode).unwrap();

        assert!(result.err().unwrap().contains(LOCATION));
        assert_eq!(for_boot(&p).unwrap(), p.home.join("disk.img"));
        let written = File::open(&target).unwrap();
        assert_eq!(
            written.metadata().unwrap().len(),
            source.metadata().unwrap().len()
        );
        assert_eq!(written.metadata().unwrap().mode() & 0o777, 0o600);
        let mut bytes = [0; 9];
        written.read_exact_at(&mut bytes, 100 << 20).unwrap();
        assert_eq!(&bytes, b"kept data");
    }
}
