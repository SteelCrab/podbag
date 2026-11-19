//! Mount the container's root filesystem and switch to it.

use crate::spec::Rootfs;
use crate::{Error, ErrorKind, Result};
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
use std::ffi::CStr;
use std::path::Path;

/// Mount the root filesystem (an overlay of the image, or a plain directory), then
/// /proc, /dev, /sys and friends under it, then pivot_root into it.
/// Runs in the container process, inside its new mount namespace.
pub(super) fn enter_rootfs(
    spec: &Rootfs,
    mounts: &[crate::volume::Mount],
    own_netns: bool,
    rootless: bool,
    privileged: bool,
) -> Result<()> {
    step(
        rootless,
        "make mounts private",
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
        ),
    )?;
    let rootfs = mount_root(spec, rootless)?;

    let at = |p: &str| rootfs.join(p);

    let mount_fs = |source, target: &Path, fstype, flags, data| {
        mount_fs(rootless, source, target, fstype, flags, data)
    };
    let make_dir = |path: &Path| make_dir(rootless, path);

    mount_fs(
        "proc",
        &at("proc"),
        "proc",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        None,
    )?;
    // A privileged container sees the kernel's files as they are.
    let read_only: &[&str] = if privileged { &[] } else { &READ_ONLY };
    if !privileged {
        mask(rootless, rootfs, "proc/")?;
    }
    for rel in read_only {
        let path = at(rel);
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        step(
            rootless,
            &format!("bind {rel}"),
            rustix::mount::mount_bind(&path, &path),
        )?;
        step(
            rootless,
            &format!("make {rel} read-only"),
            set_read_only_recursive(&path),
        )?;
    }
    mount_fs(
        "tmpfs",
        &at("dev"),
        "tmpfs",
        MountFlags::NOSUID,
        Some(c"mode=0755"),
    )?;

    for dev in ["null", "zero", "full", "random", "urandom", "tty"] {
        let target = at("dev").join(dev);
        step(
            rootless,
            "create a device mount point",
            std::fs::File::create(&target).map(drop),
        )?;
        step(
            rootless,
            "bind a device node",
            rustix::mount::mount_bind(format!("/dev/{dev}").as_str(), &target),
        )?;
    }

    mount_fs(
        "devpts",
        &at("dev/pts"),
        "devpts",
        MountFlags::NOSUID | MountFlags::NOEXEC,
        Some(c"newinstance,ptmxmode=0666,mode=0620"),
    )?;
    for (link, target) in [
        ("ptmx", "pts/ptmx"),
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        step(
            rootless,
            "create a /dev link",
            std::os::unix::fs::symlink(target, at("dev").join(link)),
        )?;
    }
    mount_fs(
        "shm",
        &at("dev/shm"),
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        Some(c"mode=1777"),
    )?;
    mount_fs(
        "mqueue",
        &at("dev/mqueue"),
        "mqueue",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        None,
    )?;

    // A new sysfs shows the container's own network devices, and needs a network namespace
    // of its own (owned by its user namespace). Without one, bind the host's.
    let sys = at("sys");
    if own_netns {
        let writable = if privileged {
            MountFlags::empty()
        } else {
            MountFlags::RDONLY
        };
        mount_fs(
            "sysfs",
            &sys,
            "sysfs",
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC | writable,
            None,
        )?;
    } else {
        make_dir(&sys)?;
        step(
            rootless,
            "bind /sys",
            rustix::mount::mount_bind_recursive("/sys", &sys),
        )?;
    }
    // The container's own cgroup tree (its cgroup namespace), where the host has cgroup v2.
    // The kernel refuses cgroup2 right on the root of the bound host cgroup2 mount (same
    // superblock: EBUSY), and that mount cannot be detached in rootless mode (it is locked),
    // so an empty tmpfs goes in between.
    let cgroup = sys.join("fs/cgroup");
    if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() && cgroup.is_dir() {
        step(
            rootless,
            "cover sys/fs/cgroup",
            rustix::mount::mount(
                "tmpfs",
                &cgroup,
                "tmpfs",
                MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
                Some(c"size=0,mode=0755"),
            ),
        )?;
        step(
            rootless,
            "mount sys/fs/cgroup",
            rustix::mount::mount(
                "cgroup2",
                &cgroup,
                "cgroup2",
                MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
                None,
            ),
        )?;
    }
    // Privileged: /sys and the container's own cgroup stay writable (as far as the kernel
    // lets its user namespace write).
    if !privileged {
        mask(rootless, rootfs, "sys/")?;
        step(
            rootless,
            "make /sys read-only",
            set_read_only_recursive(&sys),
        )?;
    }
    // The container's own mounts come last, as with runc, so they may cover /dev/shm.
    super::mounts::mount_all(rootless, rootfs, mounts)?;

    step(
        rootless,
        "enter the root filesystem",
        rustix::process::chdir(rootfs),
    )?;
    step(
        rootless,
        "pivot_root",
        rustix::process::pivot_root(".", "."),
    )?;
    step(
        rootless,
        "detach the old root",
        rustix::mount::unmount(".", UnmountFlags::DETACH),
    )?;
    step(rootless, "enter the new root", rustix::process::chdir("/"))
}

