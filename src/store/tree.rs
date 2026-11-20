//! Merging layers into one image tree (overlay semantics) and building it on disk.

#[cfg(unix)]
use super::layer::unhex;
use super::layer::{Entry, EntryKind};
use super::object::ObjectStore;
#[cfg(unix)]
use super::object::io_error;
use crate::{Error, ErrorKind, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// The merged file tree of an image, keyed by path. Parents sort before children.
#[derive(Debug, Default)]
pub struct Tree {
    pub entries: BTreeMap<String, Entry>,
}

/// Apply layers in order, like overlayfs: whiteouts and opaque markers first, then the
/// layer's own entries.
pub fn merge(layers: &[Vec<Entry>]) -> Result<Tree> {
    let mut map: BTreeMap<String, Entry> = BTreeMap::new();
    for layer in layers {
        for e in layer {
            match e.kind {
                EntryKind::Whiteout => remove_subtree(&mut map, &e.path, true),
                EntryKind::Opaque => remove_subtree(&mut map, &e.path, false),
                _ => {}
            }
        }
        for e in layer {
            if matches!(e.kind, EntryKind::Whiteout | EntryKind::Opaque) {
                continue;
            }
            let mut e = e.clone();
            if let EntryKind::Hardlink { target } = &e.kind {
                // The same inode: the target's contents, metadata and time.
                e = match map.get(target) {
                    Some(t) if matches!(t.kind, EntryKind::File { .. }) => Entry {
                        path: e.path,
                        ..t.clone()
                    },
                    _ => {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "cannot read image layer",
                            format!("hard link {} points to a missing file {target}", e.path),
                            "the image is malformed; do not use it",
                        ));
                    }
                };
            }
            ensure_parents(&mut map, &e.path);
            let replaces_dir = map
                .get(&e.path)
                .is_some_and(|old| old.kind == EntryKind::Dir);
            if replaces_dir && e.kind != EntryKind::Dir {
                remove_subtree(&mut map, &e.path, false);
            }
            map.insert(e.path.clone(), e);
        }
    }
    Ok(Tree { entries: map })
}

/// Remove everything below `path` (and `path` itself when `include_self`). "" is the root.
fn remove_subtree(map: &mut BTreeMap<String, Entry>, path: &str, include_self: bool) {
    if path.is_empty() {
        map.clear();
        return;
    }
    let prefix = format!("{path}/");
    let doomed: Vec<String> = map
        .range(path.to_string()..)
        .map(|(k, _)| k)
        .take_while(|k| {
            k.as_str() == path || k.starts_with(&prefix) || k.as_str() < prefix.as_str()
        })
        .filter(|k| (include_self && k.as_str() == path) || k.starts_with(&prefix))
        .cloned()
        .collect();
    for k in doomed {
        map.remove(&k);
    }
}

/// Every ancestor of `path` must be a directory. A missing one is created; a non-directory
/// one is replaced, as an upper directory replaces a lower file in overlayfs.
fn ensure_parents(map: &mut BTreeMap<String, Entry>, path: &str) {
    let mut end = 0;
    while let Some(i) = path[end..].find('/') {
        let parent = &path[..end + i];
        if map.get(parent).is_none_or(|p| p.kind != EntryKind::Dir) {
            map.insert(
                parent.to_string(),
                Entry {
                    path: parent.to_string(),
                    kind: EntryKind::Dir,
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    xattrs: vec![],
                    mtime: None,
                },
            );
        }
        end += i + 1;
    }
}

#[derive(Debug, Default)]
pub struct MaterializeReport {
    /// Device nodes that could not be created (rootless mode cannot create them).
    pub skipped_devices: u32,
}

/// Files whose programs refuse a file with more than one link: cron skips such a
/// crontab (another name could let someone else change it). Hard-link trees give these
/// files a copy of their own, and EROFS an inode of their own.
pub fn needs_own_inode(path: &str) -> bool {
    path == "etc/crontab" || path.starts_with("etc/cron.d/") || path.starts_with("var/spool/cron/")
}

