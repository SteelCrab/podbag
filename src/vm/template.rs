//! The VM's empty data disk, as the build stores it: `OPDISK1\n`, the disk's size (u64,
//! little-endian), then records of `[u64 offset][u32 length][bytes]` for the blocks that
//! are not zero; the whole zstd-compressed (`dev/macos/build-guest.sh` writes it). Written
//! out, everything else stays a hole: a 64 GiB disk takes a few MiB on the Mac.

use crate::{Error, ErrorKind, Result};
use std::io::{BufReader, Read};
use std::path::Path;

const MAGIC: &[u8; 8] = b"OPDISK1\n";

/// Write the disk described by `template` (compressed or not) to `dest`, a new file.
pub fn expand(template: &mut dyn Read, dest: &Path) -> Result<u64> {
    let bad = |why: String| {
        Error::new(
            ErrorKind::Internal,
            "cannot create the VM's data disk",
            why,
            "rebuild the guest (dev/macos/build-guest.sh)",
        )
    };
    let mut input = BufReader::new(template);
    let mut magic = [0u8; 8];
    let start = {
        use std::io::BufRead;
        input
            .fill_buf()
            .map_err(|e| bad(e.to_string()))?
            .starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
    };
    let mut reader: Box<dyn Read + '_> = if start {
        Box::new(
            ruzstd::decoding::StreamingDecoder::new(input)
                .map_err(|e| bad(format!("the template is not zstd: {e}")))?,
        )
    } else {
        Box::new(input)
    };
    reader
        .read_exact(&mut magic)
        .map_err(|e| bad(e.to_string()))?;
    if &magic != MAGIC {
        return Err(bad("the template has the wrong format".into()));
    }
    let mut word = [0u8; 8];
    reader
        .read_exact(&mut word)
        .map_err(|e| bad(e.to_string()))?;
    let size = u64::from_le_bytes(word);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| {
            Error::io(
                format!("cannot create {}", dest.display()),
                &e,
                "check the directory",
            )
        })?;
    file.set_len(size).map_err(|e| {
        Error::io(
            format!("cannot size {}", dest.display()),
            &e,
            "check the free space",
        )
    })?;
    let mut head = [0u8; 12];
    loop {
        match reader.read_exact(&mut head[..1]) {
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            other => other.map_err(|e| bad(e.to_string()))?,
        }
        reader
            .read_exact(&mut head[1..])
            .map_err(|e| bad(e.to_string()))?;
        let offset = u64::from_le_bytes(head[..8].try_into().unwrap_or_default());
        let len = u32::from_le_bytes(head[8..].try_into().unwrap_or_default()) as u64;
        if offset.checked_add(len).is_none_or(|end| end > size) || len > 64 << 20 {
            return Err(bad(format!("a record at {offset} leaves the disk")));
        }
        let mut data = vec![0u8; len as usize];
        reader
            .read_exact(&mut data)
            .map_err(|e| bad(e.to_string()))?;
        write_at(&file, &data, offset).map_err(|e| {
            Error::io(
                format!("cannot write {}", dest.display()),
                &e,
                "check the free space",
            )
        })?;
    }
    Ok(size)
}

#[cfg(unix)]
fn write_at(file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}

#[cfg(not(unix))]
fn write_at(file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = file;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(size: u64, blocks: &[(u64, &[u8])]) -> Vec<u8> {
        let mut t = MAGIC.to_vec();
        t.extend(size.to_le_bytes());
        for (offset, data) in blocks {
            t.extend(offset.to_le_bytes());
            t.extend((data.len() as u32).to_le_bytes());
            t.extend(*data);
        }
        t
    }

    #[test]
    fn a_template_becomes_a_sparse_disk() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.img");
        let size = 256u64 << 20;
        let t = template(
            size,
            &[
                (0, b"superblock"),
                (100 << 20, b"middle"),
                (size - 4, b"end!"),
            ],
        );
        assert_eq!(expand(&mut t.as_slice(), &disk).unwrap(), size);
        let data = std::fs::read(&disk).unwrap();
        assert_eq!(data.len() as u64, size);
        assert_eq!(&data[..10], b"superblock");
        assert_eq!(&data[100 << 20..(100 << 20) + 6], b"middle");
        assert_eq!(&data[data.len() - 4..], b"end!");
        assert!(data[20..100].iter().all(|b| *b == 0));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let used = std::fs::metadata(&disk).unwrap().blocks() * 512;
            assert!(
                used < size / 16,
                "the disk is not sparse: {used} bytes used"
            );
        }
        // It is never written over.
        assert!(expand(&mut t.as_slice(), &disk).is_err());
    }

    #[test]
    fn records_outside_the_disk_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let t = template(4096, &[(4090, b"too far")]);
        assert!(expand(&mut t.as_slice(), &dir.path().join("d")).is_err());
        let wrong = b"NOTADISK00000000".to_vec();
        assert!(expand(&mut wrong.as_slice(), &dir.path().join("e")).is_err());
    }
}