/// Kernel files and directories a container must not read (Docker's list). Files are
/// covered with /dev/null, directories with an empty read-only tmpfs.
const MASKED: [&str; 12] = [
    "proc/asound",
    "proc/acpi",
    "proc/interrupts",
    "proc/kcore",
    "proc/keys",
    "proc/latency_stats",
    "proc/timer_list",
    "proc/timer_stats",
    "proc/sched_debug",
    "proc/scsi",
    "sys/firmware",
    "sys/devices/virtual/powercap",
];

/// Kernel settings a container may read but not change (Docker's list).
const READ_ONLY: [&str; 5] = [
    "proc/bus",
    "proc/fs",
    "proc/irq",
    "proc/sys",
    "proc/sysrq-trigger",
];

/// Mount the container's root filesystem (the image overlay, or bind its directory onto
/// itself, as pivot_root needs a mount). Returns where it is.
fn mount_root(spec: &Rootfs, rootless: bool) -> Result<&Path> {
    let rootfs = match spec {
        Rootfs::Dir { path } => Path::new(path),
        Rootfs::Overlay {
            lower,
            upper,
            work,
            merged,
            data,
        } => {
            let options = overlay_options(lower, data.as_deref(), upper, work, rootless);
            let options = std::ffi::CString::new(options).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "cannot set up the container root filesystem",
                    "an overlay path contains a NUL byte",
                    "use another data directory",
                )
            })?;
            step(
                rootless,
                "mount the image overlay",
                rustix::mount::mount(
                    "overlay",
                    merged.as_str(),
                    "overlay",
                    MountFlags::empty(),
                    Some(options.as_c_str()),
                ),
            )?;
            Path::new(merged)
        }
    };
    step(
        rootless,
        "bind the root filesystem",
        rustix::mount::mount_bind_recursive(rootfs, rootfs),
    )?;
    Ok(rootfs)
}

/// For copying files of a container that does not run: in a mount namespace of this
/// process's own, mount its root filesystem and its volumes and bind mounts (not its tmpfs
/// mounts: they would hold nothing afterwards) and make it this process's root.
pub(super) fn enter_files(spec: &crate::spec::ContainerSpec, rootless: bool) -> Result<()> {
    step(
        rootless,
        "make mounts private",
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
        ),
    )?;
    let rootfs = mount_root(&spec.rootfs, rootless)?;
    let lasting: Vec<crate::volume::Mount> = spec
        .mounts
        .iter()
        .filter(|m| !matches!(m.source, crate::volume::MountSource::Tmpfs { .. }))
        .map(|m| crate::volume::Mount {
            // A new volume is filled with the image's files at the first start.
            copy_up: false,
            ..m.clone()
        })
        .collect();
    super::mounts::mount_all(rootless, rootfs, &lasting)?;
    step(
        rootless,
        "enter the root filesystem",
        rustix::process::chdir(rootfs),
    )?;
    step(rootless, "make it the root", rustix::process::chroot("."))?;
    step(rootless, "enter the root", rustix::process::chdir("/"))
}

/// For reading what a container that does not run changed: in a mount namespace of this
/// process's own, mount its root filesystem (without volumes). Returns the upper
/// directory, the merged view and the image (lower) directory.
pub(super) fn mount_changes(
    spec: &crate::spec::ContainerSpec,
    rootless: bool,
) -> Result<(String, String, String)> {
    let Rootfs::Overlay {
        lower,
        upper,
        merged,
        ..
    } = &spec.rootfs
    else {
        return Err(no_image_layer(&spec.name));
    };
    step(
        rootless,
        "make mounts private",
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
        ),
    )?;
    mount_root(&spec.rootfs, rootless)?;
    Ok((upper.clone(), merged.clone(), lower.clone()))
}

