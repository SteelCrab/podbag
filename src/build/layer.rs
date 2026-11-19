//! A container's changes, read from its overlay upper directory: as a layer tar (for
//! builds and `commit`), or as a list (`diff`).
//!
//! The upper directory tells what changed: a character device 0/0 is a deleted path
//! (written as a `.wh.<name>` entry), a directory with the overlay's `opaque` attribute
//! hides what the image had there (`.wh..wh..opq`), and a directory with a `redirect`
//! attribute was renamed from an image directory. File contents and metadata come from the
//! merged view: with composefs, an upper file may hold only metadata (metacopy).

pub use super::Change;
use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

/// Where overlayfs keeps its attributes: `trusted.` as root, `user.` in a user namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XattrSpace {
    Trusted,
    User,
}

impl XattrSpace {
    pub fn for_rootless(rootless: bool) -> XattrSpace {
        if rootless {
            XattrSpace::User
        } else {
            XattrSpace::Trusted
        }
    }

    fn name(self, what: &str) -> String {
        match self {
            XattrSpace::Trusted => format!("trusted.overlay.{what}"),
            XattrSpace::User => format!("user.overlay.{what}"),
        }
    }
}

/// Write the changes in `upper` as a layer tar to `out`, reading what files hold from
/// `merged`.
pub fn write_layer(
    upper: &Path,
    merged: &Path,
    space: XattrSpace,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let mut w = Writer {
        tar: tar::Builder::new(out),
        links: HashMap::new(),
    };
    w.upper_dir(upper, merged, "", space)?;
    w.tar.finish()
}

/// All of a container's files under `root` as a tar (`export`). The directories in `skip`
/// (relative paths: the mounts of a running container) are written empty.
pub fn write_tree(root: &Path, skip: &[String], out: &mut dyn Write) -> std::io::Result<()> {
    let mut w = Writer {
        tar: tar::Builder::new(out),
        links: HashMap::new(),
    };
    w.tree(root, "", skip)?;
    w.tar.finish()
}

