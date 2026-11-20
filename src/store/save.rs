//! `docker save`: images of the store as a `docker save` archive (`manifest.json`, the
//! configs and layers under `blobs/sha256/`).
//!
//! The store keeps an image's files, not the layer archives it came in, so each layer is
//! written anew from its entries (in the order they were read, every time stamp the
//! image's creation time). Its digest differs from the original layer's; the config
//! written with it names the new digests. The same store writes the same archive.

use super::{Store, io_error};
use crate::image::manifest;
use crate::store::layer::{EntryKind, LayerIndex};
use crate::{Error, ErrorKind, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};

impl Store {
    /// Write the images `names` (names or IDs) as one archive to `out`.
    pub fn save(&self, names: &[String], out: &mut dyn Write) -> Result<()> {
        let _shared = self.lock(false)?;
        // One entry per image, with all the names asked for it.
        let mut images: Vec<(super::ImageRecord, Vec<String>)> = Vec::new();
        for name in names {
            let (full, record) = self.find(name)?;
            // An image found by its ID has no name to give.
            let tag = (!full.starts_with("sha256:")).then_some(full);
            match images.iter_mut().find(|(r, _)| r.digest == record.digest) {
                Some((_, tags)) => tags.extend(tag),
                None => images.push((record, tag.into_iter().collect())),
            }
        }
        let mut tar = tar::Builder::new(out);
        match self.write_archive(&mut tar, &images) {
            Ok(()) => tar.finish().map_err(write_error),
            Err(e) => {
                // Without the end blocks a dropped builder would write: a reader must see
                // the archive cut short, not a smaller one that looks complete.
                std::mem::forget(tar);
                Err(e)
            }
        }
    }

    fn write_archive(
        &self,
        tar: &mut tar::Builder<&mut dyn Write>,
        images: &[(super::ImageRecord, Vec<String>)],
    ) -> Result<()> {
        let mut written = HashSet::new();
        let mut entries = Vec::new();
        for (record, tags) in images {
            let mut layers = Vec::new();
            let mut diff_ids = Vec::new();
            for id in &record.diff_ids {
                let index = self.read_layer_index(id)?;
                // Once to learn the digest and size, once into the archive.
                let mut hashed = HashWriter::default();
                self.write_layer(&index, record.created, &mut hashed)?;
                let (hex, size) = hashed.finish();
                let path = format!("blobs/sha256/{hex}");
                if written.insert(hex.clone()) {
                    append_streamed(tar, &path, size, record.created, &mut |w| {
                        self.write_layer(&index, record.created, w)
                    })?;
                }
                diff_ids.push(format!("sha256:{hex}"));
                layers.push(path);
            }
            let config = manifest::config_json(&record.config, &diff_ids, record.created);
            let hex = crate::image::registry::sha256_hex(&config);
            let config_path = format!("blobs/sha256/{hex}");
            if written.insert(hex) {
                append_bytes(tar, &config_path, &config, record.created)?;
            }
            entries.push(serde_json::json!({
                "Config": config_path,
                "RepoTags": tags.iter().map(|t| familiar(t)).collect::<Vec<_>>(),
                "Layers": layers,
            }));
        }
        let manifest = serde_json::to_vec(&entries).unwrap_or_default();
        append_bytes(tar, "manifest.json", &manifest, 0)
    }

