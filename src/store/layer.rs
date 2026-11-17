//! Reading layer tar streams into entries, storing file contents as objects.

use super::object::{Meta, ObjectStore};
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;

/// One item of a layer, with a normalized path relative to the image root.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// (name, hex value)
    pub xattrs: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File {
        object: String,
        size: u64,
    },
    Dir,
    Symlink {
        target: String,
    },
    Hardlink {
        target: String,
    },
    Char {
        major: u32,
        minor: u32,
    },
    Block {
        major: u32,
        minor: u32,
    },
    Fifo,
    /// Delete this path from lower layers.
    Whiteout,
    /// Hide everything lower layers put in this directory.
    Opaque,
}

/// The entries of one layer, saved so the layer is never downloaded again.
#[derive(Serialize, Deserialize, Debug)]
pub struct LayerIndex {
    pub diff_id: String,
    pub entries: Vec<Entry>,
}

/// Make a layer path relative and safe. Returns None for the root itself.
pub fn normalize_path(raw: &[u8]) -> Result<Option<String>> {
    let unsafe_path = || {
        Error::new(
            ErrorKind::InvalidInput,
            "cannot read image layer",
            format!(
                "unsafe path {:?} in the layer",
                String::from_utf8_lossy(raw)
            ),
            "the image is malformed or malicious; do not use it",
        )
    };
    if raw.contains(&0) {
        return Err(unsafe_path());
    }
    let text = std::str::from_utf8(raw).map_err(|_| unsafe_path())?;
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(unsafe_path()),
            p => parts.push(p),
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

/// Read a layer tar stream. File contents go to `objects`; the entries come back in order.
pub fn read_layer(tar: impl Read, objects: &ObjectStore) -> Result<Vec<Entry>> {
    let tar_error = |e: std::io::Error| {
        Error::new(
            ErrorKind::Runtime,
            "cannot read image layer",
            e.to_string(),
            "try again; if it keeps failing, the layer is broken",
        )
    };
    let mut archive = tar::Archive::new(tar);
    let mut entries = Vec::new();
    for item in archive.entries().map_err(tar_error)? {
        let mut item = item.map_err(tar_error)?;
        let Some(path) = normalize_path(&item.path_bytes())? else {
            continue;
        };
        let header = item.header().clone();
        let mode = header.mode().map_err(tar_error)? & 0o7777;
        let uid = header.uid().map_err(tar_error)? as u32;
        let gid = header.gid().map_err(tar_error)? as u32;
        let xattrs = read_xattrs(&mut item).map_err(tar_error)?;

        let (dir, base) = match path.rsplit_once('/') {
            Some((d, b)) => (d.to_string(), b.to_string()),
            None => (String::new(), path.clone()),
        };
        let entry = |path: String, kind: EntryKind| Entry {
            path,
            kind,
            mode,
            uid,
            gid,
            xattrs: xattrs.iter().map(|(k, v)| (k.clone(), hex(v))).collect(),
        };
        if base == ".wh..wh..opq" {
            entries.push(entry(dir, EntryKind::Opaque));
            continue;
        }
        if let Some(name) = base.strip_prefix(".wh.") {
            let target = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}/{name}")
            };
            entries.push(entry(target, EntryKind::Whiteout));
            continue;
        }

        use tar::EntryType as T;
        let device = |h: &tar::Header| -> std::io::Result<(u32, u32)> {
            Ok((
                h.device_major()?.unwrap_or(0),
                h.device_minor()?.unwrap_or(0),
            ))
        };
        let kind = match header.entry_type() {
            T::Regular | T::Continuous => {
                let meta = Meta {
                    uid,
                    gid,
                    mode,
                    xattrs: xattrs.clone(),
                };
                let (object, size) = objects.insert(&mut item, &meta)?;
                EntryKind::File { object, size }
            }
            T::Directory => EntryKind::Dir,
            T::Symlink => {
                let target = item
                    .link_name_bytes()
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                EntryKind::Symlink { target }
            }
            T::Link => {
                let raw = item
                    .link_name_bytes()
                    .map(|b| b.into_owned())
                    .unwrap_or_default();
                let target = normalize_path(&raw)?.ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "cannot read image layer",
                        format!("hard link {path} points to the root directory"),
                        "the image is malformed; do not use it",
                    )
                })?;
                EntryKind::Hardlink { target }
            }
            T::Char => {
                let (major, minor) = device(&header).map_err(tar_error)?;
                EntryKind::Char { major, minor }
            }
            T::Block => {
                let (major, minor) = device(&header).map_err(tar_error)?;
                EntryKind::Block { major, minor }
            }
            T::Fifo => EntryKind::Fifo,
            other => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "cannot read image layer",
                    format!("unsupported tar entry type {other:?} at {path}"),
                    "use an image built with standard tools",
                ));
            }
        };
        entries.push(entry(path, kind));
    }
    Ok(entries)
}

