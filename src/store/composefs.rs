//! Mounting composefs images: an image's EROFS metadata, and overlayfs with
//! the object store as its data-only layer.
//!
//! Containers share one EROFS mount per image on the host (`mount_shared`); each gets its
//! own overlay in its mount namespace. `with_mounted` mounts both privately for a short
//! job (the probe, `check-image`) and leaves nothing behind.

use crate::{Error, ErrorKind, Result};
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

/// `f_type` of an EROFS mount.
const EROFS_MAGIC: u32 = 0xE0F5_E1E2;
const LOOP_CTL_GET_FREE: u64 = 0x4C82;
const LOOP_CONFIGURE: u64 = 0x4C0A;
const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;

/// `struct loop_info64` from `<linux/loop.h>`.
#[repr(C)]
struct LoopInfo64 {
    device: u64,
    inode: u64,
    rdevice: u64,
    offset: u64,
    sizelimit: u64,
    number: u32,
    encrypt_type: u32,
    encrypt_key_size: u32,
    flags: u32,
    file_name: [u8; 64],
    crypt_name: [u8; 64],
    encrypt_key: [u8; 32],
    init: [u64; 2],
}

/// `struct loop_config` from `<linux/loop.h>`.
#[repr(C)]
struct LoopConfig {
    fd: u32,
    block_size: u32,
    info: LoopInfo64,
    reserved: [u64; 8],
}

/// Mount the EROFS image read-only at `target`: straight from the file (Linux 6.12 and
/// later), else through a loop device.
pub fn mount_erofs(image: &Path, target: &Path) -> std::io::Result<()> {
    match rustix::mount::mount(image, target, "erofs", MountFlags::RDONLY, None) {
        Err(rustix::io::Errno::NOTBLK) => mount_erofs_loop(image, target),
        other => other.map_err(Into::into),
    }
}

/// Mount the EROFS image through a new read-only loop device. The device clears itself
/// when the mount goes away (or at once, if the mount fails).
pub(crate) fn mount_erofs_loop(image: &Path, target: &Path) -> std::io::Result<()> {
    let file = File::open(image)?;
    let control = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/loop-control")?;
    for _ in 0..10 {
        // SAFETY: an ioctl without memory arguments on a file this function owns.
        let n = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let device = format!("/dev/loop{n}");
        let dev = File::open(&device)?;
        // SAFETY: plain integers and byte arrays; all zeros is a valid value.
        let mut config: LoopConfig = unsafe { std::mem::zeroed() };
        config.fd = file.as_raw_fd() as u32;
        config.info.flags = LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR;
        // SAFETY: LOOP_CONFIGURE reads a struct loop_config, which `config` is.
        if unsafe { libc::ioctl(dev.as_raw_fd(), LOOP_CONFIGURE as _, &config) } < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EBUSY) {
                continue; // another process took this device first
            }
            return Err(e);
        }
        return rustix::mount::mount(device.as_str(), target, "erofs", MountFlags::RDONLY, None)
            .map_err(Into::into);
    }
    Err(std::io::Error::from_raw_os_error(libc::EBUSY))
}

fn mount_error(step: &str, e: impl Into<std::io::Error>) -> Error {
    let e = e.into();
    Error::new(
        ErrorKind::Runtime,
        format!("cannot {step}"),
        e.to_string(),
        "run `podbag check`; set image_mount = \"hardlink\" in config.toml to avoid composefs",
    )
}