/// A container on a plain folder (`run --rootfs`) has no image to compare with.
pub(super) fn no_image_layer(name: &str) -> Error {
    Error::new(
        ErrorKind::Unsupported,
        format!("cannot read the changes of container {name}"),
        "it runs on a plain folder (--rootfs), not on an image",
        "copy its files with `podbag cp`",
    )
}

/// Cover the masked paths under `root` that start with `prefix`. Paths this kernel does
/// not have are skipped.
/// overlayfs options for a container's root. `data` is the object store when `lower` is an
/// image's EROFS metadata (composefs): a data-only layer, which needs metacopy and redirects.
/// Inside a user namespace, overlayfs keeps its metadata in user.* xattrs.
pub(crate) fn overlay_options(
    lower: &str,
    data: Option<&str>,
    upper: &str,
    work: &str,
    rootless: bool,
) -> String {
    let mut options = format!("lowerdir={lower}");
    if let Some(data) = data {
        options.push_str(&format!("::{data}"));
    }
    options.push_str(&format!(",upperdir={upper},workdir={work}"));
    if data.is_some() {
        options.push_str(",metacopy=on,redirect_dir=on");
    }
    if rootless {
        options.push_str(",userxattr");
    }
    options
}

fn mask(rootless: bool, root: &Path, prefix: &str) -> Result<()> {
    for rel in MASKED.iter().filter(|p| p.starts_with(prefix)) {
        let path = root.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let covered = if meta.is_dir() {
            rustix::mount::mount(
                "tmpfs",
                &path,
                "tmpfs",
                MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
                None,
            )
        } else {
            rustix::mount::mount_bind("/dev/null", &path)
        };
        step(rootless, &format!("mask {rel}"), covered)?;
    }
    Ok(())
}

fn mount_fs(
    rootless: bool,
    source: &str,
    target: &Path,
    fstype: &str,
    flags: MountFlags,
    data: Option<&CStr>,
) -> Result<()> {
    make_dir(rootless, target)?;
    step(
        rootless,
        &format!("mount {}", target_name(target)),
        rustix::mount::mount(source, target, fstype, flags, data),
    )
}

fn make_dir(rootless: bool, path: &Path) -> Result<()> {
    step(
        rootless,
        &format!("create {}", target_name(path)),
        std::fs::create_dir_all(path),
    )
}

/// The last two path components, for error messages ("dev/pts").
fn target_name(path: &Path) -> String {
    let parts: Vec<_> = path.components().rev().take(2).collect();
    parts
        .iter()
        .rev()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// mount_setattr(AT_RECURSIVE, MOUNT_ATTR_RDONLY): read-only for the mount and all submounts.
fn set_read_only_recursive(path: &Path) -> std::io::Result<()> {
    #[repr(C)]
    struct MountAttr {
        attr_set: u64,
        attr_clr: u64,
        propagation: u64,
        userns_fd: u64,
    }
    const MOUNT_ATTR_RDONLY: u64 = 0x1;
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(std::io::Error::other)?;
    // SAFETY: valid NUL-terminated path, a correctly sized mount_attr, and documented flags.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            c_path.as_ptr(),
            libc::AT_RECURSIVE as libc::c_uint,
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

/// Turn an OS error from one setup step into a podbag error.
fn step<T, E: Into<std::io::Error>>(
    rootless: bool,
    name: &str,
    r: std::result::Result<T, E>,
) -> Result<T> {
    r.map_err(|e| {
        let e: std::io::Error = e.into();
        if let Some(denied) = super::userns_denial(rootless, Some(&e), None) {
            return denied;
        }
        Error::new(
            ErrorKind::Runtime,
            "cannot set up the container root filesystem",
            format!("{name}: {e}"),
            "check that the root filesystem directory holds a complete Linux system",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_options_for_both_methods() {
        assert_eq!(
            overlay_options("/s/img", None, "/c/upper", "/c/work", false),
            "lowerdir=/s/img,upperdir=/c/upper,workdir=/c/work"
        );
        assert_eq!(
            overlay_options("/s/img", None, "/c/upper", "/c/work", true),
            "lowerdir=/s/img,upperdir=/c/upper,workdir=/c/work,userxattr"
        );
        assert_eq!(
            overlay_options(
                "/r/images/ab",
                Some("/s/objects"),
                "/c/upper",
                "/c/work",
                false
            ),
            "lowerdir=/r/images/ab::/s/objects,upperdir=/c/upper,workdir=/c/work,\
             metacopy=on,redirect_dir=on"
        );
    }
}
