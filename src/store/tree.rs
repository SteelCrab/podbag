//! Merging layers into one image tree (overlay semantics) and building it on disk.

use super::layer::{Entry, EntryKind, unhex};
use super::object::{ObjectStore, io_error};
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
                e.kind = match map.get(target).map(|t| &t.kind) {
                    Some(EntryKind::File { object, size }) => EntryKind::File {
                        object: object.clone(),
                        size: *size,
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

/// Build `tree` at `dest` (which must not exist). Files are hard links to objects.
/// Every parent is a directory this function created, so no symlink is ever followed.
pub fn materialize(
    tree: &Tree,
    objects: &ObjectStore,
    dest: &Path,
    can_mknod: bool,
) -> Result<MaterializeReport> {
    std::fs::create_dir(dest).map_err(|e| io_error(dest, e))?;
    let result = (|| {
        use std::os::unix::fs::PermissionsExt;
        // The image root is 0755 regardless of the umask, like Docker's.
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| io_error(dest, e))?;
        build(tree, objects, dest, can_mknod)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(dest);
    }
    result
}

fn build(
    tree: &Tree,
    objects: &ObjectStore,
    dest: &Path,
    can_mknod: bool,
) -> Result<MaterializeReport> {
    use std::os::unix::fs::PermissionsExt;
    let set_owner = objects.sets_owner();
    let mut report = MaterializeReport::default();
    let mut dir_modes = Vec::new();
    for (path, e) in &tree.entries {
        let p = dest.join(path);
        let fail = |err: std::io::Error| io_error(&p, err);
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
                dir_modes.push((p, e.mode));
            }
            EntryKind::File { object, .. } => {
                link_or_copy(&objects.path(object), &p, e, set_owner, |a, b| {
                    std::fs::hard_link(a, b)
                })
                .map_err(fail)?;
            }
            EntryKind::Symlink { target } => {
                std::os::unix::fs::symlink(target, &p).map_err(fail)?;
                if set_owner {
                    std::os::unix::fs::lchown(&p, Some(e.uid), Some(e.gid)).map_err(fail)?;
                }
            }
            EntryKind::Fifo => {
                make_node(&p, libc::S_IFIFO as u32 | e.mode, 0).map_err(fail)?;
                if set_owner {
                    std::os::unix::fs::chown(&p, Some(e.uid), Some(e.gid)).map_err(fail)?;
                }
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(e.mode))
                    .map_err(fail)?;
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
            }
            EntryKind::Hardlink { .. } | EntryKind::Whiteout | EntryKind::Opaque => {}
        }
    }
    for (p, mode) in dir_modes.iter().rev() {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(*mode))
            .map_err(|e| io_error(p, e))?;
    }
    Ok(report)
}

/// Hard-link the object into the tree. When the object has as many links as the file
/// system allows (EMLINK), copy it instead and give the copy the entry's metadata.
pub(crate) fn link_or_copy(
    object: &Path,
    dest: &Path,
    e: &Entry,
    set_owner: bool,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match link(object, dest) {
        Err(err) if err.raw_os_error() == Some(libc::EMLINK) => {
            std::fs::copy(object, dest)?;
            if set_owner {
                std::os::unix::fs::chown(dest, Some(e.uid), Some(e.gid))?;
                set_xattrs(dest, e)?;
            }
            // After chown, which clears set-user-ID and set-group-ID bits.
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(e.mode))
        }
        other => other,
    }
}

trait Mode700 {
    fn mode_700(&mut self) -> &mut Self;
}
impl Mode700 for std::fs::DirBuilder {
    fn mode_700(&mut self) -> &mut Self {
        use std::os::unix::fs::DirBuilderExt;
        self.mode(0o700)
    }
}

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

#[cfg(not(target_os = "linux"))]
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
        let (key, _) = objects.insert(&mut &b"hello"[..], &meta).unwrap();
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
        let report = materialize(&t, &objects, &dest, false).unwrap();
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
        let (key, _) = objects.insert(&mut &b"x"[..], &meta).unwrap();
        let mut ro = dir("ro");
        ro.mode = 0o555;
        let t = merge(&[vec![ro, file("ro/f", &key)]]).unwrap();
        let dest = tmp.path().join("rootfs");
        materialize(&t, &objects, &dest, false).unwrap();
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
        let (key, _) = objects.insert(&mut &b"content"[..], &meta).unwrap();
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

    #[test]
    fn materialize_refuses_an_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = store(tmp.path());
        let t = merge(&[vec![dir("a")]]).unwrap();
        assert!(materialize(&t, &objects, tmp.path(), false).is_err());
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
