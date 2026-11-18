//! Files in and out of a container as tar archives (`docker cp`): the details of a path,
//! a tar of it, and a tar unpacked into a directory.
//!
//! Paths are the container's: `root` is its root directory. The helper that calls these
//! has already made the container's root its own (`linux::archive`), so there `root` is
//! `/` and no symbolic link can lead out of the container.

use serde::{Deserialize, Serialize};

/// A path's details, as Docker's `X-Docker-Container-Path-Stat` header gives them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathStat {
    pub name: String,
    pub size: i64,
    /// Go's `os.FileMode`: the permission bits, and the file type in the high bits.
    pub mode: u32,
    pub mtime: String,
    /// For a symbolic link, the path it leads to in the container.
    #[serde(rename = "linkTarget")]
    pub link_target: String,
}

/// How a tar is unpacked into a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutOptions {
    /// Refuse to replace a directory with a file, or a file with a directory.
    pub no_overwrite_dir_non_dir: bool,
    /// Keep the owners the archive names; otherwise the container's root owns everything.
    pub copy_uid_gid: bool,
}

/// Go's `os.FileMode` bits for a Unix `st_mode`.
pub fn go_mode(st_mode: u32) -> u32 {
    const DIR: u32 = 1 << 31;
    const SYMLINK: u32 = 1 << 27;
    const DEVICE: u32 = 1 << 26;
    const NAMED_PIPE: u32 = 1 << 25;
    const SOCKET: u32 = 1 << 24;
    const SETUID: u32 = 1 << 23;
    const SETGID: u32 = 1 << 22;
    const CHAR_DEVICE: u32 = 1 << 21;
    const STICKY: u32 = 1 << 20;
    const IRREGULAR: u32 = 1 << 19;
    let mut mode = st_mode & 0o777;
    mode |= match st_mode & 0o170000 {
        0o040000 => DIR,
        0o120000 => SYMLINK,
        0o060000 => DEVICE,
        0o020000 => DEVICE | CHAR_DEVICE,
        0o010000 => NAMED_PIPE,
        0o140000 => SOCKET,
        0o100000 => 0,
        _ => IRREGULAR,
    };
    if st_mode & 0o4000 != 0 {
        mode |= SETUID;
    }
    if st_mode & 0o2000 != 0 {
        mode |= SETGID;
    }
    if st_mode & 0o1000 != 0 {
        mode |= STICKY;
    }
    mode
}

/// The last part of a container path, as Docker names it: `/etc/` is `etc`, `/etc/.` is
/// `.`, and `/` is `/`.
pub fn base_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        _ if trimmed.is_empty() => "/".to_string(),
        Some(last) => last.to_string(),
        None => trimmed.to_string(),
    }
}

