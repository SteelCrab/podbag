//! Reading a container's log in the kubelet's format, as it grows (for attach).

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use crate::shim::logbook::Stream;

/// The text of one log line, with its newline when the line is complete (`F`).
pub fn parse_line(line: &[u8]) -> Option<(Stream, Vec<u8>)> {
    let mut parts = line.splitn(4, |&b| b == b' ');
    let _time = parts.next()?;
    let stream = match parts.next()? {
        b"stdout" => Stream::Stdout,
        b"stderr" => Stream::Stderr,
        _ => return None,
    };
    let full = match parts.next()? {
        b"F" => true,
        b"P" => false,
        _ => return None,
    };
    let mut text = parts.next().unwrap_or_default().to_vec();
    if full {
        text.push(b'\n');
    }
    Some((stream, text))
}

/// Follows a log file from where it ends now; a new file at the path (the kubelet rotated
/// it) is read from its start.
pub struct Follower {
    path: PathBuf,
    /// The inode read and the offset in it.
    at: Option<(u64, u64)>,
    carry: Vec<u8>,
}

impl Follower {
    pub fn at_end(path: &str) -> Follower {
        let at = std::fs::metadata(path).ok().map(|m| (m.ino(), m.len()));
        Follower {
            path: PathBuf::from(path),
            at,
            carry: Vec::new(),
        }
    }

    /// The lines written since the last call.
    pub fn next_lines(&mut self) -> Vec<(Stream, Vec<u8>)> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let Ok(meta) = file.metadata() else {
            return Vec::new();
        };
        let offset = match self.at {
            Some((ino, offset)) if ino == meta.ino() && offset <= meta.len() => offset,
            _ => {
                self.carry.clear();
                0
            }
        };
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(offset)).is_err() || file.read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        self.at = Some((meta.ino(), offset + bytes.len() as u64));
        self.carry.extend_from_slice(&bytes);
        let mut out = Vec::new();
        while let Some(end) = self.carry.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.carry.drain(..=end).collect();
            if let Some(parsed) = parse_line(&line[..line.len() - 1]) {
                out.push(parsed);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_give_their_text() {
        assert_eq!(
            parse_line(b"2024-11-17T13:26:40.1Z stdout F hello world"),
            Some((Stream::Stdout, b"hello world\n".to_vec()))
        );
        assert_eq!(
            parse_line(b"2024-11-17T13:26:40.1Z stderr P part"),
            Some((Stream::Stderr, b"part".to_vec()))
        );
        assert_eq!(
            parse_line(b"t stdout F "),
            Some((Stream::Stdout, b"\n".to_vec()))
        );
        assert_eq!(parse_line(b"garbage"), None);
    }

    #[test]
    fn follows_from_the_end_and_across_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("0.log");
        std::fs::write(&path, "t stdout F old\n").unwrap();
        let mut f = Follower::at_end(path.to_str().unwrap());
        assert!(f.next_lines().is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"t stdout F new\nt stderr F hal").unwrap();
        assert_eq!(f.next_lines(), [(Stream::Stdout, b"new\n".to_vec())]);
        std::io::Write::write_all(&mut file, b"f\n").unwrap();
        assert_eq!(f.next_lines(), [(Stream::Stderr, b"half\n".to_vec())]);
        std::fs::rename(&path, tmp.path().join("0.log.1")).unwrap();
        std::fs::write(&path, "t stdout F rotated\n").unwrap();
        assert_eq!(f.next_lines(), [(Stream::Stdout, b"rotated\n".to_vec())]);
    }
}
