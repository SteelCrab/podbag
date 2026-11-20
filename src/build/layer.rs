//! A container's changes, read from its overlay upper directory: as a layer tar (for
//! builds and `commit`), or as a list (`diff`).
//!
//! The upper directory tells what changed: a character device 0/0 is a deleted path
//! (written as a `.wh.<name>` entry), a directory with the overlay's `opaque` attribute
//! hides what the image had there (`.wh..wh..opq`), and a directory with a `redirect`
//! attribute was renamed from an image directory. File contents and metadata come from the
//! merged view: with composefs, an upper file may hold only metadata (metacopy).
//!
//! The container may change its files while they are read, and swap a file or a folder
//! for a link: no path is resolved from the host. Each tree is opened once, and every path
//! below it is opened with `openat2` inside that tree (`RESOLVE_IN_ROOT`), refusing links
//! on the way (`RESOLVE_NO_SYMLINKS`). Files are opened without following a link, and
//! their type and size come from the opened file.

pub use super::Change;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags, Stat};
use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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

/// A file tree that paths cannot leave: every path is resolved inside it.
struct Root(OwnedFd);

impl Root {
    /// Open the tree at `path`, a path the caller trusts (not one inside a container).
    fn open(path: &Path) -> std::io::Result<Root> {
        let fd = rustix::fs::open(
            path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Root(fd))
    }

    /// Open `rel` inside the tree, through no link. A link as the last part is opened
    /// itself with `O_PATH | O_NOFOLLOW`; other flags refuse it.
    fn at(&self, rel: &str, flags: OFlags) -> std::io::Result<OwnedFd> {
        let path = if rel.is_empty() { "." } else { rel };
        let resolve =
            ResolveFlags::IN_ROOT | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        let flags = flags | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut tries = 0;
        loop {
            match rustix::fs::openat2(&self.0, path, flags, Mode::empty(), resolve) {
                // A rename elsewhere in the tree: the kernel asks to try again.
                Err(rustix::io::Errno::AGAIN) if tries < 8 => tries += 1,
                other => return other.map_err(Into::into),
            }
        }
    }

    /// The details of `rel` itself (a link is not followed). None when it is gone, or
    /// cannot be reached without a link.
    fn stat(&self, rel: &str) -> Option<Stat> {
        let fd = self.at(rel, OFlags::PATH).ok()?;
        rustix::fs::fstat(&fd).ok()
    }