#[cfg(unix)]
/// Build `tree` at `dest` (which must not exist). Files are hard links to objects.
/// Every parent is a directory this function created, so no symlink is ever followed.
/// The root, and entries without a time (made by merge(), or from a layer stored by an
/// older podbag), get `default_mtime`; such files keep their object's time.
pub fn materialize(
    tree: &Tree,
    objects: &ObjectStore,
    dest: &Path,
    can_mknod: bool,
    default_mtime: u64,
) -> Result<MaterializeReport> {
    std::fs::create_dir(dest).map_err(|e| io_error(dest, e))?;
    let result = (|| {
        use std::os::unix::fs::PermissionsExt;
        // The image root is 0755 regardless of the umask, like Docker's.
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| io_error(dest, e))?;
        let report = build(tree, objects, dest, can_mknod, default_mtime)?;
        set_path_mtime(dest, default_mtime).map_err(|e| io_error(dest, e))?;
        Ok(report)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(dest);
    }
    result
}

#[cfg(unix)]
fn build(
    tree: &Tree,
    objects: &ObjectStore,
    dest: &Path,
    can_mknod: bool,
    default_mtime: u64,
) -> Result<MaterializeReport> {
    use std::os::unix::fs::PermissionsExt;
    let set_owner = objects.sets_owner();
    let mut report = MaterializeReport::default();
    let mut dirs = Vec::new();
    for (path, e) in &tree.entries {
        let p = dest.join(path);
        let fail = |err: std::io::Error| io_error(&p, err);
        let mtime = e.mtime.unwrap_or(default_mtime);
        match &e.kind {
            EntryKind::Dir => {
                std::fs::DirBuilder::new()
                    .mode_700()
                    .create(&p)
                    .map_err(fail)?;
                if set_owner {
                    std::os::unix::fs::chown(&p, Some(e.uid), Some(e.gid)).map_err(fail)?;
                    set_xattrs(&p, e).map_err(fail)?;
                }
                dirs.push((p, e.mode, mtime));
            }
            EntryKind::File { object, .. } if needs_own_inode(path) => {
                copy_file(&objects.path(object), &p, e, set_owner, e.mtime).map_err(fail)?;
            }
            EntryKind::File { object, .. } => {
                // The object, or its copy with this file's time. A layer stored by an older
                // podbag has no times: its files keep the object's.
                let source = match e.mtime {
                    Some(t) => objects.with_mtime(object, &meta_of(e), t)?,
                    None => objects.path(object),
                };
                link_or_copy(&source, &p, e, set_owner, |a, b| std::fs::hard_link(a, b))
                    .map_err(fail)?;
            }
            EntryKind::Symlink { target } => {
                std::os::unix::fs::symlink(target, &p).map_err(fail)?;
                if set_owner {
                    std::os::unix::fs::lchown(&p, Some(e.uid), Some(e.gid)).map_err(fail)?;
                }
                set_path_mtime(&p, mtime).map_err(fail)?;
            }
            EntryKind::Fifo => {
                make_node(&p, libc::S_IFIFO as u32 | e.mode, 0).map_err(fail)?;
                if set_owner {
                    std::os::unix::fs::chown(&p, Some(e.uid), Some(e.gid)).map_err(fail)?;
                }
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(e.mode))
                    .map_err(fail)?;
                set_path_mtime(&p, mtime).map_err(fail)?;
            }
            EntryKind::Char { major, minor } | EntryKind::Block { major, minor } => {
                if !can_mknod {
                    report.skipped_devices += 1;
                    continue;
                }
                let kind = if matches!(e.kind, EntryKind::Char { .. }) {
                    libc::S_IFCHR
                } else {
                    libc::S_IFBLK
                };
                make_node(
                    &p,
                    kind as u32 | e.mode,
                    libc::makedev(*major as _, *minor as _) as u64,
                )
                .map_err(fail)?;
                std::os::unix::fs::chown(&p, Some(e.uid), Some(e.gid)).map_err(fail)?;
                // mknod applied the umask.
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(e.mode))
                    .map_err(fail)?;
                set_path_mtime(&p, mtime).map_err(fail)?;
            }
            EntryKind::Hardlink { .. } | EntryKind::Whiteout | EntryKind::Opaque => {}
        }
    }
    // Deepest first, after their children: a new child changes a directory's time.
    for (p, mode, mtime) in dirs.iter().rev() {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(*mode))
            .and_then(|()| set_path_mtime(p, *mtime))
            .map_err(|e| io_error(p, e))?;
    }
    Ok(report)
}