#[cfg(unix)]
pub use unix::{extract, stat, write_tar};

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::{Error, ErrorKind, Result};
    use std::ffi::OsString;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::path::{Path, PathBuf};

    /// How many symbolic links one path may pass through, like the kernel's limit.
    const MAX_LINKS: u32 = 40;

    /// The details of `path` in the container whose root is `root`. A symbolic link as
    /// the last part is reported as a link, unless the path ends with `/` or `/.`.
    pub fn stat(root: &Path, path: &str) -> Result<PathStat> {
        let full = locate(root, path).map_err(|e| missing(path, e))?;
        let meta = std::fs::symlink_metadata(&full).map_err(|e| missing(path, e))?;
        let link_target = if meta.file_type().is_symlink() {
            real_path(root, &absolute(path))
                .ok()
                .and_then(|p| {
                    p.strip_prefix(root)
                        .ok()
                        .map(|rel| format!("/{}", rel.display()))
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok(PathStat {
            name: base_name(path),
            size: meta.len() as i64,
            mode: go_mode(meta.mode()),
            mtime: crate::shim::logbook::rfc3339(
                meta.mtime().max(0) as u64,
                meta.mtime_nsec().clamp(0, 999_999_999) as u32,
            ),
            link_target,
        })
    }

    /// Write `path` as a tar whose names start with its last part (`etc/passwd` for
    /// `/etc`; `./passwd` for `/etc/.`). Sockets are left out, as Docker does.
    pub fn write_tar(root: &Path, path: &str, out: &mut dyn Write) -> Result<()> {
        let full = locate(root, path).map_err(|e| missing(path, e))?;
        let meta = std::fs::symlink_metadata(&full).map_err(|e| missing(path, e))?;
        let name = match base_name(path).as_str() {
            "/" => ".".to_string(),
            other => other.to_string(),
        };
        let mut builder = tar::Builder::new(out);
        let written = append(&mut builder, &full, name.as_bytes(), &meta);
        let written = match written {
            Ok(()) => builder.finish(),
            Err(e) => {
                // Without the end blocks a dropped builder would write: a reader must see
                // the archive cut short, not a smaller one that looks complete.
                std::mem::forget(builder);
                Err(e)
            }
        };
        written.map_err(|e| {
            Error::io(
                format!("cannot read {path} in the container"),
                &e,
                "check that the path can be read",
            )
        })
    }

    fn append(
        builder: &mut tar::Builder<&mut dyn Write>,
        src: &Path,
        name: &[u8],
        meta: &std::fs::Metadata,
    ) -> std::io::Result<()> {
        let kind = meta.file_type();
        if kind.is_socket() {
            return Ok(());
        }
        let mut header = tar::Header::new_gnu();
        header.set_metadata_in_mode(meta, tar::HeaderMode::Complete);
        if kind.is_dir() {
            header.set_size(0);
            // Directories end with `/`, as Docker writes them.
            let mut dir_name = name.to_vec();
            if !dir_name.ends_with(b"/") {
                dir_name.push(b'/');
            }
            append_named(builder, &mut header, &dir_name, None, std::io::empty())?;
            // The kernel's file systems (/proc, /sys, …) come out empty, as with Docker,
            // which reads a container's files from outside it.
            if is_virtual(src) {
                return Ok(());
            }
            let mut children = match std::fs::read_dir(src) {
                Ok(list) => list.filter_map(|c| c.ok()).collect::<Vec<_>>(),
                // Gone meanwhile (a running container changes its files): left out.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(e),
            };
            children.sort_by_key(|c| c.file_name());
            for child in children {
                let child_name = [&dir_name[..], child.file_name().as_bytes()].concat();
                // DirEntry::metadata does not follow a symbolic link.
                let child_meta = match child.metadata() {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                };
                append(builder, &child.path(), &child_name, &child_meta)?;
            }
            Ok(())
        } else if kind.is_symlink() {
            header.set_size(0);
            let target = std::fs::read_link(src)?;
            let target = target.as_os_str().as_bytes();
            append_named(builder, &mut header, name, Some(target), std::io::empty())
        } else if kind.is_file() {
            use std::os::unix::fs::OpenOptionsExt;
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(src)
            {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            // Exactly the size in the header, even if the file changes meanwhile.
            let exact = Exact {
                inner: file,
                left: meta.len(),
            };
            append_named(builder, &mut header, name, None, exact)
        } else {
            // A FIFO or a device: the header says all.
            header.set_size(0);
            append_named(builder, &mut header, name, None, std::io::empty())
        }
    }

    /// Append an entry named exactly `name`: tar-rs would drop a leading `./`, which
    /// Docker's clients need. A name or link target too long for the header goes first in
    /// a GNU record of its own.
    fn append_named(
        builder: &mut tar::Builder<&mut dyn Write>,
        header: &mut tar::Header,
        name: &[u8],
        link: Option<&[u8]>,
        data: impl Read,
    ) -> std::io::Result<()> {
        if let Some(link) = link {
            if link.len() > header.as_old_mut().linkname.len() {
                long_record(builder, b'K', link)?;
            }
            put_field(&mut header.as_old_mut().linkname, link);
        }
        if name.len() > header.as_old_mut().name.len() {
            long_record(builder, b'L', name)?;
        }
        put_field(&mut header.as_old_mut().name, name);
        header.set_cksum();
        builder.append(header, data)
    }

    fn put_field(field: &mut [u8], value: &[u8]) {
        field.fill(0);
        let n = value.len().min(field.len());
        field[..n].copy_from_slice(&value[..n]);
    }

    /// A GNU long name (`L`) or long link target (`K`) record for the next entry.
    fn long_record(
        builder: &mut tar::Builder<&mut dyn Write>,
        kind: u8,
        value: &[u8],
    ) -> std::io::Result<()> {
        let mut header = tar::Header::new_gnu();
        put_field(&mut header.as_old_mut().name, b"././@LongLink");
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(value.len() as u64 + 1);
        header.set_entry_type(tar::EntryType::new(kind));
        header.set_cksum();
        builder.append(&header, value.chain(&[0u8][..]))
    }

    /// Whether `dir` is the root of one of the kernel's own file systems.
    fn is_virtual(dir: &Path) -> bool {
        #[cfg(target_os = "linux")]
        {
            // proc, sysfs, cgroup, cgroup2, devpts, mqueue, securityfs, debugfs, tracefs,
            // bpf, pstore, configfs, fusectl, selinuxfs, binfmt_misc, efivarfs, nsfs.
            const KINDS: [u32; 17] = [
                0x9fa0,
                0x6265_6572,
                0x0027_e0eb,
                0x6367_7270,
                0x1cd1,
                0x1980_0202,
                0x7363_6673,
                0x6462_6720,
                0x7472_6163,
                0xcafe_4a11,
                0x6165_676c,
                0x6265_6570,
                0x6573_5543,
                0xf97c_ff8c,
                0x4249_4e4d,
                0xde5e_81e4,
                0x6e73_6673,
            ];
            rustix::fs::statfs(dir).is_ok_and(|s| KINDS.contains(&(s.f_type as u32)))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = dir;
            false
        }
    }

    /// `left` bytes of `inner`, padded with zeros if it ends early.
    struct Exact<R> {
        inner: R,
        left: u64,
    }

    impl<R: Read> Read for Exact<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let want = buf
                .len()
                .min(usize::try_from(self.left).unwrap_or(usize::MAX));
            if want == 0 {
                return Ok(0);
            }
            let mut n = self.inner.read(&mut buf[..want])?;
            if n == 0 {
                buf[..want].fill(0);
                n = want;
            }
            self.left -= n as u64;
            Ok(n)
        }
    }

    /// Unpack the tar (gzip-compressed or not) from `input` into the directory `path`.
    pub fn extract(
        root: &Path,
        path: &str,
        input: &mut dyn Read,
        options: &PutOptions,
    ) -> Result<()> {
        let dest = real_path(root, &absolute(path)).map_err(|e| missing(path, e))?;
        let meta = std::fs::metadata(&dest).map_err(|e| missing(path, e))?;
        if !meta.is_dir() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("cannot copy to {path} in the container"),
                "extraction point is not a directory",
                "give a directory that exists in the container",
            ));
        }
        let mut input = BufReader::new(input);
        let gzip = input.fill_buf().map(|b| b.starts_with(&[0x1f, 0x8b]));
        let gzip = gzip.map_err(|e| bad_archive(&e.to_string()))?;
        let reader: Box<dyn Read + '_> = if gzip {
            Box::new(flate2::read::GzDecoder::new(input))
        } else {
            Box::new(input)
        };
        let mut archive = tar::Archive::new(reader);
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(true);
        archive.set_preserve_ownerships(options.copy_uid_gid);
        archive.set_unpack_xattrs(false);
        archive.set_overwrite(true);
        let entries = archive.entries().map_err(|e| bad_archive(&e.to_string()))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| bad_archive(&e.to_string()))?;
            let name = entry
                .path()
                .map_err(|e| bad_archive(&e.to_string()))?
                .into_owned();
            let Some(relative) = inside(&name) else {
                return Err(bad_archive(&format!(
                    "{} leads out of the destination",
                    name.display()
                )));
            };
            if relative.as_os_str().is_empty() {
                // The destination itself.
                continue;
            }
            make_room(&dest.join(&relative), entry.header().entry_type(), options)?;
            match entry.unpack_in(&dest) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(bad_archive(&format!(
                        "{} leads out of the destination",
                        name.display()
                    )));
                }
                Err(e) => {
                    return Err(Error::io(
                        format!("cannot copy {} into the container", name.display()),
                        &e,
                        "check the space left and the permissions at the destination",
                    ));
                }
            }
        }
        Ok(())
    }

    /// The entry's path below the destination: None when a `..` would leave it.
    fn inside(name: &Path) -> Option<PathBuf> {
        let mut out = PathBuf::new();
        for part in name.components() {
            match part {
                std::path::Component::Normal(p) => out.push(p),
                std::path::Component::ParentDir => return None,
                _ => {}
            }
        }
        Some(out)
    }

    /// What is at `target` gives way to the entry, as Docker's rules say.
    fn make_room(target: &Path, kind: tar::EntryType, options: &PutOptions) -> Result<()> {
        let Ok(existing) = std::fs::symlink_metadata(target) else {
            return Ok(());
        };
        let entry_is_dir = kind.is_dir();
        let existing_is_dir = existing.is_dir();
        if entry_is_dir == existing_is_dir {
            return Ok(());
        }
        if options.no_overwrite_dir_non_dir {
            let why = if existing_is_dir {
                format!(
                    "cannot overwrite directory {} with a non-directory",
                    target.display()
                )
            } else {
                format!(
                    "cannot overwrite non-directory {} with a directory",
                    target.display()
                )
            };
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cannot copy into the container",
                why,
                "remove what is in the way first, or copy to another path",
            ));
        }
        let removed = if existing_is_dir {
            std::fs::remove_dir_all(target)
        } else {
            std::fs::remove_file(target)
        };
        removed.map_err(|e| {
            Error::io(
                format!("cannot replace {} in the container", target.display()),
                &e,
                "check the permissions at the destination",
            )
        })
    }

    /// `path` from the container's root, as Docker reads it: a relative path starts there.
    fn absolute(path: &str) -> String {
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    }

    /// `path` with every directory before the last part resolved; the last part is
    /// followed too when the path ends with `/`, `/.` or `/..`.
    fn locate(root: &Path, path: &str) -> std::io::Result<PathBuf> {
        let abs = absolute(path);
        if abs.ends_with('/') || abs.ends_with("/.") || abs.ends_with("/..") {
            return real_path(root, &abs);
        }
        let (parent, last) = abs.rsplit_once('/').unwrap_or(("", &abs));
        Ok(real_path(root, parent)?.join(last))
    }

    /// Resolve `path` below `root` as the kernel would inside the container: symbolic
    /// links are followed, an absolute one starts again at `root`, and `..` never leaves it.
    fn real_path(root: &Path, path: &str) -> std::io::Result<PathBuf> {
        let parts = |p: &[u8]| -> Vec<OsString> {
            p.split(|b| *b == b'/')
                .filter(|s| !s.is_empty() && *s != b".")
                .map(|s| std::ffi::OsStr::from_bytes(s).to_os_string())
                .rev()
                .collect()
        };
        let mut todo = parts(path.as_bytes());
        let mut done: Vec<OsString> = Vec::new();
        let mut links = 0;
        while let Some(part) = todo.pop() {
            if part == ".." {
                done.pop();
                continue;
            }
            let mut next = root.to_path_buf();
            next.extend(&done);
            next.push(&part);
            let meta = std::fs::symlink_metadata(&next)?;
            if meta.file_type().is_symlink() {
                links += 1;
                if links > MAX_LINKS {
                    return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
                }
                let target = std::fs::read_link(&next)?;
                if target.is_absolute() {
                    done.clear();
                }
                todo.extend(parts(target.as_os_str().as_bytes()));
            } else {
                if !todo.is_empty() && !meta.is_dir() {
                    return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR));
                }
                done.push(part);
            }
        }
        let mut out = root.to_path_buf();
        out.extend(&done);
        Ok(out)
    }

    fn missing(path: &str, e: std::io::Error) -> Error {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::new(
                ErrorKind::NotFound,
                format!("could not find the file {path} in the container"),
                "no such file or directory",
                "check the path; it is read from the container's root",
            )
        } else {
            Error::io(
                format!("cannot look up {path} in the container"),
                &e,
                "check the path; it is read from the container's root",
            )
        }
    }

    fn bad_archive(why: &str) -> Error {
        Error::new(
            ErrorKind::InvalidInput,
            "cannot unpack the archive into the container",
            why.to_string(),
            "send a tar archive (it may be gzip-compressed) whose paths stay below the destination",
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::io::Read;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    fn names(tar: &[u8]) -> Vec<String> {
        let mut archive = tar::Archive::new(tar);
        archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect()
    }

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            // set_path refuses `..`; write the name bytes directly, as a hostile tar would.
            h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn go_file_modes_match_docker() {
        assert_eq!(go_mode(0o040755), (1 << 31) | 0o755);
        assert_eq!(go_mode(0o120777), (1 << 27) | 0o777);
        assert_eq!(go_mode(0o100644), 0o644);
        assert_eq!(go_mode(0o104755), (1 << 23) | 0o755);
        assert_eq!(go_mode(0o020666), (1 << 26) | (1 << 21) | 0o666);
    }

    #[test]
    fn base_names_follow_docker() {
        assert_eq!(base_name("/etc/"), "etc");
        assert_eq!(base_name("/etc/."), ".");
        assert_eq!(base_name("/"), "/");
        assert_eq!(base_name("etc/hosts"), "hosts");
    }

    #[test]
    fn stat_reports_a_link_as_a_link_unless_asked_to_follow() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        symlink("/real", root.path().join("link")).unwrap();
        let link = stat(root.path(), "/link").unwrap();
        assert_eq!(link.mode & (1 << 27), 1 << 27, "{link:?}");
        assert_eq!(link.link_target, "/real");
        let followed = stat(root.path(), "/link/").unwrap();
        assert_eq!(followed.mode & (1 << 31), 1 << 31, "{followed:?}");
        assert_eq!(followed.name, "link");
        let missing = stat(root.path(), "/nothing").unwrap_err();
        assert_eq!(missing.kind, ErrorKind::NotFound);
    }

    #[test]
    fn links_never_lead_out_of_the_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("etc")).unwrap();
        // Absolute, and through `..` past the root: both stay in it.
        symlink("/etc", root.path().join("abs")).unwrap();
        symlink("../../../../etc", root.path().join("up")).unwrap();
        let mut out = Vec::new();
        write_tar(root.path(), "/abs/", &mut out).unwrap();
        assert_eq!(names(&out), ["abs/"]);
        let into = stat(root.path(), "/up/").unwrap();
        assert_eq!(into.mode & (1 << 31), 1 << 31);
        extract(
            root.path(),
            "/up",
            &mut tar_of(&[("x", b"in")]).as_slice(),
            &PutOptions::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.path().join("etc/x")).unwrap(), b"in");
    }

    #[test]
    fn a_tar_of_a_directory_is_named_after_it() {
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join("d");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("f"), "data").unwrap();
        symlink("f", d.join("l")).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(d.join("sock")).unwrap();
        let mut out = Vec::new();
        write_tar(root.path(), "/d", &mut out).unwrap();
        assert_eq!(names(&out), ["d/", "d/f", "d/l", "d/sub/"]);
        let mut archive = tar::Archive::new(out.as_slice());
        let mut file = archive.entries().unwrap().nth(1).unwrap().unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "data");
        let mut contents = Vec::new();
        write_tar(root.path(), "/d/.", &mut contents).unwrap();
        assert_eq!(names(&contents), ["./", "./f", "./l", "./sub/"]);
        // A long name and a long link target survive.
        let long = "n".repeat(150);
        std::fs::write(d.join(&long), "x").unwrap();
        symlink(&long, d.join("to-long")).unwrap();
        let mut with_long = Vec::new();
        write_tar(root.path(), "/d", &mut with_long).unwrap();
        let mut archive = tar::Archive::new(with_long.as_slice());
        let found: Vec<(String, Option<String>)> = archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let link = e.link_name().unwrap().map(|l| l.display().to_string());
                (e.path().unwrap().display().to_string(), link)
            })
            .collect();
        assert!(found.contains(&(format!("d/{long}"), None)), "{found:?}");
        assert!(
            found.contains(&("d/to-long".to_string(), Some(long.clone()))),
            "{found:?}"
        );
    }

    #[test]
    fn extraction_refuses_paths_that_leave_the_destination() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dest")).unwrap();
        let tar = tar_of(&[("ok", b"1"), ("../escaped", b"2")]);
        let err = extract(
            root.path(),
            "/dest",
            &mut tar.as_slice(),
            &PutOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidInput, "{err}");
        assert!(!root.path().join("escaped").exists());
    }

    #[test]
    fn extraction_follows_docker_rules_for_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("dest/taken")).unwrap();
        let tar = tar_of(&[("taken", b"file")]);
        let strict = PutOptions {
            no_overwrite_dir_non_dir: true,
            copy_uid_gid: false,
        };
        let err = extract(root.path(), "/dest", &mut tar.as_slice(), &strict).unwrap_err();
        assert!(err.why.contains("cannot overwrite directory"), "{err}");
        assert!(root.path().join("dest/taken").is_dir());
        extract(
            root.path(),
            "/dest",
            &mut tar.as_slice(),
            &PutOptions::default(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("dest/taken")).unwrap(),
            b"file"
        );
    }

    #[test]
    fn extraction_reads_gzip_and_needs_a_directory() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dest")).unwrap();
        std::fs::write(root.path().join("file"), "").unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_of(&[("sub/z", b"zipped")])).unwrap();
        let gz = gz.finish().unwrap();
        extract(
            root.path(),
            "dest",
            &mut gz.as_slice(),
            &PutOptions::default(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("dest/sub/z")).unwrap(),
            b"zipped"
        );
        let mode = std::fs::metadata(root.path().join("dest/sub/z"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
        let tar = tar_of(&[("x", b"")]);
        let not_dir = extract(
            root.path(),
            "/file",
            &mut tar.as_slice(),
            &PutOptions::default(),
        )
        .unwrap_err();
        assert_eq!(not_dir.kind, ErrorKind::InvalidInput);
        let missing = extract(
            root.path(),
            "/nothing",
            &mut tar.as_slice(),
            &PutOptions::default(),
        )
        .unwrap_err();
        assert_eq!(missing.kind, ErrorKind::NotFound);
        assert!(!Path::new(&root.path().join("x")).exists());
    }
}
