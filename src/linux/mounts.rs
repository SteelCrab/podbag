//! A container's own mounts (`-v`, `--tmpfs`, image volumes), made in the container process
//! before `pivot_root`.
//!
//! The image is untrusted: a symbolic link at a mount target may point anywhere. Targets are
//! therefore resolved inside the root filesystem (`openat2` with `RESOLVE_IN_ROOT`), missing
//! parts are created relative to directory descriptors, and mounts are attached to the
//! target's descriptor (`open_tree`, `fsmount`, `move_mount`), never to a path string.

use crate::volume::{Mount, MountSource};
use crate::{Error, ErrorKind, Result};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags};
use rustix::mount::{FsMountFlags, FsOpenFlags, MountAttrFlags, MoveMountFlags, OpenTreeFlags};
use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

/// Make every mount under `root` (the container's root, not yet entered), parents first.
pub(super) fn mount_all(rootless: bool, root: &Path, mounts: &[Mount]) -> Result<()> {
    if mounts.is_empty() {
        return Ok(());
    }
    let root_fd = rustix::fs::open(
        root,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| fail("/", e.into()))?;
    let mut ordered: Vec<&Mount> = mounts.iter().collect();
    ordered.sort_by_key(|m| m.target.split('/').filter(|p| !p.is_empty()).count());
    for m in ordered {
        mount_one(root_fd.as_fd(), m).map_err(|e| {
            super::userns_denial(rootless, Some(&e), None).unwrap_or_else(|| fail(&m.target, e))
        })?;
    }
    Ok(())
}

fn mount_one(root: BorrowedFd<'_>, m: &Mount) -> std::io::Result<()> {
    match &m.source {
        MountSource::Bind { path } => {
            let is_file = !std::fs::metadata(path)?.is_dir();
            let target = open_target(root, &m.target, is_file)?;
            bind(Path::new(path), target.as_fd(), m.read_only)
        }
        MountSource::Volume { path, .. } => {
            let target = open_target(root, &m.target, false)?;
            if m.copy_up {
                copy_up(target.as_fd(), Path::new(path))?;
            }
            bind(Path::new(path), target.as_fd(), m.read_only)
        }
        MountSource::Tmpfs { options } => {
            let target = open_target(root, &m.target, false)?;
            tmpfs(options, m.read_only, target.as_fd())
        }
    }
}

fn fail(target: &str, e: std::io::Error) -> Error {
    Error::new(
        ErrorKind::Runtime,
        format!("cannot mount {target} in the container"),
        e.to_string(),
        "check the source path and that the target can be a directory in the image",
    )
}

/// How many symbolic links one target may pass through, like the kernel's limit.
const MAX_LINKS: u32 = 40;

