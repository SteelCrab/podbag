use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Each log file holds up to this many bytes; with one older file kept, a container keeps
/// about 10 MiB of logs.
pub const LOG_FILE_LIMIT: u64 = 5 << 20;

/// Longest log line. Longer output without a newline is split, as Docker does.
pub const MAX_LINE: usize = 16 << 10;

const CURRENT: &str = "log.json";
const PREVIOUS: &str = "log.json.1";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One line of output, with the same keys as Docker's json-file driver.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub log: String,
    pub stream: Stream,
    pub time: String,
}

/// Appends output to `log.json`, a line at a time. When the file passes the limit it
/// becomes `log.json.1` (replacing the older one) and a new file starts.
pub struct LogWriter {
    dir: PathBuf,
    file: File,
    size: u64,
    limit: u64,
    partial: [Vec<u8>; 2],
}

impl LogWriter {
    pub fn open(dir: &Path, limit: u64) -> Result<LogWriter> {
        let path = dir.join(CURRENT);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| log_error(&path, e))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(LogWriter {
            dir: dir.to_path_buf(),
            file,
            size,
            limit,
            partial: [Vec::new(), Vec::new()],
        })
    }

    /// Add output. Complete lines are written now; a trailing partial line waits.
    /// A line reaches at most [`MAX_LINE`] bytes; longer output is split there.
    pub fn write(&mut self, stream: Stream, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let partial = &mut self.partial[stream as usize];
            let room = MAX_LINE - partial.len();
            let chunk = &bytes[..bytes.len().min(room)];
            let end = match chunk.iter().position(|&b| b == b'\n') {
                Some(pos) => pos + 1,
                None => chunk.len(),
            };
            partial.extend_from_slice(&chunk[..end]);
            bytes = &bytes[end..];
            if partial.last() == Some(&b'\n') || partial.len() == MAX_LINE {
                let line = std::mem::take(partial);
                self.emit(stream, &line)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn pending(&self, stream: Stream) -> usize {
        self.partial[stream as usize].len()
    }

    /// Write out partial lines (when the container ends).
    pub fn flush_partial(&mut self) -> Result<()> {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let line = std::mem::take(&mut self.partial[stream as usize]);
            if !line.is_empty() {
                self.emit(stream, &line)?;
            }
        }
        Ok(())
    }

    fn emit(&mut self, stream: Stream, line: &[u8]) -> Result<()> {
        let (secs, nanos) = now();
        let entry = LogLine {
            log: String::from_utf8_lossy(line).into_owned(),
            stream,
            time: rfc3339(secs, nanos),
        };
        let mut json = serde_json::to_string(&entry).unwrap_or_default();
        json.push('\n');
        if self.size > 0 && self.size + json.len() as u64 > self.limit {
            self.rotate()?;
        }
        let path = self.dir.join(CURRENT);
        self.file
            .write_all(json.as_bytes())
            .map_err(|e| log_error(&path, e))?;
        self.size += json.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        let (current, previous) = (self.dir.join(CURRENT), self.dir.join(PREVIOUS));
        std::fs::rename(&current, &previous).map_err(|e| log_error(&current, e))?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current)
            .map_err(|e| log_error(&current, e))?;
        self.size = 0;
        Ok(())
    }
}

/// Every stored log line, oldest first.
pub fn read_logs(dir: &Path) -> Result<Vec<LogLine>> {
    let mut out = Vec::new();
    for name in [PREVIOUS, CURRENT] {
        let path = dir.join(name);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(log_error(&path, e)),
        };
        for line in BufReader::new(file).lines() {
            if let Ok(entry) =
                serde_json::from_str::<LogLine>(&line.map_err(|e| log_error(&path, e))?)
            {
                out.push(entry);
            }
        }
    }
    Ok(out)
}

/// Reads log lines as they are written (`logs -f`). The first call returns every stored
/// line. It follows `log.json` by byte offset; when the writer rotates it, the rest of the
/// old file (now `log.json.1`) is read first.
pub struct LogFollower {
    dir: PathBuf,
    /// Inode of the file being read and the offset reached in it.
    at: Option<(u64, u64)>,
    /// The start of a line whose end is not written yet.
    carry: Vec<u8>,
}

impl LogFollower {
    pub fn new(dir: &Path) -> LogFollower {
        LogFollower {
            dir: dir.to_path_buf(),
            at: None,
            carry: Vec::new(),
        }
    }

    pub fn next_lines(&mut self) -> Result<Vec<LogLine>> {
        use std::os::unix::fs::MetadataExt;
        let mut out = Vec::new();
        let (current, previous) = (self.dir.join(CURRENT), self.dir.join(PREVIOUS));
        let inode = |p: &Path| std::fs::metadata(p).ok().map(|m| m.ino());
        match self.at {
            None => {
                if inode(&previous).is_some() {
                    self.read_from(&previous, 0, &mut out)?;
                }
            }
            Some((ino, offset)) if inode(&current) != Some(ino) => {
                // Rotated: finish the old file, if it is still there.
                if inode(&previous) == Some(ino) {
                    self.read_from(&previous, offset, &mut out)?;
                }
                self.carry.clear();
                self.at = None;
            }
            Some(_) => {}
        }
        let Some(ino) = inode(&current) else {
            return Ok(out);
        };
        let offset = self.at.map_or(0, |(_, o)| o);
        let end = self.read_from(&current, offset, &mut out)?;
        self.at = Some((ino, end));
        Ok(out)
    }