/// The changes in `upper`: added when `lower` (the image) lacks the path.
pub fn changes(upper: &Path, lower: &Path) -> std::io::Result<Vec<Change>> {
    let mut out = Vec::new();
    list_changes(upper, lower, "", &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn list_changes(
    upper: &Path,
    lower: &Path,
    rel: &str,
    out: &mut Vec<Change>,
) -> std::io::Result<()> {
    for (name, meta) in children(&upper.join(rel))? {
        let child = join(rel, &name);
        let path = format!("/{child}");
        if is_whiteout(&meta) {
            out.push(Change { path, kind: 2 });
            continue;
        }
        let in_image = std::fs::symlink_metadata(lower.join(&child)).is_ok();
        out.push(Change {
            path,
            kind: if in_image { 0 } else { 1 },
        });
        if meta.is_dir() {
            list_changes(upper, lower, &child, out)?;
        }
    }
    Ok(())
}

/// A deleted path in an overlay upper directory.
pub fn is_whiteout(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_char_device() && meta.rdev() == 0
}

struct Writer<'a> {
    tar: tar::Builder<&'a mut dyn Write>,
    /// (device, inode) of files written, for hard links.
    links: HashMap<(u64, u64), String>,
}

impl Writer<'_> {
    /// The entries below `rel` in the upper directory.
    fn upper_dir(
        &mut self,
        upper: &Path,
        merged: &Path,
        rel: &str,
        space: XattrSpace,
    ) -> std::io::Result<()> {
        for (name, meta) in children(&upper.join(rel))? {
            let child = join(rel, &name);
            if is_whiteout(&meta) {
                self.marker(&join(rel, &format!(".wh.{name}")))?;
                continue;
            }
            if !meta.is_dir() {
                self.entry(&merged.join(&child), &child, Some(&meta))?;
                continue;
            }
            let at = upper.join(&child);
            let opaque = get_xattr(&at, &space.name("opaque")).is_some_and(|v| v == b"y");
            let renamed = get_xattr(&at, &space.name("redirect")).is_some();
            self.entry(&merged.join(&child), &child, None)?;
            if renamed {
                // A renamed image directory: all of it, in place of what was there.
                self.marker(&format!("{child}/.wh..wh..opq"))?;
                self.merged_dir(merged, &child)?;
            } else {
                if opaque {
                    self.marker(&format!("{child}/.wh..wh..opq"))?;
                }
                self.upper_dir(upper, merged, &child, space)?;
            }
        }
        Ok(())
    }

    /// Everything below `rel` in the merged view.
    fn merged_dir(&mut self, merged: &Path, rel: &str) -> std::io::Result<()> {
        self.tree(merged, rel, &[])
    }

    /// Everything below `rel` in `root`; the directories in `skip` are written empty.
    fn tree(&mut self, root: &Path, rel: &str, skip: &[String]) -> std::io::Result<()> {
        for (name, meta) in children(&root.join(rel))? {
            let child = join(rel, &name);
            self.entry(&root.join(&child), &child, None)?;
            if meta.is_dir() && !skip.contains(&child) {
                self.tree(root, &child, skip)?;
            }
        }
        Ok(())
    }

    fn marker(&mut self, name: &str) -> std::io::Result<()> {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o600);
        h.set_uid(0);
        h.set_gid(0);
        h.set_size(0);
        h.set_mtime(0);
        self.tar.append_data(&mut h, name, std::io::empty())
    }

    /// One entry from `src` (in the merged view). `upper` is its upper-directory metadata,
    /// whose inode tells hard links apart. A path gone meanwhile is left out.
    fn entry(
        &mut self,
        src: &Path,
        name: &str,
        upper: Option<&std::fs::Metadata>,
    ) -> std::io::Result<()> {
        let meta = match std::fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let kind = meta.file_type();
        if kind.is_socket() {
            return Ok(());
        }
        let mut h = tar::Header::new_gnu();
        h.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
        h.set_size(0);
        let xattrs = kept_xattrs(src);
        if !xattrs.is_empty() {
            let pax: Vec<(String, Vec<u8>)> = xattrs
                .into_iter()
                .map(|(k, v)| (format!("SCHILY.xattr.{k}"), v))
                .collect();
            self.tar
                .append_pax_extensions(pax.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
        }
        if kind.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            return self
                .tar
                .append_data(&mut h, format!("{name}/"), std::io::empty());
        }
        if kind.is_symlink() {
            h.set_entry_type(tar::EntryType::Symlink);
            let target = std::fs::read_link(src)?;
            return self.tar.append_link(&mut h, name, target);
        }
        if kind.is_file() {
            let id = upper.map_or((meta.dev(), meta.ino()), |u| (u.dev(), u.ino()));
            let nlink = upper.map_or(meta.nlink(), |u| u.nlink());
            if nlink > 1 {
                if let Some(first) = self.links.get(&id) {
                    h.set_entry_type(tar::EntryType::Link);
                    let first = first.clone();
                    return self.tar.append_link(&mut h, name, first);
                }
                self.links.insert(id, name.to_string());
            }
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(meta.len());
            let file = std::fs::File::open(src)?;
            return self
                .tar
                .append_data(&mut h, name, Exact(file.take(meta.len()), meta.len()));
        }
        // A FIFO or a device: the header says all.
        if kind.is_fifo() {
            h.set_entry_type(tar::EntryType::Fifo);
        } else {
            h.set_entry_type(if kind.is_char_device() {
                tar::EntryType::Char
            } else {
                tar::EntryType::Block
            });
            let rdev = meta.rdev();
            h.set_device_major(libc::major(rdev))?;
            h.set_device_minor(libc::minor(rdev))?;
        }
        self.tar.append_data(&mut h, name, std::io::empty())
    }
}

/// Exactly `.1` bytes: zeros when the file shrank meanwhile.
struct Exact<R>(R, u64);

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let want = buf.len().min(usize::try_from(self.1).unwrap_or(usize::MAX));
        if want == 0 {
            return Ok(0);
        }
        let mut n = self.0.read(&mut buf[..want])?;
        if n == 0 {
            buf[..want].fill(0);
            n = want;
        }
        self.1 -= n as u64;
        Ok(n)
    }
}