/// Run `f` in a new thread with a private mount namespace of its own: its mounts are seen
/// by no other thread and go away with the thread.
pub(crate) fn in_private_mount_ns<T: Send>(f: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    std::thread::scope(|s| {
        let worker = s.spawn(|| {
            // SAFETY: unshare has no memory arguments; it affects only this thread.
            if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
                return Err(mount_error(
                    "create a private mount namespace",
                    std::io::Error::last_os_error(),
                ));
            }
            rustix::mount::mount_change(
                "/",
                MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
            )
            .map_err(|e| mount_error("make the mounts private", e))?;
            f()
        });
        match worker.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// Run `f` on the image's tree: the EROFS `image` and overlayfs with `objects` as its
/// data-only layer, mounted under `scratch` in a private mount namespace. The caller
/// removes `scratch` afterwards.
pub fn with_mounted<T: Send>(
    image: &Path,
    objects: &Path,
    scratch: &Path,
    f: impl FnOnce(&Path) -> T + Send,
) -> Result<T> {
    let dir = |name: &str| scratch.join(name);
    for name in ["lower", "upper", "work", "merged"] {
        std::fs::create_dir_all(dir(name)).map_err(|e| {
            Error::io(
                format!("cannot create {}", dir(name).display()),
                &e,
                "check the permissions of the podbag data directory",
            )
        })?;
    }
    in_private_mount_ns(|| {
        mount_erofs(image, &dir("lower"))
            .map_err(|e| mount_error("mount the image's EROFS metadata", e))?;
        let text = |p: &Path| p.to_string_lossy().into_owned();
        let options = crate::linux::overlay_options(
            &text(&dir("lower")),
            Some(&text(objects)),
            &text(&dir("upper")),
            &text(&dir("work")),
            false,
        );
        let options = std::ffi::CString::new(options).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "cannot mount the image",
                "a path contains a NUL byte",
                "use another data directory",
            )
        })?;
        rustix::mount::mount(
            "overlay",
            &dir("merged"),
            "overlay",
            MountFlags::empty(),
            Some(options.as_c_str()),
        )
        .map_err(|e| mount_error("mount the image overlay", e))?;
        let out = f(&dir("merged"));
        let _ = rustix::mount::unmount(&dir("merged"), UnmountFlags::DETACH);
        let _ = rustix::mount::unmount(&dir("lower"), UnmountFlags::DETACH);
        Ok(out)
    })
}

/// Try composefs on this kernel in `scratch` (removed afterwards): an image of one file
/// and one symbolic link, mounted with its object; the file is read, appended to (a copy-up
/// from the data-only layer) and read again. The error names the failed step.
pub fn probe(scratch: &Path) -> std::result::Result<(), String> {
    let result = probe_in(scratch);
    let _ = crate::store::remove_tree(scratch);
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    result.map_err(|why| format!("{why} (Linux {})", kernel.trim()))
}

fn probe_in(scratch: &Path) -> std::result::Result<(), String> {
    use crate::store::layer::{Entry, EntryKind};
    use std::io::Write;
    const CONTENT: &[u8] = b"podbag composefs probe\n";
    let failed = |what: &str, e: &dyn std::fmt::Display| format!("{what}: {e}");
    let objects = scratch.join("objects");
    let key = {
        use sha2::Digest;
        format!(
            "{}.0.0.644",
            crate::store::layer::hex(&sha2::Sha256::digest(CONTENT))
        )
    };
    let object = objects.join(crate::store::object::ObjectStore::relative_path(&key));
    std::fs::create_dir_all(object.parent().expect("object path has a parent"))
        .and_then(|()| std::fs::write(&object, CONTENT))
        .map_err(|e| failed("cannot write the probe's object", &e))?;
    let entry = |path: &str, kind: EntryKind, mode: u32| Entry {
        path: path.into(),
        kind,
        mode,
        uid: 0,
        gid: 0,
        xattrs: vec![],
        mtime: None,
    };
    let tree = crate::store::tree::merge(&[vec![
        entry("d", EntryKind::Dir, 0o755),
        entry(
            "d/f",
            EntryKind::File {
                object: key,
                size: CONTENT.len() as u64,
            },
            0o644,
        ),
        entry(
            "l",
            EntryKind::Symlink {
                target: "d/f".into(),
            },
            0o777,
        ),
    ]])
    .map_err(|e| failed("cannot build the probe's tree", &e))?;
    let image = scratch.join("probe.erofs");
    let bytes = crate::store::erofs::build(&tree, 0)
        .map_err(|e| failed("cannot build the probe's EROFS image", &e))?;
    std::fs::write(&image, bytes).map_err(|e| failed("cannot write the probe's image", &e))?;
    with_mounted(&image, &objects, &scratch.join("mnt"), |root| {
        let read = |p: &str| std::fs::read(root.join(p));
        match read("d/f") {
            Ok(b) if b == CONTENT => {}
            Ok(_) => return Err("the file's contents are wrong".to_string()),
            Err(e) => return Err(failed("cannot read a file", &e)),
        }
        match std::fs::read_link(root.join("l")) {
            Ok(t) if t == Path::new("d/f") => {}
            Ok(t) => return Err(format!("the symbolic link points to {}", t.display())),
            Err(e) => return Err(failed("cannot read a symbolic link", &e)),
        }
        OpenOptions::new()
            .append(true)
            .open(root.join("d/f"))
            .and_then(|mut f| f.write_all(b"+"))
            .map_err(|e| failed("cannot write to a file", &e))?;
        match read("d/f") {
            Ok(b) if b.strip_suffix(b"+") == Some(CONTENT) => Ok(()),
            Ok(_) => Err("a written file lost its contents".to_string()),
            Err(e) => Err(failed("cannot read a written file", &e)),
        }
    })
    .map_err(|e| format!("{}: {}", e.what, e.why))?
}