    /// One layer as a tar, from its entries and the objects that hold its files.
    pub(super) fn write_layer(
        &self,
        index: &LayerIndex,
        mtime: u64,
        out: &mut dyn Write,
    ) -> Result<()> {
        let mut b = tar::Builder::new(out);
        for e in &index.entries {
            let mut h = tar::Header::new_gnu();
            h.set_mode(e.mode & 0o7777);
            h.set_uid(u64::from(e.uid));
            h.set_gid(u64::from(e.gid));
            // A layer stored by an older podbag has no times: the image's.
            h.set_mtime(e.mtime.unwrap_or(mtime));
            h.set_size(0);
            if !e.xattrs.is_empty() {
                let values: Vec<(String, Vec<u8>)> = e
                    .xattrs
                    .iter()
                    .map(|(k, v)| (format!("SCHILY.xattr.{k}"), unhex(v)))
                    .collect();
                b.append_pax_extensions(values.iter().map(|(k, v)| (k.as_str(), v.as_slice())))
                    .map_err(write_error)?;
            }
            let path = e.path.as_str();
            let empty = std::io::empty();
            let done = match &e.kind {
                EntryKind::File { object, size } => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(*size);
                    let file_path = self.objects.path(object);
                    let file =
                        std::fs::File::open(&file_path).map_err(|e| io_error(&file_path, e))?;
                    b.append_data(&mut h, path, Exact::new(file, *size))
                }
                EntryKind::Dir => {
                    h.set_entry_type(tar::EntryType::Directory);
                    b.append_data(&mut h, format!("{path}/"), empty)
                }
                EntryKind::Symlink { target } => {
                    h.set_entry_type(tar::EntryType::Symlink);
                    b.append_link(&mut h, path, target)
                }
                EntryKind::Hardlink { target } => {
                    h.set_entry_type(tar::EntryType::Link);
                    b.append_link(&mut h, path, target)
                }
                EntryKind::Char { major, minor } | EntryKind::Block { major, minor } => {
                    h.set_entry_type(if matches!(e.kind, EntryKind::Char { .. }) {
                        tar::EntryType::Char
                    } else {
                        tar::EntryType::Block
                    });
                    h.set_device_major(*major).map_err(write_error)?;
                    h.set_device_minor(*minor).map_err(write_error)?;
                    b.append_data(&mut h, path, empty)
                }
                EntryKind::Fifo => {
                    h.set_entry_type(tar::EntryType::Fifo);
                    b.append_data(&mut h, path, empty)
                }
                EntryKind::Whiteout => {
                    h.set_entry_type(tar::EntryType::Regular);
                    let marked = match path.rsplit_once('/') {
                        Some((dir, base)) => format!("{dir}/.wh.{base}"),
                        None => format!(".wh.{path}"),
                    };
                    b.append_data(&mut h, marked, empty)
                }
                EntryKind::Opaque => {
                    h.set_entry_type(tar::EntryType::Regular);
                    b.append_data(&mut h, format!("{path}/.wh..wh..opq"), empty)
                }
            };
            done.map_err(write_error)?;
        }
        b.finish().map_err(write_error)
    }
}

/// `docker.io/library/alpine:3` → `alpine:3`, as `docker save` names images.
fn familiar(full: &str) -> String {
    full.strip_prefix("docker.io/library/")
        .or_else(|| full.strip_prefix("docker.io/"))
        .unwrap_or(full)
        .to_string()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

fn append_bytes(
    tar: &mut tar::Builder<&mut dyn Write>,
    path: &str,
    data: &[u8],
    mtime: u64,
) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_mode(0o644);
    h.set_mtime(mtime);
    h.set_size(data.len() as u64);
    tar.append_data(&mut h, path, data).map_err(write_error)
}

/// A file entry whose `size` bytes `write` produces straight into the archive.
fn append_streamed(
    tar: &mut tar::Builder<&mut dyn Write>,
    path: &str,
    size: u64,
    mtime: u64,
    write: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_mode(0o644);
    h.set_mtime(mtime);
    h.set_size(size);
    h.set_path(path).map_err(write_error)?;
    h.set_cksum();
    // The header alone: an empty body adds no padding.
    tar.append(&h, std::io::empty()).map_err(write_error)?;
    let mut counted = Counted {
        inner: &mut **tar.get_mut(),
        n: 0,
    };
    write(&mut counted)?;
    if counted.n != size {
        return Err(Error::new(
            ErrorKind::Internal,
            "cannot save the image",
            format!("a layer came out as {} bytes, then as {size}", counted.n),
            "report this bug",
        ));
    }
    let pad = ((512 - size % 512) % 512) as usize;
    counted
        .inner
        .write_all(&[0u8; 512][..pad])
        .map_err(write_error)
}

struct Counted<W> {
    inner: W,
    n: u64,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Default)]
struct HashWriter {
    hasher: Sha256,
    n: u64,
}

impl HashWriter {
    fn finish(self) -> (String, u64) {
        let hex = self
            .hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        (hex, self.n)
    }
}