/// The entries of a directory, sorted by name. A directory gone meanwhile is empty.
fn children(dir: &Path) -> std::io::Result<Vec<(String, std::fs::Metadata)>> {
    let list = match std::fs::read_dir(dir) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in list {
        let entry = entry?;
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        out.push((entry.file_name().to_string_lossy().into_owned(), meta));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

/// The extended attributes a layer keeps: file capabilities and `user.*`, but not the
/// overlay's own.
fn kept_xattrs(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for name in list_xattrs(path) {
        let keep = name == "security.capability"
            || (name.starts_with("user.") && !name.starts_with("user.overlay."));
        if keep && let Some(v) = get_xattr(path, &name) {
            out.push((name, v));
        }
    }
    out.sort();
    out
}

fn c_path(path: &Path) -> Option<CString> {
    CString::new(path.as_os_str().as_bytes()).ok()
}

fn list_xattrs(path: &Path) -> Vec<String> {
    let Some(p) = c_path(path) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: p is a valid C string; buf is writable for its length.
        let n = unsafe { libc::llistxattr(p.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            buf.truncate(n as usize);
            break;
        }
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE)
            && buf.len() < 1 << 20
        {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return Vec::new();
    }
    buf.split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(|n| OsStr::from_bytes(n).to_string_lossy().into_owned())
        .collect()
}

pub(crate) fn get_xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    let p = c_path(path)?;
    let n = CString::new(name).ok()?;
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: valid C strings; buf is writable for its length.
        let got =
            unsafe { libc::lgetxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if got >= 0 {
            buf.truncate(got as usize);
            return Some(buf);
        }
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE)
            && buf.len() < 1 << 20
        {
            buf.resize(buf.len() * 8, 0);
            continue;
        }
        return None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entries(tar: &[u8]) -> Vec<(String, tar::EntryType, String)> {
        let mut a = tar::Archive::new(tar);
        a.entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let path = e.path().unwrap().display().to_string();
                let path = path.trim_end_matches('/').to_string();
                let kind = e.header().entry_type();
                let link = e
                    .link_name()
                    .unwrap()
                    .map(|l| l.display().to_string())
                    .unwrap_or_default();
                let mut body = String::new();
                if kind == tar::EntryType::Regular {
                    e.read_to_string(&mut body).unwrap();
                }
                (path, kind, if link.is_empty() { body } else { link })
            })
            .collect()
    }

    /// An upper directory and a merged view that shows the same files (no image below).
    fn layer_of(upper: &Path) -> Vec<(String, tar::EntryType, String)> {
        let mut out = Vec::new();
        write_layer(upper, upper, XattrSpace::User, &mut out).unwrap();
        entries(&out)
    }

    #[test]
    fn files_dirs_links_and_hard_links() {
        let tmp = tempfile::tempdir().unwrap();
        let up = tmp.path();
        std::fs::create_dir_all(up.join("etc/app")).unwrap();
        std::fs::write(up.join("etc/app/conf"), "x=1\n").unwrap();
        std::fs::hard_link(up.join("etc/app/conf"), up.join("etc/app/same")).unwrap();
        std::os::unix::fs::symlink("conf", up.join("etc/app/link")).unwrap();
        std::fs::set_permissions(
            up.join("etc/app/conf"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        use tar::EntryType as T;
        assert_eq!(
            layer_of(up),
            [
                ("etc".into(), T::Directory, String::new()),
                ("etc/app".into(), T::Directory, String::new()),
                ("etc/app/conf".into(), T::Regular, "x=1\n".into()),
                ("etc/app/link".into(), T::Symlink, "conf".into()),
                ("etc/app/same".into(), T::Link, "etc/app/conf".into()),
            ]
        );
    }

    #[test]
    fn contents_come_from_the_merged_view() {
        let tmp = tempfile::tempdir().unwrap();
        let (up, merged) = (tmp.path().join("up"), tmp.path().join("merged"));
        std::fs::create_dir_all(&up).unwrap();
        std::fs::create_dir_all(&merged).unwrap();
        // A metacopy file is empty in the upper directory.
        std::fs::write(up.join("big"), "").unwrap();
        std::fs::write(merged.join("big"), "all the data").unwrap();
        std::fs::write(merged.join("untouched"), "image file").unwrap();
        let mut out = Vec::new();
        write_layer(&up, &merged, XattrSpace::User, &mut out).unwrap();
        assert_eq!(
            entries(&out),
            [(
                "big".to_string(),
                tar::EntryType::Regular,
                "all the data".to_string()
            )]
        );
    }

    #[test]
    fn opaque_directories_and_kept_xattrs() {
        let tmp = tempfile::tempdir().unwrap();
        let up = tmp.path();
        std::fs::create_dir(up.join("cache")).unwrap();
        std::fs::write(up.join("cache/new"), "n").unwrap();
        std::fs::write(up.join("tagged"), "t").unwrap();
        let set = |p: &Path, k: &str, v: &[u8]| {
            let (p, k) = (c_path(p).unwrap(), CString::new(k).unwrap());
            // SAFETY: valid C strings and buffer.
            unsafe { libc::lsetxattr(p.as_ptr(), k.as_ptr(), v.as_ptr().cast(), v.len(), 0) == 0 }
        };
        if !set(&up.join("cache"), "user.overlay.opaque", b"y") {
            eprintln!("user xattrs are not supported here: skipped");
            return;
        }
        assert!(set(&up.join("tagged"), "user.note", b"hello"));
        assert!(set(&up.join("tagged"), "user.overlay.origin", b"x"));
        let mut out = Vec::new();
        write_layer(up, up, XattrSpace::User, &mut out).unwrap();
        let names: Vec<String> = entries(&out).into_iter().map(|e| e.0).collect();
        assert_eq!(
            names,
            ["cache", "cache/.wh..wh..opq", "cache/new", "tagged"]
        );
        let mut a = tar::Archive::new(&out[..]);
        let mut tagged = a
            .entries()
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.path().unwrap().ends_with("tagged"))
            .unwrap();
        let keys: Vec<String> = tagged
            .pax_extensions()
            .unwrap()
            .unwrap()
            .map(|e| e.unwrap().key().unwrap().to_string())
            .collect();
        assert_eq!(keys, ["SCHILY.xattr.user.note"]);
    }

    #[test]
    #[ignore = "needs root to make a whiteout: dev/vm.sh test-root"]
    fn whiteouts_become_markers_as_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (up, lower) = (tmp.path().join("up"), tmp.path().join("lower"));
        std::fs::create_dir_all(up.join("etc")).unwrap();
        std::fs::create_dir_all(lower.join("etc")).unwrap();
        std::fs::write(lower.join("etc/gone"), "").unwrap();
        let wh = c_path(&up.join("etc/gone")).unwrap();
        // SAFETY: a valid path; a 0/0 character device is overlayfs's whiteout.
        assert_eq!(unsafe { libc::mknod(wh.as_ptr(), libc::S_IFCHR, 0) }, 0);
        std::fs::write(up.join("etc/new"), "n").unwrap();
        let names: Vec<String> = layer_of(&up).into_iter().map(|e| e.0).collect();
        assert_eq!(names, ["etc", "etc/.wh.gone", "etc/new"]);
        assert_eq!(
            changes(&up, &lower).unwrap(),
            [
                Change {
                    path: "/etc".into(),
                    kind: 0
                },
                Change {
                    path: "/etc/gone".into(),
                    kind: 2
                },
                Change {
                    path: "/etc/new".into(),
                    kind: 1
                },
            ]
        );
    }

    #[test]
    fn changes_tell_added_from_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let (up, lower) = (tmp.path().join("up"), tmp.path().join("lower"));
        std::fs::create_dir_all(up.join("etc")).unwrap();
        std::fs::create_dir_all(lower.join("etc")).unwrap();
        std::fs::write(lower.join("etc/hosts"), "").unwrap();
        std::fs::write(up.join("etc/hosts"), "new").unwrap();
        std::fs::create_dir_all(up.join("srv/app")).unwrap();
        let got: Vec<(String, u8)> = changes(&up, &lower)
            .unwrap()
            .into_iter()
            .map(|c| (c.path, c.kind))
            .collect();
        assert_eq!(
            got,
            [
                ("/etc".to_string(), 0),
                ("/etc/hosts".to_string(), 0),
                ("/srv".to_string(), 1),
                ("/srv/app".to_string(), 1),
            ]
        );
    }
}