    /// Parse complete lines from `offset` to the end. Returns the new offset.
    fn read_from(&mut self, path: &Path, offset: u64, out: &mut Vec<LogLine>) -> Result<u64> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(offset),
            Err(e) => return Err(log_error(path, e)),
        };
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_to_end(&mut bytes))
            .map_err(|e| log_error(path, e))?;
        self.carry.extend_from_slice(&bytes);
        while let Some(pos) = self.carry.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.carry.drain(..=pos).collect();
            if let Ok(entry) = serde_json::from_slice::<LogLine>(&line) {
                out.push(entry);
            }
        }
        Ok(offset + bytes.len() as u64)
    }
}

fn now() -> (u64, u32) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}

/// `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ` for a Unix time.
pub fn rfc3339(secs: u64, nanos: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{nanos:09}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn log_error(path: &Path, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot write the container log {}", path.display()),
        &e,
        "check free disk space",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(dir: &std::path::Path) -> Vec<(Stream, String)> {
        read_logs(dir)
            .unwrap()
            .into_iter()
            .map(|l| (l.stream, l.log))
            .collect()
    }

    #[test]
    fn splits_chunks_into_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = LogWriter::open(tmp.path(), 1 << 20).unwrap();
        w.write(Stream::Stdout, b"a\nb\nc").unwrap();
        w.write(Stream::Stderr, b"err\n").unwrap();
        w.write(Stream::Stdout, b"d\n").unwrap();
        assert_eq!(
            lines(tmp.path()),
            [
                (Stream::Stdout, "a\n".to_string()),
                (Stream::Stdout, "b\n".into()),
                (Stream::Stderr, "err\n".into()),
                (Stream::Stdout, "cd\n".into())
            ]
        );
    }

    #[test]
    fn keeps_partial_lines_until_flush() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = LogWriter::open(tmp.path(), 1 << 20).unwrap();
        w.write(Stream::Stdout, b"no newline").unwrap();
        assert!(lines(tmp.path()).is_empty());
        w.flush_partial().unwrap();
        assert_eq!(
            lines(tmp.path()),
            [(Stream::Stdout, "no newline".to_string())]
        );
    }

    #[test]
    fn log_rotates_and_keeps_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = LogWriter::open(tmp.path(), 1024).unwrap();
        for i in 0..100 {
            w.write(Stream::Stdout, format!("line {i:03}\n").as_bytes())
                .unwrap();
        }
        let all = lines(tmp.path());
        assert_eq!(all.last().unwrap().1, "line 099\n");
        let files: u64 = ["log.json", "log.json.1"]
            .iter()
            .map(|f| {
                std::fs::metadata(tmp.path().join(f))
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum();
        assert!(files <= 2 * 1024 + 200, "{files} bytes kept");
        assert!(
            !all.iter().any(|(_, l)| l == "line 000\n"),
            "the oldest lines are dropped"
        );
        // Lines come out in order.
        let numbers: Vec<u32> = all.iter().map(|(_, l)| l[5..8].parse().unwrap()).collect();
        assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "{numbers:?}");
    }

    // Output without newlines (progress bars, binary data) must not pile up in memory.
    #[test]
    fn long_lines_are_split_at_16_kib() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = LogWriter::open(tmp.path(), 1 << 20).unwrap();
        for _ in 0..10 {
            w.write(Stream::Stdout, &[b'x'; 5000]).unwrap();
        }
        assert!(
            w.pending(Stream::Stdout) < MAX_LINE,
            "{} bytes buffered",
            w.pending(Stream::Stdout)
        );
        w.flush_partial().unwrap();
        let all = lines(tmp.path());
        assert!(all.iter().all(|(_, l)| l.len() <= MAX_LINE));
        assert_eq!(all.iter().map(|(_, l)| l.len()).sum::<usize>(), 50_000);
    }

    #[test]
    fn follower_continues_across_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = LogWriter::open(tmp.path(), 200).unwrap();
        let mut f = LogFollower::new(tmp.path());
        let mut seen = Vec::new();
        for i in 0..30 {
            w.write(Stream::Stdout, format!("line {i:02}\n").as_bytes())
                .unwrap();
            seen.extend(f.next_lines().unwrap().into_iter().map(|l| l.log));
        }
        let want: Vec<String> = (0..30).map(|i| format!("line {i:02}\n")).collect();
        assert_eq!(seen, want);
    }

    #[test]
    fn time_is_rfc3339_utc() {
        let t = rfc3339(1_731_850_000, 123_456_789);
        assert_eq!(t, "2024-11-17T13:26:40.123456789Z");
        assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00.000000000Z");
    }
}