/// Mount `image` at `target` for every container of the image, unless it is mounted there
/// already. `<target>.lock` keeps two starts from mounting it twice.
pub fn mount_shared(image: &Path, target: &Path) -> Result<()> {
    let fail = |what: String, e: std::io::Error| {
        Error::io(
            what,
            &e,
            "check the permissions of the podbag runtime directory",
        )
    };
    let parent = target.parent().expect("a mount point has a parent");
    crate::store::perm::private_dir_builder()
        .recursive(true)
        .create(parent)
        .map_err(|e| fail(format!("cannot create {}", parent.display()), e))?;
    let lock_path = target.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| fail(format!("cannot open {}", lock_path.display()), e))?;
    lock.lock()
        .map_err(|e| fail(format!("cannot lock {}", lock_path.display()), e))?;
    if is_erofs(target) {
        return Ok(());
    }
    match std::fs::create_dir(target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(fail(format!("cannot create {}", target.display()), e)),
    }
    mount_erofs(image, target).map_err(|e| mount_error("mount the image's EROFS metadata", e))
}

pub(crate) fn is_erofs(path: &Path) -> bool {
    rustix::fs::statfs(path).is_ok_and(|s| s.f_type as u32 == EROFS_MAGIC)
}

/// Undo `mount_shared`: detach every mount at `target`, then remove it and its lock.
pub fn unmount_shared(target: &Path) -> std::io::Result<()> {
    // Only an EROFS mount is taken down: a plain directory (a lost mount, or a store that
    // never mounted anything) needs no privilege to remove.
    for _ in 0..16 {
        if !is_erofs(target) {
            break;
        }
        match rustix::mount::unmount(target, UnmountFlags::DETACH) {
            Ok(()) => {} // an earlier race may have stacked a second mount: check again
            Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOENT) => break,
            Err(e) => return Err(e.into()),
        }
    }
    for result in [
        std::fs::remove_dir(target),
        std::fs::remove_file(target.with_extension("lock")),
    ] {
        match result {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::layer::{Entry, EntryKind, hex};
    use crate::store::object::{Meta, ObjectStore};
    use crate::store::tree::{Tree, materialize, merge};
    use std::collections::BTreeMap;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    /// What a container sees of one path.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Seen {
        mode: u32,
        uid: u32,
        gid: u32,
        /// None for directories, whose sizes differ between file systems.
        size: Option<u64>,
        target: Option<PathBuf>,
        rdev: Option<u64>,
        xattrs: BTreeMap<String, Vec<u8>>,
        sha: Option<String>,
        mtime: i64,
    }

    fn xattrs_of(path: &Path) -> BTreeMap<String, Vec<u8>> {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let mut names = vec![0u8; 4096];
        // SAFETY: valid path and buffer of the given length.
        let n = unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
        assert!(n >= 0, "llistxattr {}", path.display());
        let mut out = BTreeMap::new();
        for name in names[..n as usize]
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
        {
            let cname = std::ffi::CString::new(name).unwrap();
            let mut value = vec![0u8; 65536];
            // SAFETY: as above.
            let len = unsafe {
                libc::lgetxattr(
                    c.as_ptr(),
                    cname.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                )
            };
            assert!(len >= 0);
            value.truncate(len as usize);
            out.insert(String::from_utf8(name.to_vec()).unwrap(), value);
        }
        out
    }

    /// Every path below `root` (not `root` itself), plus the groups of paths that share an
    /// inode. `contents`: hash regular files.
    fn walk(root: &Path, contents: bool) -> (BTreeMap<String, Seen>, Vec<Vec<String>>) {
        fn go(
            root: &Path,
            dir: &Path,
            contents: bool,
            out: &mut BTreeMap<String, Seen>,
            inodes: &mut BTreeMap<u64, Vec<String>>,
        ) {
            let mut list: Vec<PathBuf> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            list.sort();
            for p in list {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let m = std::fs::symlink_metadata(&p).unwrap();
                let ft = m.file_type();
                if ft.is_file() {
                    inodes.entry(m.ino()).or_default().push(rel.clone());
                }
                use std::os::unix::fs::FileTypeExt;
                let sha = (contents && ft.is_file()).then(|| {
                    use sha2::Digest;
                    hex(&sha2::Sha256::digest(std::fs::read(&p).unwrap()))
                });
                out.insert(
                    rel,
                    Seen {
                        mode: m.mode(),
                        uid: m.uid(),
                        gid: m.gid(),
                        size: (!ft.is_dir()).then(|| m.size()),
                        target: ft.is_symlink().then(|| std::fs::read_link(&p).unwrap()),
                        rdev: (ft.is_char_device() || ft.is_block_device()).then(|| m.rdev()),
                        xattrs: xattrs_of(&p),
                        sha,
                        mtime: m.mtime(),
                    },
                );
                if ft.is_dir() {
                    go(root, &p, contents, out, inodes);
                }
            }
        }
        let mut out = BTreeMap::new();
        let mut inodes = BTreeMap::new();
        go(root, root, contents, &mut out, &mut inodes);
        let mut groups: Vec<Vec<String>> = inodes.into_values().collect();
        groups.sort();
        (out, groups)
    }

    fn without_overlay_xattrs(mut seen: BTreeMap<String, Seen>) -> BTreeMap<String, Seen> {
        for s in seen.values_mut() {
            s.xattrs.retain(|k, _| !k.starts_with("trusted.overlay."));
        }
        seen
    }

    struct Fixture {
        tmp: tempfile::TempDir,
        tree: Tree,
        objects: ObjectStore,
        image: PathBuf,
    }

    impl Fixture {
        fn objects_dir(&self) -> PathBuf {
            self.tmp.path().join("objects")
        }
    }

    /// A tree with every kind of entry, its objects stored with owners, and its image.
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let objects = ObjectStore::new(tmp.path().join("objects"), true).unwrap();
        let mut entries = Vec::new();
        let mut file = |path: &str,
                        data: &[u8],
                        mode: u32,
                        uid: u32,
                        xattrs: Vec<(String, Vec<u8>)>,
                        mtime: u64| {
            let meta = Meta {
                uid,
                gid: uid,
                mode,
                xattrs: xattrs.clone(),
            };
            let (object, size) = objects.insert(&mut &data[..], &meta, Some(mtime)).unwrap();
            entries.push(Entry {
                path: path.into(),
                kind: EntryKind::File { object, size },
                mode,
                uid,
                gid: uid,
                xattrs: xattrs.iter().map(|(k, v)| (k.clone(), hex(v))).collect(),
                mtime: Some(mtime),
            });
        };
        file("bin/tool", b"tool", 0o4755, 0, vec![], 10);
        file("a/one", b"same", 0o644, 0, vec![], 20);
        file("b/two", b"same", 0o644, 0, vec![], 20);
        // The same contents with another time: an inode of its own.
        file("c/three", b"same", 0o644, 0, vec![], 30);
        file("empty", b"", 0o644, 0, vec![], 40);
        file("empty2", b"", 0o644, 0, vec![], 40);
        file("etc/conf", b"x=1\n", 0o644, 0, vec![], 50);
        // A crontab with the same object and time: an inode of its own too.
        file("etc/cron.d/job", b"x=1\n", 0o644, 0, vec![], 50);
        file("home/user/notes", b"mine", 0o600, 1000, vec![], 60);
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        file("big", &big, 0o644, 0, vec![], 70);
        let cap: Vec<u8> = vec![
            1, 0, 0, 2, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        file(
            "bin/ping",
            b"ping",
            0o755,
            0,
            vec![
                ("security.capability".into(), cap),
                ("user.note".into(), b"hi".to_vec()),
            ],
            80,
        );
        let plain = |path: &str, kind: EntryKind, mode: u32, uid: u32| Entry {
            path: path.into(),
            kind,
            mode,
            uid,
            gid: uid,
            xattrs: vec![],
            mtime: Some(90 + u64::from(mode)),
        };
        entries.push(plain("tmp", EntryKind::Dir, 0o1777, 0));
        entries.push(plain("home/user", EntryKind::Dir, 0o700, 1000));
        entries.push(plain(
            "lib/link",
            EntryKind::Symlink {
                target: "../etc/conf".into(),
            },
            0o777,
            0,
        ));
        entries.push(plain(
            "dev/null2",
            EntryKind::Char { major: 1, minor: 3 },
            0o666,
            0,
        ));
        entries.push(plain("run/fifo", EntryKind::Fifo, 0o600, 0));
        let tree = merge(&[entries]).unwrap();
        let image = tmp.path().join("image.erofs");
        // Directories merge() made have no time: both views give them the default, 1.
        std::fs::write(&image, crate::store::erofs::build(&tree, 1).unwrap()).unwrap();
        Fixture {
            tmp,
            tree,
            objects,
            image,
        }
    }

    fn hardlink_view(f: &Fixture) -> (BTreeMap<String, Seen>, Vec<Vec<String>>) {
        let dest = f.tmp.path().join("tree");
        materialize(&f.tree, &f.objects, &dest, true, 1).unwrap();
        walk(&dest, true)
    }

    fn mount_count(target: &Path) -> usize {
        std::fs::read_to_string("/proc/thread-self/mountinfo")
            .unwrap()
            .lines()
            .filter(|l| {
                let fields: Vec<&str> = l.split(' ').collect();
                let fs = l.split(" - ").nth(1).unwrap_or("");
                fields.get(4) == Some(&target.to_str().unwrap()) && fs.starts_with("erofs ")
            })
            .count()
    }

    fn loop_devices_for(image: &Path) -> Vec<String> {
        let image = std::fs::canonicalize(image).unwrap();
        std::fs::read_dir("/sys/block")
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("loop"))
            .filter(|e| {
                std::fs::read_to_string(e.path().join("loop/backing_file"))
                    .is_ok_and(|b| Path::new(b.trim()) == image)
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn erofs_metadata_mounts_as_root() {
        let f = fixture();
        let (expected, groups) = hardlink_view(&f);
        let mnt = f.tmp.path().join("mnt");
        std::fs::create_dir(&mnt).unwrap();
        let (seen, seen_groups) = in_private_mount_ns(|| {
            mount_erofs(&f.image, &mnt).map_err(|e| crate::Error::io("mount", &e, ""))?;
            Ok(walk(&mnt, false))
        })
        .unwrap();
        assert_eq!(seen_groups, groups);
        for (path, s) in &seen {
            if s.size.is_some_and(|n| n > 0) && s.mode & 0o170000 == 0o100000 {
                let redirect = &s.xattrs["trusted.overlay.redirect"];
                assert!(redirect.starts_with(b"/"), "{path}");
                assert!(
                    f.objects_dir()
                        .join(std::str::from_utf8(&redirect[1..]).unwrap())
                        .is_file()
                );
                assert_eq!(s.xattrs["trusted.overlay.metacopy"], b"");
            }
        }
        let strip = |m: BTreeMap<String, Seen>| -> BTreeMap<String, Seen> {
            without_overlay_xattrs(m)
                .into_iter()
                .map(|(k, mut s)| {
                    s.sha = None;
                    (k, s)
                })
                .collect()
        };
        assert_eq!(strip(seen), strip(expected));
    }

    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn the_loop_fallback_mounts_as_root() {
        let f = fixture();
        let mnt = f.tmp.path().join("mnt");
        std::fs::create_dir(&mnt).unwrap();
        let image = f.image.clone();
        let (by_loop, devices) = in_private_mount_ns(|| {
            mount_erofs_loop(&image, &mnt).map_err(|e| crate::Error::io("mount", &e, ""))?;
            let seen = walk(&mnt, false);
            let devices = loop_devices_for(&image);
            rustix::mount::unmount(&mnt, rustix::mount::UnmountFlags::empty())
                .map_err(|e| crate::Error::io("unmount", &e.into(), ""))?;
            Ok((seen, devices))
        })
        .unwrap();
        assert_eq!(devices.len(), 1, "{devices:?}");
        let by_file = in_private_mount_ns(|| {
            mount_erofs(&image, &mnt).map_err(|e| crate::Error::io("mount", &e, ""))?;
            Ok(walk(&mnt, false))
        })
        .unwrap();
        assert_eq!(by_loop, by_file);
        // The loop device goes away with its mount.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !loop_devices_for(&image).is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(loop_devices_for(&image), Vec::<String>::new());
    }

    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn composefs_matches_the_hardlink_tree_as_root() {
        let f = fixture();
        let expected = hardlink_view(&f);
        let scratch = f.tmp.path().join("scratch");
        let (seen, groups) = with_mounted(&f.image, &f.objects_dir(), &scratch, |root| {
            walk(root, true)
        })
        .unwrap();
        assert_eq!(groups, expected.1);
        assert_eq!(without_overlay_xattrs(seen), expected.0);
    }

    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn copy_up_reads_contents_from_objects_as_root() {
        let f = fixture();
        let scratch = f.tmp.path().join("scratch");
        let got = with_mounted(&f.image, &f.objects_dir(), &scratch, |root| {
            use std::io::Write;
            let append = |rel: &str, extra: &[u8]| {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(root.join(rel))
                    .unwrap()
                    .write_all(extra)
                    .unwrap();
            };
            append("etc/conf", b"y=2\n");
            std::fs::OpenOptions::new()
                .write(true)
                .open(root.join("big"))
                .unwrap()
                .set_len(3)
                .unwrap();
            std::fs::set_permissions(
                root.join("bin/tool"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            let after_chmod = std::fs::read(root.join("bin/tool")).unwrap();
            append("bin/tool", b"!");
            (
                std::fs::read(root.join("etc/conf")).unwrap(),
                std::fs::read(root.join("big")).unwrap(),
                after_chmod,
                std::fs::read(root.join("bin/tool")).unwrap(),
            )
        })
        .unwrap();
        assert_eq!(got.0, b"x=1\ny=2\n");
        assert_eq!(got.1, vec![0, 1, 2]);
        assert_eq!(got.2, b"tool");
        assert_eq!(got.3, b"tool!");
    }

    /// The kernel finds every odd name (binary search across directory blocks), walks a
    /// deep tree, and reads the end of a file over 4 GiB through the data-only layer.
    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn odd_names_deep_trees_and_huge_files_through_the_kernel_as_root() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let objects = ObjectStore::new(objects_dir.clone(), true).unwrap();
        let meta = Meta {
            uid: 0,
            gid: 0,
            mode: 0o644,
            xattrs: vec![],
        };
        let (small, _) = objects.insert(&mut &b"x"[..], &meta, None).unwrap();
        let (leaf, _) = objects.insert(&mut &b"leaf"[..], &meta, None).unwrap();
        // A sparse 5 GiB object; overlayfs does not check contents against the key.
        let huge = format!("cd{}.0.0.644", "0".repeat(62));
        let huge_path = objects_dir.join(ObjectStore::relative_path(&huge));
        std::fs::create_dir_all(huge_path.parent().unwrap()).unwrap();
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::File::create(&huge_path).unwrap();
            f.set_len(5 << 30).unwrap();
            f.seek(SeekFrom::Start((5 << 30) - 4)).unwrap();
            f.write_all(b"tail").unwrap();
        }
        let names = crate::store::erofs::odd_names();
        let file = |path: String, object: &str, size: u64| Entry {
            path,
            kind: EntryKind::File {
                object: object.to_string(),
                size,
            },
            mode: 0o644,
            uid: 0,
            gid: 0,
            xattrs: vec![],
            mtime: None,
        };
        let mut entries: Vec<Entry> = names
            .iter()
            .map(|n| file(format!("odd/{n}"), &small, 1))
            .collect();
        let deep = format!("deep/{}leaf", "d/".repeat(200));
        entries.push(file(deep.clone(), &leaf, 4));
        entries.push(file("huge".into(), &huge, 5 << 30));
        let tree = merge(&[entries]).unwrap();
        let image = tmp.path().join("image.erofs");
        std::fs::write(&image, crate::store::erofs::build(&tree, 1).unwrap()).unwrap();
        let scratch = tmp.path().join("scratch");
        let (missing, listed, leaf_text, size, tail) =
            with_mounted(&image, &objects_dir, &scratch, |root| {
                use std::io::{Read, Seek, SeekFrom};
                let missing: Vec<String> = names
                    .iter()
                    .filter(|n| std::fs::symlink_metadata(root.join("odd").join(n)).is_err())
                    .cloned()
                    .collect();
                let listed = std::fs::read_dir(root.join("odd")).unwrap().count();
                let leaf_text = std::fs::read(root.join(&deep)).unwrap();
                let mut f = std::fs::File::open(root.join("huge")).unwrap();
                let size = f.metadata().unwrap().len();
                f.seek(SeekFrom::End(-4)).unwrap();
                let mut tail = [0u8; 4];
                f.read_exact(&mut tail).unwrap();
                (missing, listed, leaf_text, size, tail)
            })
            .unwrap();
        assert_eq!(missing, Vec::<String>::new());
        assert_eq!(listed, names.len());
        assert_eq!(leaf_text, b"leaf");
        assert_eq!(size, 5 << 30);
        assert_eq!(&tail, b"tail");
    }

    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn probe_passes_on_this_kernel_as_root() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = tmp.path().join("probe");
        assert_eq!(probe(&scratch), Ok(()));
        assert!(!scratch.exists());
    }

    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn shared_mounts_are_made_once_as_root() {
        let f = fixture();
        let target = f.tmp.path().join("run/images/abc");
        let image = f.image.clone();
        let (after, gone) = in_private_mount_ns(|| {
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..4)
                    .map(|_| s.spawn(|| mount_shared(&image, &target)))
                    .collect();
                for h in handles {
                    h.join().unwrap().unwrap();
                }
            });
            mount_shared(&image, &target)?;
            let after = mount_count(&target);
            unmount_shared(&target).map_err(|e| crate::Error::io("unmount", &e, ""))?;
            Ok((after, mount_count(&target)))
        })
        .unwrap();
        assert_eq!(after, 1);
        assert_eq!(gone, 0);
        assert!(!target.exists());
        assert!(!target.with_extension("lock").exists());
    }
}