/// Extended attributes from PAX headers. Only `security.capability` and `user.*` are kept:
/// other namespaces cannot be set inside a user namespace.
fn read_xattrs<R: Read>(item: &mut tar::Entry<R>) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    if let Some(exts) = item.pax_extensions()? {
        for ext in exts {
            let ext = ext?;
            let Ok(key) = ext.key() else { continue };
            if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
                if name == "security.capability" || name.starts_with("user.") {
                    out.push((name.to_string(), ext.value_bytes().to_vec()));
                }
            }
        }
    }
    Ok(out)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::store::object::ObjectStore;

    fn tar_of(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        build(&mut b);
        b.into_inner().unwrap()
    }

    fn add(
        b: &mut tar::Builder<Vec<u8>>,
        path: &str,
        kind: tar::EntryType,
        data: &[u8],
        link: Option<&str>,
        mode: u32,
        uid: u64,
    ) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_uid(uid);
        h.set_gid(uid);
        h.set_size(data.len() as u64);
        match link {
            Some(l) => b.append_link(&mut h, path, l).unwrap(),
            None => b.append_data(&mut h, path, data).unwrap(),
        }
    }

    #[test]
    fn normalize_rejects_escapes() {
        for bad in [&b"../x"[..], b"a/../../b", b"a\0b", b"x/.."] {
            assert_eq!(
                normalize_path(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
        assert_eq!(normalize_path(b"./a//b/").unwrap().as_deref(), Some("a/b"));
        assert_eq!(
            normalize_path(b"/etc/passwd").unwrap().as_deref(),
            Some("etc/passwd")
        );
        assert_eq!(normalize_path(b".").unwrap(), None);
        assert_eq!(normalize_path(b"/").unwrap(), None);
    }

    #[test]
    fn reads_every_entry_kind() {
        use tar::EntryType as T;
        let tar = tar_of(|b| {
            add(b, "./", T::Directory, b"", None, 0o755, 0);
            add(b, "etc", T::Directory, b"", None, 0o755, 0);
            add(b, "etc/hostname", T::Regular, b"box\n", None, 0o640, 101);
            add(b, "etc/link", T::Symlink, b"", Some("hostname"), 0o777, 0);
            add(
                b,
                "etc/hard",
                T::Link,
                b"",
                Some("etc/hostname"),
                0o640,
                101,
            );
            add(b, "run/fifo", T::Fifo, b"", None, 0o600, 0);
            add(b, "etc/.wh.old", T::Regular, b"", None, 0o644, 0);
            add(b, "opt/.wh..wh..opq", T::Regular, b"", None, 0o644, 0);
        });
        let tmp = tempfile::tempdir().unwrap();
        let objects = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let entries = read_layer(tar.as_slice(), &objects).unwrap();
        let kinds: Vec<(&str, &EntryKind)> =
            entries.iter().map(|e| (e.path.as_str(), &e.kind)).collect();
        let host_obj = match &entries[1].kind {
            EntryKind::File { object, size } => {
                assert_eq!(*size, 4);
                object.clone()
            }
            other => panic!("expected a file, got {other:?}"),
        };
        assert_eq!(
            kinds,
            vec![
                ("etc", &EntryKind::Dir),
                (
                    "etc/hostname",
                    &EntryKind::File {
                        object: host_obj,
                        size: 4
                    }
                ),
                (
                    "etc/link",
                    &EntryKind::Symlink {
                        target: "hostname".into()
                    }
                ),
                (
                    "etc/hard",
                    &EntryKind::Hardlink {
                        target: "etc/hostname".into()
                    }
                ),
                ("run/fifo", &EntryKind::Fifo),
                ("etc/old", &EntryKind::Whiteout),
                ("opt", &EntryKind::Opaque),
            ]
        );
        assert_eq!(
            (entries[1].mode, entries[1].uid, entries[1].gid),
            (0o640, 101, 101)
        );
    }

    #[test]
    fn keeps_only_supported_xattrs() {
        let mut b = tar::Builder::new(Vec::new());
        let pax: Vec<(&str, &[u8])> = vec![
            ("SCHILY.xattr.security.capability", b"\x01\x00"),
            ("SCHILY.xattr.user.a", b"1"),
            ("SCHILY.xattr.trusted.b", b"2"),
        ];
        b.append_pax_extensions(pax).unwrap();
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o755);
        h.set_uid(0);
        h.set_gid(0);
        h.set_size(1);
        b.append_data(&mut h, "bin/ping", &b"x"[..]).unwrap();
        let tar = b.into_inner().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let objects = ObjectStore::new(tmp.path().join("objects"), false).unwrap();
        let entries = read_layer(tar.as_slice(), &objects).unwrap();
        let names: Vec<&str> = entries[0].xattrs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["security.capability", "user.a"]);
        assert_eq!(entries[0].xattrs[0].1, "0100");
    }
}