/// An entry's metadata as an object's: since merge() gave hard links their target's,
/// it is the metadata of the entry's object.
#[cfg(unix)]
fn meta_of(e: &Entry) -> super::object::Meta {
    super::object::Meta {
        uid: e.uid,
        gid: e.gid,
        mode: e.mode,
        xattrs: e
            .xattrs
            .iter()
            .map(|(k, v)| (k.clone(), unhex(v)))
            .collect(),
    }
}

#[cfg(unix)]
/// Hard-link the object into the tree. When the object has as many links as the file
/// system allows (EMLINK), copy it instead.
pub(crate) fn link_or_copy(
    object: &Path,
    dest: &Path,
    e: &Entry,
    set_owner: bool,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match link(object, dest) {
        Err(err) if err.raw_os_error() == Some(libc::EMLINK) => {
            copy_file(object, dest, e, set_owner, None)
        }
        other => other,
    }
}

#[cfg(unix)]
/// Copy the object into the tree, with the entry's metadata, and `mtime` (None: the
/// object's time).
fn copy_file(
    object: &Path,
    dest: &Path,
    e: &Entry,
    set_owner: bool,
    mtime: Option<u64>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::copy(object, dest)?;
    if set_owner {
        std::os::unix::fs::chown(dest, Some(e.uid), Some(e.gid))?;
        set_xattrs(dest, e)?;
    }
    // After chown, which clears set-user-ID and set-group-ID bits.
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(e.mode))?;
    match mtime.or(super::object::mtime_of(&std::fs::metadata(object)?)) {
        Some(t) => set_path_mtime(dest, t),
        None => Ok(()),
    }
}

#[cfg(unix)]
/// Set the access and modification times of `path` (not of a symlink's target).
fn set_path_mtime(path: &Path, secs: u64) -> std::io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(std::io::Error::other)?;
    let t = libc::timespec {
        // Below i64::MAX: the layer reader cuts larger times.
        tv_sec: secs as _,
        tv_nsec: 0,
    };
    let times = [t, t];
    // SAFETY: a NUL-terminated path and an array of two timespecs, as utimensat reads.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
trait Mode700 {
    fn mode_700(&mut self) -> &mut Self;
}
#[cfg(unix)]
impl Mode700 for std::fs::DirBuilder {
    fn mode_700(&mut self) -> &mut Self {
        use std::os::unix::fs::DirBuilderExt;
        self.mode(0o700)
    }
}