/// Open the target inside `root`, creating what is missing (directories, or an empty file
/// as the last part when `file`). Symbolic links are followed as the container will see
/// them: an absolute link starts again at `root`, and `..` never leaves it.
fn open_target(root: BorrowedFd<'_>, target: &str, file: bool) -> std::io::Result<OwnedFd> {
    let mut todo: VecDeque<String> = target
        .split('/')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    // The resolved path so far: real directories only.
    let mut done: Vec<String> = Vec::new();
    let mut links = 0;
    while let Some(part) = todo.pop_front() {
        let last = todo.is_empty();
        match part.as_str() {
            "." => continue,
            ".." => {
                done.pop();
                continue;
            }
            _ => {}
        }
        let parent = open_resolved(root, &done, true)?;
        match rustix::fs::statat(&parent, part.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => match FileType::from_raw_mode(st.st_mode) {
                FileType::Symlink => {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(rustix::io::Errno::LOOP.into());
                    }
                    let to = rustix::fs::readlinkat(&parent, part.as_str(), Vec::new())?;
                    let to = to.to_string_lossy().into_owned();
                    if to.starts_with('/') {
                        done.clear();
                    }
                    for p in to.split('/').filter(|p| !p.is_empty()).rev() {
                        todo.push_front(p.to_string());
                    }
                    continue;
                }
                FileType::Directory => {}
                _ if last && file => {}
                _ => return Err(rustix::io::Errno::NOTDIR.into()),
            },
            Err(rustix::io::Errno::NOENT) => {
                if last && file {
                    rustix::fs::openat(
                        &parent,
                        part.as_str(),
                        OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC,
                        Mode::from_raw_mode(0o644),
                    )?;
                } else {
                    match rustix::fs::mkdirat(&parent, part.as_str(), Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
        done.push(part);
        if matches!(done[0].as_str(), "proc" | "sys") {
            return Err(refused(target, &done));
        }
    }
    if done.is_empty() {
        return Err(refused(target, &done));
    }
    let fd = open_resolved(root, &done, !file)?;
    // The names were checked; also check what the descriptor really is.
    const PROC_SUPER_MAGIC: i64 = 0x9fa0;
    const SYSFS_MAGIC: i64 = 0x6265_6572;
    let fs = rustix::fs::fstatfs(&fd)?;
    if matches!(fs.f_type as i64, PROC_SUPER_MAGIC | SYSFS_MAGIC) {
        return Err(refused(target, &done));
    }
    Ok(fd)
}

/// A target whose links lead to the root, /proc or /sys, which cannot hold a mount.
fn refused(target: &str, resolved: &[String]) -> std::io::Error {
    std::io::Error::other(format!(
        "{target} leads to /{} in the image, which cannot hold a mount",
        resolved.join("/")
    ))
}

/// Open a path that has no symbolic links left, inside `root` (both checked by the kernel).
fn open_resolved(root: BorrowedFd<'_>, parts: &[String], dir: bool) -> std::io::Result<OwnedFd> {
    let path = if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    };
    let mut flags = OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    if dir {
        flags |= OFlags::DIRECTORY;
    }
    let resolve = ResolveFlags::IN_ROOT | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    Ok(rustix::fs::openat2(
        root,
        path.as_str(),
        flags,
        Mode::empty(),
        resolve,
    )?)
}

/// Bind `source` (recursively) onto the target descriptor, read-only if asked.
fn bind(source: &Path, target: BorrowedFd<'_>, read_only: bool) -> std::io::Result<()> {
    let tree = rustix::mount::open_tree(
        rustix::fs::CWD,
        source,
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_RECURSIVE,
    )?;
    if read_only {
        set_read_only(tree.as_fd())?;
    }
    attach(tree.as_fd(), target)
}

/// A new tmpfs with `options` (file system options and mount flags), attached to the target.
fn tmpfs(options: &str, read_only: bool, target: BorrowedFd<'_>) -> std::io::Result<()> {
    let fs = rustix::mount::fsopen("tmpfs", FsOpenFlags::FSOPEN_CLOEXEC)?;
    // Shown as the device in /proc/mounts, as with Docker.
    rustix::mount::fsconfig_set_string(&fs, "source", "tmpfs")?;
    let mut attrs = MountAttrFlags::empty();
    if read_only {
        attrs |= MountAttrFlags::MOUNT_ATTR_RDONLY;
    }
    for option in options.split(',').filter(|o| !o.is_empty()) {
        match option {
            "ro" => attrs |= MountAttrFlags::MOUNT_ATTR_RDONLY,
            "rw" => attrs -= MountAttrFlags::MOUNT_ATTR_RDONLY,
            "nosuid" => attrs |= MountAttrFlags::MOUNT_ATTR_NOSUID,
            "suid" => attrs -= MountAttrFlags::MOUNT_ATTR_NOSUID,
            "nodev" => attrs |= MountAttrFlags::MOUNT_ATTR_NODEV,
            "dev" => attrs -= MountAttrFlags::MOUNT_ATTR_NODEV,
            "noexec" => attrs |= MountAttrFlags::MOUNT_ATTR_NOEXEC,
            "exec" => attrs -= MountAttrFlags::MOUNT_ATTR_NOEXEC,
            _ => match option.split_once('=') {
                Some((key, value)) => rustix::mount::fsconfig_set_string(&fs, key, value)?,
                None => return Err(rustix::io::Errno::INVAL.into()),
            },
        }
    }
    rustix::mount::fsconfig_create(&fs)?;
    let mount = rustix::mount::fsmount(&fs, FsMountFlags::FSMOUNT_CLOEXEC, attrs)?;
    attach(mount.as_fd(), target)
}

fn attach(mount: BorrowedFd<'_>, target: BorrowedFd<'_>) -> std::io::Result<()> {
    Ok(rustix::mount::move_mount(
        mount,
        "",
        target,
        "",
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
    )?)
}

/// mount_setattr(AT_EMPTY_PATH | AT_RECURSIVE, MOUNT_ATTR_RDONLY) on a detached tree.
fn set_read_only(tree: BorrowedFd<'_>) -> std::io::Result<()> {
    #[repr(C)]
    struct MountAttr {
        attr_set: u64,
        attr_clr: u64,
        propagation: u64,
        userns_fd: u64,
    }
    let attr = MountAttr {
        attr_set: 0x1, // MOUNT_ATTR_RDONLY
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: a valid descriptor, an empty NUL-terminated path, and a correctly sized struct.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            c"".as_ptr(),
            (libc::AT_EMPTY_PATH | libc::AT_RECURSIVE) as libc::c_uint,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Fill an empty volume with the image's files at the target (Docker's first-use copy):
/// owners, modes and symbolic links are kept; device files and sockets are skipped.
/// Another container may share the volume and plant links in it at any moment, so the
/// copy works through directory descriptors and never follows or overwrites anything
/// in the volume.
fn copy_up(target: BorrowedFd<'_>, volume: &Path) -> std::io::Result<()> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let to = rustix::fs::open(volume, flags, Mode::empty())?;
    if has_entries(to.as_fd())? {
        return Ok(()); // Filled meanwhile, or not new.
    }
    let from = rustix::fs::openat(target, ".", flags, Mode::empty())?;
    copy_tree(from.as_fd(), to.as_fd())?;
    let st = rustix::fs::fstat(&from)?;
    set_owner_and_mode(to.as_fd(), &st)
}

fn has_entries(dir: BorrowedFd<'_>) -> std::io::Result<bool> {
    let mut entries = rustix::fs::Dir::read_from(dir)?;
    while let Some(entry) = entries.read() {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Copy the contents of directory `from` into directory `to`. A name that already exists
/// in `to` is skipped, not entered or overwritten.
fn copy_tree(from: BorrowedFd<'_>, to: BorrowedFd<'_>) -> std::io::Result<()> {
    use rustix::io::Errno;
    let open_dir = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut entries = rustix::fs::Dir::read_from(from)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        let st = rustix::fs::statat(from, name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(st.st_mode) {
            FileType::Symlink => {
                let link = rustix::fs::readlinkat(from, name, Vec::new())?;
                match rustix::fs::symlinkat(link.as_c_str(), to, name) {
                    Err(Errno::EXIST) => continue,
                    r => r?,
                }
                // SAFETY: a valid directory descriptor and a NUL-terminated name.
                let rc = unsafe {
                    libc::fchownat(
                        to.as_raw_fd(),
                        name.as_ptr(),
                        st.st_uid,
                        st.st_gid,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if rc != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            FileType::Directory => {
                match rustix::fs::mkdirat(to, name, Mode::from_raw_mode(0o700)) {
                    Err(Errno::EXIST) => continue,
                    r => r?,
                }
                let sub_from = rustix::fs::openat(from, name, open_dir, Mode::empty())?;
                let sub_to = rustix::fs::openat(to, name, open_dir, Mode::empty())?;
                copy_tree(sub_from.as_fd(), sub_to.as_fd())?;
                set_owner_and_mode(sub_to.as_fd(), &st)?;
            }
            FileType::RegularFile => {
                let create = OFlags::CREATE
                    | OFlags::EXCL
                    | OFlags::WRONLY
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC;
                let dst = match rustix::fs::openat(to, name, create, Mode::from_raw_mode(0o600)) {
                    Err(Errno::EXIST) => continue,
                    r => r?,
                };
                let src = rustix::fs::openat(
                    from,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                let mut out = std::fs::File::from(dst);
                std::io::copy(&mut std::fs::File::from(src), &mut out)?;
                set_owner_and_mode(out.as_fd(), &st)?;
            }
            _ => continue,
        }
    }
    Ok(())
}

/// Owner first, then mode: changing the owner clears set-user-ID bits.
fn set_owner_and_mode(fd: BorrowedFd<'_>, st: &rustix::fs::Stat) -> std::io::Result<()> {
    // SAFETY: plain system calls on a valid descriptor.
    unsafe {
        if libc::fchown(fd.as_raw_fd(), st.st_uid, st.st_gid) != 0
            || libc::fchmod(fd.as_raw_fd(), st.st_mode & 0o7777) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn open_dir(path: &Path) -> OwnedFd {
        rustix::fs::open(path, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap()
    }

    /// Another container sharing the volume planted links where the copy will write.
    #[test]
    fn copy_does_not_follow_links_in_the_volume() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(src.join("name"), "img").unwrap();
        std::fs::write(src.join("d/f"), "inner").unwrap();
        std::fs::set_permissions(src.join("name"), std::fs::Permissions::from_mode(0o4755))
            .unwrap();
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, "orig").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        let outdir = tmp.path().join("outdir");
        std::fs::create_dir(&outdir).unwrap();
        std::os::unix::fs::symlink(&outside, dst.join("name")).unwrap();
        std::os::unix::fs::symlink(&outdir, dst.join("d")).unwrap();

        copy_tree(open_dir(&src).as_fd(), open_dir(&dst).as_fd()).unwrap();

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "orig");
        let mode = std::fs::metadata(&outside).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        assert!(!outdir.join("f").exists(), "wrote through a planted link");
    }

    #[test]
    fn copy_keeps_files_links_and_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(src.join("d/f"), "inner").unwrap();
        std::fs::set_permissions(src.join("d/f"), std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink("d/f", src.join("link")).unwrap();

        copy_tree(open_dir(&src).as_fd(), open_dir(&dst).as_fd()).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("d/f")).unwrap(), "inner");
        let mode = std::fs::metadata(dst.join("d/f"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o640);
        assert_eq!(
            std::fs::read_link(dst.join("link")).unwrap(),
            Path::new("d/f")
        );
    }
}