    /// The entries of the directory `rel`, sorted by name, with their details. A directory
    /// gone meanwhile, or one that became a link, is empty.
    fn children(&self, rel: &str) -> std::io::Result<Vec<(String, Stat)>> {
        let dir = match self.at(rel, OFlags::RDONLY | OFlags::DIRECTORY) {
            Ok(fd) => fd,
            Err(e) if gone(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for entry in rustix::fs::Dir::read_from(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            // One name, relative to the directory itself: nothing to resolve.
            match rustix::fs::statat(&dir, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => out.push((OsStr::from_bytes(name).to_string_lossy().into_owned(), st)),
                Err(rustix::io::Errno::NOENT) => {}
                Err(e) => return Err(e.into()),
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// The target of the link `rel`.
    fn read_link(&self, rel: &str) -> std::io::Result<PathBuf> {
        let fd = self.at(rel, OFlags::PATH)?;
        let target = rustix::fs::readlinkat(&fd, "", Vec::new())?;
        Ok(PathBuf::from(OsStr::from_bytes(target.as_bytes())))
    }

    /// The regular file `rel`, opened for reading, with its details. None when it is gone
    /// or is no longer a regular file.
    fn open_file(&self, rel: &str) -> std::io::Result<Option<(std::fs::File, Stat)>> {
        let fd = match self.at(rel, OFlags::PATH) {
            Ok(fd) => fd,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let st = rustix::fs::fstat(&fd)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Ok(None);
        }
        // The same inode, opened for reading: no path is looked up again.
        let file = std::fs::File::open(proc_path(&fd))?;
        let st = rustix::fs::fstat(&file)?;
        Ok(Some((file, st)))
    }

    /// The value of the extended attribute `name` of `rel` (not of a link's target).
    fn xattr(&self, rel: &str, name: &str) -> Option<Vec<u8>> {
        let fd = self.at(rel, OFlags::PATH).ok()?;
        let st = rustix::fs::fstat(&fd).ok()?;
        if FileType::from_raw_mode(st.st_mode) == FileType::Symlink {
            return None;
        }
        get_xattr_at(&proc_path(&fd), name)
    }

    /// The extended attributes a layer keeps: file capabilities and `user.*`, but not the
    /// overlay's own. A link has none.
    fn kept_xattrs(&self, rel: &str, st: &Stat) -> Vec<(String, Vec<u8>)> {
        if FileType::from_raw_mode(st.st_mode) == FileType::Symlink {
            return Vec::new();
        }
        let Ok(fd) = self.at(rel, OFlags::PATH) else {
            return Vec::new();
        };
        let path = proc_path(&fd);
        let mut out = Vec::new();
        for name in list_xattrs_at(&path) {
            let keep = name == "security.capability"
                || (name.starts_with("user.") && !name.starts_with("user.overlay."));
            if keep && let Some(v) = get_xattr_at(&path, &name) {
                out.push((name, v));
            }
        }
        out.sort();
        out
    }
}

/// `/proc/self/fd/<n>`: the file of a descriptor, for calls that take a path.
fn proc_path(fd: &impl AsFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
}

/// Gone, or reachable only through a link: left out.
fn gone(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOENT | libc::ELOOP | libc::ENOTDIR | libc::EXDEV)
    )
}

/// Write the changes in `upper` as a layer tar to `out`, reading what files hold from
/// `merged`.
pub fn write_layer(
    upper: &Path,
    merged: &Path,
    space: XattrSpace,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let (upper, merged) = (Root::open(upper)?, Root::open(merged)?);
    let mut w = Writer {
        tar: tar::Builder::new(out),
        links: HashMap::new(),
    };
    w.upper_dir(&upper, &merged, "", space)?;
    w.tar.finish()
}

/// All of a container's files under `root` as a tar (`export`). The directories in `skip`
/// (relative paths: the mounts of a running container) are written empty.
pub fn write_tree(root: &Path, skip: &[String], out: &mut dyn Write) -> std::io::Result<()> {
    let root = Root::open(root)?;
    let mut w = Writer {
        tar: tar::Builder::new(out),
        links: HashMap::new(),
    };
    w.tree(&root, "", skip)?;
    w.tar.finish()
}

/// The changes in `upper`: added when `lower` (the image) lacks the path.
pub fn changes(upper: &Path, lower: &Path) -> std::io::Result<Vec<Change>> {
    let (upper, lower) = (Root::open(upper)?, Root::open(lower)?);
    let mut out = Vec::new();
    list_changes(&upper, &lower, "", &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn list_changes(
    upper: &Root,
    lower: &Root,
    rel: &str,
    out: &mut Vec<Change>,
) -> std::io::Result<()> {
    for (name, st) in upper.children(rel)? {
        let child = join(rel, &name);
        let path = format!("/{child}");
        if is_whiteout(&st) {
            out.push(Change { path, kind: 2 });
            continue;
        }
        let in_image = lower.stat(&child).is_some();
        out.push(Change {
            path,
            kind: if in_image { 0 } else { 1 },
        });
        if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
            list_changes(upper, lower, &child, out)?;
        }
    }
    Ok(())
}

/// A deleted path in an overlay upper directory.
fn is_whiteout(st: &Stat) -> bool {
    FileType::from_raw_mode(st.st_mode) == FileType::CharacterDevice && st.st_rdev == 0
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
        upper: &Root,
        merged: &Root,
        rel: &str,
        space: XattrSpace,
    ) -> std::io::Result<()> {
        for (name, st) in upper.children(rel)? {
            let child = join(rel, &name);
            if is_whiteout(&st) {
                self.marker(&join(rel, &format!(".wh.{name}")))?;
                continue;
            }
            if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
                self.entry(merged, &child, Some(&st))?;
                continue;
            }
            let opaque = upper
                .xattr(&child, &space.name("opaque"))
                .is_some_and(|v| v == b"y");
            let renamed = upper.xattr(&child, &space.name("redirect")).is_some();
            self.entry(merged, &child, None)?;
            if renamed {
                // A renamed image directory: all of it, in place of what was there.
                self.marker(&format!("{child}/.wh..wh..opq"))?;
                self.tree(merged, &child, &[])?;
            } else {
                if opaque {
                    self.marker(&format!("{child}/.wh..wh..opq"))?;
                }
                self.upper_dir(upper, merged, &child, space)?;
            }
        }
        Ok(())
    }

    /// Everything below `rel` in `root`; the directories in `skip` are written empty.
    fn tree(&mut self, root: &Root, rel: &str, skip: &[String]) -> std::io::Result<()> {
        for (name, st) in root.children(rel)? {
            let child = join(rel, &name);
            self.entry(root, &child, None)?;
            if FileType::from_raw_mode(st.st_mode) == FileType::Directory && !skip.contains(&child)
            {
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

    /// One entry from `rel` in `root` (the merged view). `upper` is its upper-directory
    /// details, whose inode tells hard links apart. A path gone meanwhile, or one that
    /// became a link on the way, is left out.
    fn entry(&mut self, root: &Root, rel: &str, upper: Option<&Stat>) -> std::io::Result<()> {
        let Some(st) = root.stat(rel) else {
            return Ok(());
        };
        let kind = FileType::from_raw_mode(st.st_mode);
        // A regular file is opened first: its details come from the open file.
        let (file, st) = if kind == FileType::RegularFile {
            match root.open_file(rel)? {
                Some((file, st)) => (Some(file), st),
                None => return Ok(()),
            }
        } else {
            (None, st)
        };
        if kind == FileType::Socket {
            return Ok(());
        }
        let mut h = tar::Header::new_gnu();
        h.set_mode(st.st_mode & 0o7777);
        h.set_uid(u64::from(st.st_uid));
        h.set_gid(u64::from(st.st_gid));
        h.set_mtime(st.st_mtime.max(0) as u64);
        h.set_size(0);
        let xattrs = root.kept_xattrs(rel, &st);
        if !xattrs.is_empty() {
            let pax: Vec<(String, Vec<u8>)> = xattrs
                .into_iter()
                .map(|(k, v)| (format!("SCHILY.xattr.{k}"), v))
                .collect();
            self.tar
                .append_pax_extensions(pax.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
        }
        match kind {
            FileType::Directory => {
                h.set_entry_type(tar::EntryType::Directory);
                self.tar
                    .append_data(&mut h, format!("{rel}/"), std::io::empty())
            }
            FileType::Symlink => {
                h.set_entry_type(tar::EntryType::Symlink);
                let target = match root.read_link(rel) {
                    Ok(t) => t,
                    Err(e) if gone(&e) => return Ok(()),
                    Err(e) => return Err(e),
                };
                self.tar.append_link(&mut h, rel, target)
            }
            FileType::RegularFile => {
                let id = upper.map_or((st.st_dev, st.st_ino), |u| (u.st_dev, u.st_ino));
                let nlink = upper.map_or(st.st_nlink, |u| u.st_nlink);
                if nlink > 1 {
                    if let Some(first) = self.links.get(&id) {
                        h.set_entry_type(tar::EntryType::Link);
                        let first = first.clone();
                        return self.tar.append_link(&mut h, rel, first);
                    }
                    self.links.insert(id, rel.to_string());
                }
                let size = st.st_size.max(0) as u64;
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(size);
                let file = file.expect("a regular file was opened");
                self.tar
                    .append_data(&mut h, rel, Exact(file.take(size), size))
            }
            // A FIFO or a device: the header says all.
            _ => {
                if kind == FileType::Fifo {
                    h.set_entry_type(tar::EntryType::Fifo);
                } else {
                    h.set_entry_type(if kind == FileType::CharacterDevice {
                        tar::EntryType::Char
                    } else {
                        tar::EntryType::Block
                    });
                    h.set_device_major(libc::major(st.st_rdev))?;
                    h.set_device_minor(libc::minor(st.st_rdev))?;
                }
                self.tar.append_data(&mut h, rel, std::io::empty())
            }
        }
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

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

fn c_path(path: &Path) -> Option<CString> {
    CString::new(path.as_os_str().as_bytes()).ok()
}

/// The attribute names of the file at `path` (a `/proc/self/fd/<n>` path).
fn list_xattrs_at(path: &Path) -> Vec<String> {
    let Some(p) = c_path(path) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: p is a valid C string; buf is writable for its length.
        let n = unsafe { libc::listxattr(p.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
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

/// One attribute of the file at `path` (a `/proc/self/fd/<n>` path).
fn get_xattr_at(path: &Path, name: &str) -> Option<Vec<u8>> {
    let p = c_path(path)?;
    let n = CString::new(name).ok()?;
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: valid C strings; buf is writable for its length.
        let got =
            unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
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

    /// What a container leaves when it swaps directories and files for links while the
    /// walk runs: the upper directory lists `d/secret`, `r/secret` and `f` as files, and
    /// the merged view has links there that lead out of the container's root.
    #[test]
    fn links_never_lead_out_of_the_container() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "HOST SECRET").unwrap();
        std::fs::write(outside.join("host-only-name"), "").unwrap();
        let (up, merged) = (tmp.path().join("up"), tmp.path().join("merged"));
        for d in ["d", "r"] {
            std::fs::create_dir_all(up.join(d)).unwrap();
            std::fs::write(up.join(d).join("secret"), "upper").unwrap();
        }
        std::fs::write(up.join("f"), "upper").unwrap();
        std::fs::create_dir_all(&merged).unwrap();
        std::os::unix::fs::symlink(&outside, merged.join("d")).unwrap();
        std::os::unix::fs::symlink("../outside", merged.join("r")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), merged.join("f")).unwrap();
        let leaks = |tar: &[u8]| {
            let text = String::from_utf8_lossy(tar);
            text.contains("HOST SECRET") || text.contains("host-only-name")
        };

        let mut out = Vec::new();
        write_layer(&up, &merged, XattrSpace::User, &mut out).unwrap();
        assert!(!leaks(&out), "write_layer read a file outside the root");
        let mut out = Vec::new();
        write_tree(&merged, &[], &mut out).unwrap();
        assert!(!leaks(&out), "write_tree read a file outside the root");
        // Paths that became links after the walk listed them.
        let root = Root::open(&merged).unwrap();
        let mut out = Vec::new();
        {
            let mut w = Writer {
                tar: tar::Builder::new(&mut out as &mut dyn Write),
                links: HashMap::new(),
            };
            w.tree(&root, "d", &[]).unwrap();
            w.tree(&root, "r", &[]).unwrap();
            for rel in ["d/secret", "r/secret", "f", "d/host-only-name"] {
                w.entry(&root, rel, None).unwrap();
            }
            w.tar.finish().unwrap();
        }
        assert!(!leaks(&out), "a walk below a link read outside the root");
        // The upper directory itself, with a link where a directory was.
        std::fs::remove_dir_all(up.join("r")).unwrap();
        std::os::unix::fs::symlink(&outside, up.join("r")).unwrap();
        std::fs::create_dir_all(up.join("x")).unwrap();
        let mut out = Vec::new();
        write_layer(&up, &up, XattrSpace::User, &mut out).unwrap();
        assert!(
            !leaks(&out),
            "write_layer listed a directory outside the upper one"
        );
        // The image (here the merged view) has links at d and r: what is below them in
        // the upper directory is new, whatever the host has there.
        std::fs::remove_file(up.join("r")).unwrap();
        std::fs::create_dir_all(up.join("r")).unwrap();
        std::fs::write(up.join("r/secret"), "upper").unwrap();
        let got: Vec<(String, u8)> = changes(&up, &merged)
            .unwrap()
            .into_iter()
            .map(|c| (c.path, c.kind))
            .collect();
        assert!(got.contains(&("/d/secret".to_string(), 1)), "{got:?}");
        assert!(got.contains(&("/r/secret".to_string(), 1)), "{got:?}");
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
