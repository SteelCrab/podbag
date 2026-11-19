//! The guest files a release build carries (the `embed-guest` feature): written once per
//! guest to `~/.openpod/vm/<id>/`, so that the openpod binary alone runs containers on a
//! Mac (design 2026-11-19 macOS, 3.1).

use std::io::Write;
use std::path::{Path, PathBuf};

/// The guest's files: name, bytes, and whether the bytes are zstd-compressed.
#[derive(Clone, Copy)]
pub struct Embedded {
    /// Names this guest: another guest gets a directory of its own.
    pub id: &'static str,
    pub files: &'static [(&'static str, &'static [u8], bool)],
}

#[cfg(feature = "embed-guest")]
pub const EMBEDDED: Option<Embedded> = Some(Embedded {
    id: env!("OPENPOD_GUEST_ID"),
    files: &[
        (
            "Image",
            include_bytes!(concat!(env!("OPENPOD_GUEST_DIR"), "/Image.zst")),
            true,
        ),
        (
            "initramfs.gz",
            include_bytes!(concat!(env!("OPENPOD_GUEST_DIR"), "/initramfs.gz")),
            false,
        ),
        (
            "disk.zst",
            include_bytes!(concat!(env!("OPENPOD_GUEST_DIR"), "/disk.zst")),
            false,
        ),
    ],
});

#[cfg(not(feature = "embed-guest"))]
pub const EMBEDDED: Option<Embedded> = None;

/// Written last, holding the id and each file's size: a directory without it was left by a
/// process that died while writing, and one whose files no longer have those sizes (cut,
/// removed) is written again.
const DONE: &str = "complete";

/// `<root>/<id>/` with the files, written unless a complete copy is there: into a
/// directory of this process first, then renamed into place.
pub fn extract(e: &Embedded, root: &Path) -> std::io::Result<PathBuf> {
    let dir = root.join(e.id);
    if complete(&dir, e.id) {
        return Ok(dir);
    }
    std::fs::create_dir_all(root)?;
    let part = root.join(format!(".{}-{}.part", e.id, std::process::id()));
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir(&part)?;
    if let Err(err) = write_files(e, &part) {
        let _ = std::fs::remove_dir_all(&part);
        return Err(err);
    }
    if dir.exists() && !complete(&dir, e.id) {
        std::fs::remove_dir_all(&dir)?;
    }
    match std::fs::rename(&part, &dir) {
        Ok(()) => Ok(dir),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&part);
            // Another openpod wrote the same guest meanwhile.
            if complete(&dir, e.id) {
                Ok(dir)
            } else {
                Err(err)
            }
        }
    }
}

fn write_files(e: &Embedded, dir: &Path) -> std::io::Result<()> {
    let mut done = format!("{}\n", e.id);
    for (name, bytes, zstd) in e.files {
        let mut f = std::fs::File::create(dir.join(name))?;
        if *zstd {
            let mut decoder = ruzstd::decoding::StreamingDecoder::new(*bytes).map_err(|err| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string())
            })?;
            std::io::copy(&mut decoder, &mut f)?;
        } else {
            f.write_all(bytes)?;
        }
        f.sync_all()?;
        done.push_str(&format!("{} {name}\n", f.metadata()?.len()));
    }
    std::fs::write(dir.join(DONE), done)
}

/// The marker names this guest, and every file it lists has the size it lists.
fn complete(dir: &Path, id: &str) -> bool {
    let Ok(done) = std::fs::read_to_string(dir.join(DONE)) else {
        return false;
    };
    let mut lines = done.lines();
    if lines.next() != Some(id) {
        return false;
    }
    let mut files = 0;
    for line in lines {
        let sized = line.split_once(' ').and_then(|(size, name)| {
            let len = std::fs::metadata(dir.join(name)).ok()?.len();
            Some(size.parse::<u64>().ok()? == len)
        });
        if sized != Some(true) {
            return false;
        }
        files += 1;
    }
    files > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guest(id: &'static str) -> Embedded {
        let image: &'static [u8] = Box::leak(
            ruzstd::encoding::compress_to_vec(
                &b"kernel bytes"[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            )
            .into_boxed_slice(),
        );
        let files: &'static [(&'static str, &'static [u8], bool)] = Box::leak(Box::new([
            ("Image", image, true),
            ("initramfs.gz", &b"initramfs"[..], false),
        ]));
        Embedded { id, files }
    }

    #[test]
    fn the_guest_is_written_once_with_its_files() {
        let root = tempfile::tempdir().unwrap();
        let e = guest("aaaa");
        let dir = extract(&e, root.path()).unwrap();
        assert_eq!(dir, root.path().join("aaaa"));
        assert_eq!(std::fs::read(dir.join("Image")).unwrap(), b"kernel bytes");
        assert_eq!(
            std::fs::read(dir.join("initramfs.gz")).unwrap(),
            b"initramfs"
        );
        // A complete copy is used as it is: not written again.
        use std::os::unix::fs::MetadataExt;
        let inode = |p: &Path| std::fs::metadata(p).unwrap().ino();
        let first = inode(&dir.join("Image"));
        assert_eq!(extract(&e, root.path()).unwrap(), dir);
        assert_eq!(inode(&dir.join("Image")), first);
        // Nothing else is left beside it.
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_cut_or_lost_file_is_written_again() {
        let root = tempfile::tempdir().unwrap();
        let e = guest("eeee");
        let dir = extract(&e, root.path()).unwrap();
        std::fs::write(dir.join("Image"), b"cut").unwrap();
        extract(&e, root.path()).unwrap();
        assert_eq!(std::fs::read(dir.join("Image")).unwrap(), b"kernel bytes");
        std::fs::remove_file(dir.join("initramfs.gz")).unwrap();
        extract(&e, root.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.join("initramfs.gz")).unwrap(),
            b"initramfs"
        );
    }

    #[test]
    fn a_half_written_copy_is_written_again() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("bbbb");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Image"), b"cut sh").unwrap();
        assert_eq!(extract(&guest("bbbb"), root.path()).unwrap(), dir);
        assert_eq!(std::fs::read(dir.join("Image")).unwrap(), b"kernel bytes");
        assert!(complete(&dir, "bbbb"));
    }

    #[test]
    fn another_guest_gets_a_directory_of_its_own() {
        let root = tempfile::tempdir().unwrap();
        let old = extract(&guest("cccc"), root.path()).unwrap();
        let new = extract(&guest("dddd"), root.path()).unwrap();
        assert_ne!(old, new);
        assert!(complete(&old, "cccc") && complete(&new, "dddd"));
    }
}