#[cfg(unix)]
fn make_node(path: &Path, mode: u32, dev: u64) -> std::io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(std::io::Error::other)?;
    // SAFETY: NUL-terminated path; mknod has no other memory arguments.
    if unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, dev as libc::dev_t) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn set_xattrs(path: &Path, e: &Entry) -> std::io::Result<()> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(std::io::Error::other)?;
    for (name, hex) in &e.xattrs {
        let value = unhex(hex);
        let c_name = std::ffi::CString::new(name.as_str()).map_err(std::io::Error::other)?;
        // SAFETY: NUL-terminated strings and a value buffer of the given length.
        let rc = unsafe {
            libc::lsetxattr(
                c_path.as_ptr(),
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

/// Images cannot be unpacked on this system.
#[cfg(not(unix))]
pub fn materialize(
    _tree: &Tree,
    _objects: &ObjectStore,
    _dest: &Path,
    _can_mknod: bool,
    _default_mtime: u64,
) -> Result<MaterializeReport> {
    Err(crate::Error::unsupported_os())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn set_xattrs(_path: &Path, e: &Entry) -> std::io::Result<()> {
    let _ = unhex;
    let _ = e;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::store::layer::{Entry, EntryKind};
    use crate::store::object::{Meta, ObjectStore};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn e(path: &str, kind: EntryKind) -> Entry {
        Entry {
            path: path.into(),
            kind,
            mode: 0o755,
            uid: 0,
            gid: 0,
            xattrs: vec![],
            mtime: None,
        }
    }
    fn dir(path: &str) -> Entry {
        e(path, EntryKind::Dir)
    }
    fn file(path: &str, object: &str) -> Entry {
        e(
            path,
            EntryKind::File {
                object: object.into(),
                size: 1,
            },
        )
    }
    fn paths(t: &Tree) -> Vec<&str> {
        t.entries.keys().map(String::as_str).collect()
    }

    #[test]
    fn whiteout_removes_lower_files_and_children() {
        let t = merge(&[
            vec![
                dir("a"),
                file("a/x", "o1"),
                dir("a/sub"),
                file("a/sub/y", "o2"),
                file("b", "o3"),
            ],
            vec![e("a/sub", EntryKind::Whiteout), e("b", EntryKind::Whiteout)],
        ])
        .unwrap();
        assert_eq!(paths(&t), ["a", "a/x"]);
    }

    #[test]
    fn opaque_hides_lower_children_but_keeps_new_ones() {
        let t = merge(&[
            vec![dir("opt"), file("opt/a", "o1"), file("opt/z", "o2")],
            vec![e("opt", EntryKind::Opaque), file("opt/b", "o3")],
        ])
        .unwrap();
        assert_eq!(paths(&t), ["opt", "opt/b"]);
    }

    #[test]
    fn merge_replaces_a_non_directory_parent() {
        let t = merge(&[
            vec![e(
                "etc",
                EntryKind::Symlink {
                    target: "/tmp".into(),
                },
            )],
            vec![file("etc/passwd", "o1")],
        ])
        .unwrap();
        assert_eq!(t.entries["etc"].kind, EntryKind::Dir);
        assert!(matches!(
            t.entries["etc/passwd"].kind,
            EntryKind::File { .. }
        ));
    }

    #[test]
    fn missing_parents_are_created() {
        let t = merge(&[vec![file("usr/local/bin/tool", "o1")]]).unwrap();
        assert_eq!(
            paths(&t),
            ["usr", "usr/local", "usr/local/bin", "usr/local/bin/tool"]
        );
        assert_eq!(t.entries["usr/local"].mode, 0o755);
    }

    #[test]
    fn directory_replaced_by_a_file_drops_its_children() {
        let t = merge(&[vec![dir("d"), file("d/x", "o1")], vec![file("d", "o2")]]).unwrap();
        assert_eq!(paths(&t), ["d"]);
    }

    #[test]
    fn hardlink_becomes_the_same_object() {
        let t = merge(&[
            vec![file("h1", "obj")],
            vec![e(
                "h2",
                EntryKind::Hardlink {
                    target: "h1".into(),
                },
            )],
        ])
        .unwrap();
        assert_eq!(
            t.entries["h2"].kind,
            EntryKind::File {
                object: "obj".into(),
                size: 1
            }
        );
    }

    /// A hard link is its target's inode: whatever its own header says, it has the
    /// target's owner, mode, xattrs and time (the tree links it to the target's object).
    #[test]
    fn hardlink_takes_its_targets_metadata_and_time() {
        let mut target = file("h1", "obj");
        (target.mode, target.uid, target.gid) = (0o4755, 101, 102);
        target.xattrs = vec![("user.a".into(), "01".into())];
        target.mtime = Some(100);
        let mut link = e(
            "h2",
            EntryKind::Hardlink {
                target: "h1".into(),
            },
        );
        (link.mode, link.mtime) = (0o644, Some(200));
        let t = merge(&[vec![target], vec![link]]).unwrap();
        let h2 = &t.entries["h2"];
        assert_eq!((h2.mode, h2.uid, h2.gid), (0o4755, 101, 102));
        assert_eq!(h2.xattrs, [("user.a".to_string(), "01".to_string())]);
        assert_eq!(h2.mtime, Some(100));
    }

    #[test]
    fn hardlink_to_a_missing_file_is_rejected() {
        let err = merge(&[vec![e(
            "h2",
            EntryKind::Hardlink {
                target: "nope".into(),
            },
        )]])
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidInput);
        assert!(
            err.why.contains("h2") && err.why.contains("nope"),
            "{}",
            err.why
        );
    }

    fn store(tmp: &std::path::Path) -> ObjectStore {
        ObjectStore::new(tmp.join("objects"), false).unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs root: dev/vm.sh test-root"]
    fn device_nodes_keep_their_mode_under_a_umask_as_root() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mut null = e("null", EntryKind::Char { major: 1, minor: 3 });
        null.mode = 0o666;
        let tree = merge(&[vec![null]]).unwrap();
        // SAFETY: umask has no memory arguments.
        let old = unsafe { libc::umask(0o022) };
        let built = materialize(&tree, &store(tmp.path()), &tmp.path().join("r"), true, 0);
        // SAFETY: as above.
        unsafe { libc::umask(old) };
        built.unwrap();
        let mode = std::fs::symlink_metadata(tmp.path().join("r/null"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o666);
    }

    #[test]
    fn materialize_builds_the_tree_without_following_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = store(tmp.path());
        let meta = Meta {
            uid: 0,
            gid: 0,
            mode: 0o644,
            xattrs: vec![],
        };
        let (key, _) = objects.insert(&mut &b"hello"[..], &meta, None).unwrap();
        let t = merge(&[vec![
            dir("a"),
            file("a/f", &key),
            e(
                "a/link",
                EntryKind::Symlink {
                    target: "/etc".into(),
                },
            ),
            file("h1", &key),
            e(
                "h2",
                EntryKind::Hardlink {
                    target: "h1".into(),
                },
            ),
            e("p", EntryKind::Fifo),
        ]])
        .unwrap();
        let dest = tmp.path().join("rootfs");
        let report = materialize(&t, &objects, &dest, false, 0).unwrap();
        assert_eq!(report.skipped_devices, 0);
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(std::fs::read(dest.join("a/f")).unwrap(), b"hello");
        assert!(
            std::fs::symlink_metadata(dest.join("a/link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(dest.join("a/link")).unwrap(),
            std::path::Path::new("/etc")
        );
        let ino = |p: &str| std::fs::metadata(dest.join(p)).unwrap().ino();
        assert_eq!(ino("h1"), ino("h2"));
        assert_eq!(
            ino("h1"),
            std::fs::metadata(objects.path(&key)).unwrap().ino()
        );
        assert!(
            std::fs::metadata(dest.join("p"))
                .unwrap()
                .file_type()
                .is_fifo_like()
        );
    }

    #[test]
    fn materialize_applies_readonly_directory_modes_last() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = store(tmp.path());
        let meta = Meta {
            uid: 0,
            gid: 0,
            mode: 0o644,
            xattrs: vec![],
        };
        let (key, _) = objects.insert(&mut &b"x"[..], &meta, None).unwrap();
        let mut ro = dir("ro");
        ro.mode = 0o555;
        let t = merge(&[vec![ro, file("ro/f", &key)]]).unwrap();
        let dest = tmp.path().join("rootfs");
        materialize(&t, &objects, &dest, false, 0).unwrap();
        let mode = std::fs::metadata(dest.join("ro"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o555);
        std::fs::set_permissions(dest.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // ext4 allows about 65000 links per inode. Empty files with the same owner and mode
    // (Python's __init__.py) reach that across many images; then the file is copied.
    #[test]
    fn a_full_link_count_falls_back_to_a_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = store(tmp.path());
        let meta = Meta {
            uid: 0,
            gid: 0,
            mode: 0o640,
            xattrs: vec![],
        };
        let (key, _) = objects.insert(&mut &b"content"[..], &meta, None).unwrap();
        let entry = file("f", &key);
        let dest = tmp.path().join("f");
        let too_many = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::EMLINK));
        link_or_copy(&objects.path(&key), &dest, &entry, false, too_many).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"content");
        assert_ne!(
            std::fs::metadata(&dest).unwrap().ino(),
            std::fs::metadata(objects.path(&key)).unwrap().ino()
        );
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o7777,
            0o755
        );
    }

    fn timed(mut e: Entry, t: u64) -> Entry {
        e.mtime = Some(t);
        e
    }

    /// Every entry gets its own time; directories after their children (adding a child
    /// changes a directory's time). Entries without one, and the root, get the default.
    #[test]
    fn materialize_gives_every_entry_its_time() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = store(tmp.path());
        let meta = Meta {
            uid: 0,
            gid: 0,
            mode: 0o755,
            xattrs: vec![],
        };
        let (key, _) = objects.insert(&mut &b"same"[..], &meta, Some(100)).unwrap();
        let t = merge(&[vec![
            timed(dir("d"), 10),
            timed(file("d/f", &key), 100),
            timed(file("g", &key), 200),
            timed(
                e(
                    "l",
                    EntryKind::Symlink {
                        target: "d/f".into(),
                    },
                ),
                30,
            ),
            timed(e("p", EntryKind::Fifo), 40),
            dir("untimed"),
        ]])
        .unwrap();
        let dest = tmp.path().join("rootfs");
        materialize(&t, &objects, &dest, false, 5).unwrap();
        let at = |p: &str| std::fs::symlink_metadata(dest.join(p)).unwrap().mtime();
        assert_eq!(
            ["", "d", "d/f", "g", "l", "p", "untimed"].map(at),
            [5, 10, 100, 200, 30, 40, 5]
        );
        // The file whose time is the object's links it; the other one gets a copy.
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        assert_eq!(ino(&dest.join("d/f")), ino(&objects.path(&key)));
        assert_ne!(ino(&dest.join("g")), ino(&dest.join("d/f")));
        assert_eq!(std::fs::read(dest.join("g")).unwrap(), b"same");
    }

    /// cron skips a crontab that has more than one link: crontabs get an inode of
    /// their own, while other files keep sharing the object.
    #[test]
    fn crontabs_get_an_inode_of_their_own() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = store(tmp.path());
        let meta = Meta {
            uid: 0,
            gid: 0,
            mode: 0o755,
            xattrs: vec![],
        };
        let job = b"* * * * * root true\n";
        let (key, _) = objects.insert(&mut &job[..], &meta, Some(100)).unwrap();
        let paths = [
            "etc/crontab",
            "etc/cron.d/job",
            "var/spool/cron/crontabs/root",
            "var/spool/cron/root",
            "usr/share/doc/crontab",
        ];
        let t = merge(&[paths.iter().map(|p| timed(file(p, &key), 100)).collect()]).unwrap();
        let dest = tmp.path().join("rootfs");
        materialize(&t, &objects, &dest, false, 5).unwrap();
        let links = |p: &str| std::fs::metadata(dest.join(p)).unwrap().nlink();
        assert_eq!(paths.map(links), [1, 1, 1, 1, 2]);
        for p in &paths[..4] {
            let m = std::fs::metadata(dest.join(p)).unwrap();
            assert_eq!(std::fs::read(dest.join(p)).unwrap(), job, "{p}");
            assert_eq!((m.mtime(), m.permissions().mode() & 0o7777), (100, 0o755));
        }
    }

    /// A crontab is a private copy: it needs no shared time copy in the store.
    #[test]
    fn a_crontab_with_another_time_makes_no_time_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = store(tmp.path());
        let meta = Meta {
            uid: 0,
            gid: 0,
            mode: 0o755,
            xattrs: vec![],
        };
        let job = b"* * * * * root true\n";
        let (key, _) = objects.insert(&mut &job[..], &meta, Some(100)).unwrap();
        let t = merge(&[vec![timed(file("etc/cron.d/job", &key), 200)]]).unwrap();
        let dest = tmp.path().join("rootfs");
        materialize(&t, &objects, &dest, false, 5).unwrap();
        assert_eq!(objects.total_size().unwrap().0, 1);
        let m = std::fs::metadata(dest.join("etc/cron.d/job")).unwrap();
        assert_eq!((m.mtime(), m.nlink()), (200, 1));
    }

    #[test]
    fn only_crontabs_need_an_inode_of_their_own() {
        let cases = [
            ("etc/crontab", true),
            ("etc/cron.d/job", true),
            ("var/spool/cron/crontabs/root", true),
            ("var/spool/cron/root", true),
            ("etc/crontab.bak", false),
            ("etc/cron.daily/logrotate", false),
            ("etc/cron.dx/job", false),
            ("etc/crontabs/root", false),
            ("usr/etc/crontab", false),
        ];
        for (path, want) in cases {
            assert_eq!(needs_own_inode(path), want, "{path}");
        }
    }

    #[test]
    fn materialize_refuses_an_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = store(tmp.path());
        let t = merge(&[vec![dir("a")]]).unwrap();
        assert!(materialize(&t, &objects, tmp.path(), false, 0).is_err());
    }

    trait FifoLike {
        fn is_fifo_like(&self) -> bool;
    }
    impl FifoLike for std::fs::FileType {
        fn is_fifo_like(&self) -> bool {
            use std::os::unix::fs::FileTypeExt;
            self.is_fifo()
        }
    }
}
