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
    security: &crate::spec::Security,
) -> Result<()> {
    let privileged = security.privileged;
    // Mounts of the host reach the container only through a mount that asks for them:
    // then the copies of the host's mounts stay slaves (they receive, never send), as with
    // runc's rslave. New mounts made here are private either way.
    let receive = mounts.iter().any(|m| {
        matches!(&m.source, crate::volume::MountSource::Bind { propagation, .. } if !propagation.is_private())
    });
    step(
        rootless,
        "make mounts private",
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::REC
                | if receive {
                    MountPropagationFlags::DOWNSTREAM
                } else {
                    MountPropagationFlags::PRIVATE
                },
        ),
    )?;
    let (masked, read_only) = kernel_paths(security);
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
    mask(rootless, rootfs, &masked, "proc/")?;
    for rel in read_only.iter().filter(|p| p.starts_with("proc/")) {
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
    mask(rootless, rootfs, &masked, "sys/")?;
    if !privileged {
        step(
            rootless,
            "make /sys read-only",
            set_read_only_recursive(&sys),
        )?;
    }
    // The container's own mounts come last, as with runc, so they may cover /dev/shm.
    super::mounts::mount_all(rootless, rootfs, mounts)?;
    // Other kernel paths a caller listed.
    mask(rootless, rootfs, &masked, "")?;
    for rel in read_only
        .iter()
        .filter(|p| !p.starts_with("proc/") && !p.starts_with("sys/"))
    {
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

/// The kernel paths to hide and to make read-only, relative to the root: the caller's
/// lists, or Docker's; none for a privileged container (it sees the kernel's files as
/// they are).
fn kernel_paths(security: &crate::spec::Security) -> (Vec<String>, Vec<String>) {
    if security.privileged {
        return (Vec::new(), Vec::new());
    }
    let relative = |list: &Option<Vec<String>>, default: &[&str]| -> Vec<String> {
        match list {
            Some(paths) => paths
                .iter()
                .map(|p| p.trim_start_matches('/').to_string())
                .filter(|p| !p.is_empty() && !p.split('/').any(|c| c == ".."))
                .collect(),
            None => default.iter().map(|p| p.to_string()).collect(),
        }
    };
    (
        relative(&security.masked_paths, &MASKED),
        relative(&security.readonly_paths, &READ_ONLY),
    )
}

/// After `pivot_root`: make the root mount read-only (not the mounts on it).
pub(super) fn make_root_read_only(rootless: bool) -> Result<()> {
    step(
        rootless,
        "make the root filesystem read-only",
        rustix::mount::mount_remount("/", MountFlags::BIND | MountFlags::RDONLY, ""),
    )
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
    // Paths with the same content share an inode in the image. With the inode index, a
    // copy-up would keep that link, and a write to one path would show at the others.
    // Some kernels turn the index on by default; Docker and Podman turn it off too.
    options.push_str(",index=off");
    if data.is_some() {
        options.push_str(",metacopy=on,redirect_dir=on");
    }
    if rootless {
        options.push_str(",userxattr");
    }
    options
}

/// `prefix` "" takes the paths outside /proc and /sys.
fn mask(rootless: bool, root: &Path, masked: &[String], prefix: &str) -> Result<()> {
    let wanted = |p: &&String| match prefix {
        "" => !p.starts_with("proc/") && !p.starts_with("sys/"),
        _ => p.starts_with(prefix),
    };
    for rel in masked.iter().filter(wanted) {
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
    fn kernel_paths_come_from_the_settings_or_dockers_lists() {
        use crate::spec::Security;
        let (masked, read_only) = kernel_paths(&Security::default());
        assert_eq!(masked.len(), MASKED.len());
        assert!(read_only.contains(&"proc/sys".to_string()));
        let privileged = Security {
            privileged: true,
            ..Default::default()
        };
        assert_eq!(kernel_paths(&privileged), (vec![], vec![]));
        let own = Security {
            masked_paths: Some(vec!["/proc/kcore".into(), "/x/../etc".into(), "/".into()]),
            readonly_paths: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(
            kernel_paths(&own),
            (vec!["proc/kcore".to_string()], Vec::<String>::new())
        );
    }

    #[test]
    fn overlay_options_for_both_methods() {
        assert_eq!(
            overlay_options("/s/img", None, "/c/upper", "/c/work", false),
            "lowerdir=/s/img,upperdir=/c/upper,workdir=/c/work,index=off"
        );
        assert_eq!(
            overlay_options("/s/img", None, "/c/upper", "/c/work", true),
            "lowerdir=/s/img,upperdir=/c/upper,workdir=/c/work,index=off,userxattr"
        );
        assert_eq!(
            overlay_options(
                "/r/images/ab",
                Some("/s/objects"),
                "/c/upper",
                "/c/work",
                false
            ),
            "lowerdir=/r/images/ab::/s/objects,upperdir=/c/upper,workdir=/c/work,index=off,\
             metacopy=on,redirect_dir=on"
        );
    }

    /// The kernel's default for the overlay inode index, on for one test and put back when
    /// the guard goes, even when the test panics.
    struct IndexDefault(String);

    impl IndexDefault {
        const PARAM: &str = "/sys/module/overlay/parameters/index";

        fn on() -> IndexDefault {
            let old = std::fs::read_to_string(Self::PARAM).unwrap();
            std::fs::write(Self::PARAM, "Y").unwrap();
            IndexDefault(old.trim().to_string())
        }
    }

    impl Drop for IndexDefault {
        fn drop(&mut self) {
            let _ = std::fs::write(Self::PARAM, &self.0);
        }
    }

    /// Paths with the same content share an inode in the image. With the kernel's
    /// `index=on`, a copy-up keeps such hard links, and a write to one path would change
    /// the others: the mount must turn the index off.
    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn a_write_changes_only_its_own_path_under_an_index_on_default_as_root() {
        let _index = IndexDefault::on();
        // Without index=off the write shows at the other path; else this file system
        // cannot test it (the kernel falls back to no index).
        let control = write_one_read_the_other(|lower, upper, work| {
            format!("lowerdir={lower},upperdir={upper},workdir={work}")
        });
        if control == "same\n" {
            eprintln!("the kernel does not use the inode index here: skipped");
            return;
        }
        let seen = write_one_read_the_other(|lower, upper, work| {
            overlay_options(lower, None, upper, work, false)
        });
        assert_eq!(seen, "same\n");
    }

    /// Mount an overlay whose lower directory holds `a` and `b`, one inode, with the
    /// options made from the lower, upper and work directories; append to `a`; read `b`.
    fn write_one_read_the_other(options: impl Fn(&str, &str, &str) -> String) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let dir = |name: &str| tmp.path().join(name);
        for name in ["lower", "upper", "work", "merged"] {
            std::fs::create_dir(dir(name)).unwrap();
        }
        std::fs::write(dir("lower").join("a"), "same\n").unwrap();
        std::fs::hard_link(dir("lower").join("a"), dir("lower").join("b")).unwrap();
        let text = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
        let options = options(&text(dir("lower")), &text(dir("upper")), &text(dir("work")));
        let merged = dir("merged");
        let seen = crate::store::composefs::in_private_mount_ns(|| {
            let options = std::ffi::CString::new(options).unwrap();
            let mounted = rustix::mount::mount(
                "overlay",
                &merged,
                "overlay",
                MountFlags::empty(),
                Some(options.as_c_str()),
            );
            mounted.map_err(|e| crate::Error::io("mount", &e.into(), ""))?;
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(merged.join("a"))
                .and_then(|mut f| f.write_all(b"changed\n"))
                .map_err(|e| crate::Error::io("append", &e, ""))?;
            std::fs::read_to_string(merged.join("b")).map_err(|e| crate::Error::io("read", &e, ""))
        });
        seen.unwrap()
    }
}