impl Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        self.n += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `size` bytes of a file, padded with zeros if it is shorter (it never is: objects do
/// not change).
struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Exact<R> {
    fn new(inner: R, size: u64) -> Exact<R> {
        Exact { inner, left: size }
    }
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

fn write_error(e: std::io::Error) -> Error {
    Error::io(
        "cannot write the image archive",
        &e,
        "check that the reader is still there and that there is space for it",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn layer(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        build(&mut b);
        b.into_inner().unwrap()
    }

    fn add(b: &mut tar::Builder<Vec<u8>>, path: &str, kind: tar::EntryType, data: &[u8]) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_mode(0o755);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_size(data.len() as u64);
        if kind == tar::EntryType::Symlink {
            b.append_link(&mut h, path, std::str::from_utf8(data).unwrap())
                .unwrap();
        } else {
            b.append_data(&mut h, path, data).unwrap();
        }
    }

    /// `docker save` keeps file times; so does a round trip through podbag.
    #[test]
    fn a_saved_image_keeps_its_file_times() {
        use tar::EntryType as T;
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("store"), false, false).unwrap();
        let at = |b: &mut tar::Builder<Vec<u8>>, path: &str, kind: T, data: &[u8], t: u64| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_mode(0o755);
            h.set_uid(0);
            h.set_gid(0);
            h.set_mtime(t);
            h.set_size(data.len() as u64);
            if kind == T::Symlink {
                b.append_link(&mut h, path, std::str::from_utf8(data).unwrap())
                    .unwrap();
            } else {
                b.append_data(&mut h, path, data).unwrap();
            }
        };
        let only = layer(|b| {
            at(b, "app", T::Directory, b"", 100);
            at(b, "app/main.py", T::Regular, b"print(1)\n", 200);
            at(b, "app/link", T::Symlink, b"main.py", 300);
        });
        store
            .import_for_test("example.com/app:1", vec![only], 1)
            .unwrap();
        let mut archive = Vec::new();
        store
            .save(&["example.com/app:1".to_string()], &mut archive)
            .unwrap();
        let other = tempfile::tempdir().unwrap();
        let fresh = Store::open_at(other.path().join("store"), false, false).unwrap();
        fresh
            .load(&mut archive.as_slice(), u64::MAX, &HashMap::new)
            .unwrap();
        let (_, record) = fresh.find("example.com/app:1").unwrap();
        let root = fresh.rootfs_path(&record.digest);
        let at = |p: &str| {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(root.join(p)).unwrap().mtime()
        };
        assert_eq!(["app", "app/main.py", "app/link"].map(at), [100, 200, 300]);
    }

    #[test]
    fn a_saved_image_loads_back_with_the_same_files() {
        use tar::EntryType as T;
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("store"), false, false).unwrap();
        let base = layer(|b| {
            add(b, "bin", T::Directory, b"");
            add(b, "bin/tool", T::Regular, b"#!/bin/sh\necho tool\n");
            add(b, "etc", T::Directory, b"");
            add(b, "etc/gone", T::Regular, b"old");
            add(b, "etc/link", T::Symlink, b"../bin/tool");
        });
        let top = layer(|b| {
            add(b, "etc/.wh.gone", T::Regular, b"");
            add(b, "etc/new", T::Regular, b"new");
        });
        store
            .import_for_test("example.com/app:1", vec![base, top], 1)
            .unwrap();
        let mut archive = Vec::new();
        store
            .save(&["example.com/app:1".to_string()], &mut archive)
            .unwrap();
        // The same store writes the same archive.
        let mut again = Vec::new();
        store
            .save(&["example.com/app:1".to_string()], &mut again)
            .unwrap();
        assert_eq!(archive, again);

        let other = tempfile::tempdir().unwrap();
        let fresh = Store::open_at(other.path().join("store"), false, false).unwrap();
        let none = HashMap::new;
        let report = fresh
            .load(&mut archive.as_slice(), u64::MAX, &none)
            .unwrap();
        assert_eq!(report.names, ["example.com/app:1"]);
        let (_, record) = fresh.find("example.com/app:1").unwrap();
        assert_eq!(record.diff_ids.len(), 2);
        let root = fresh.rootfs_path(&record.digest);
        assert_eq!(
            std::fs::read_to_string(root.join("bin/tool")).unwrap(),
            "#!/bin/sh\necho tool\n"
        );
        assert_eq!(std::fs::read(root.join("etc/new")).unwrap(), b"new");
        assert!(!root.join("etc/gone").exists(), "the whiteout was lost");
        assert_eq!(
            std::fs::read_link(root.join("etc/link")).unwrap(),
            std::path::Path::new("../bin/tool")
        );
        // An unknown name is refused before anything is written.
        let mut nothing = Vec::new();
        let err = store
            .save(&["example.com/none:1".to_string()], &mut nothing)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
        assert!(nothing.is_empty());
    }
}
