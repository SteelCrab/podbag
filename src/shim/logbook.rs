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
///
/// For a Kubernetes container it writes the kubelet's format to the kubelet's file instead
/// (see [`cri_line`]). The kubelet rotates that file itself: it renames it and asks for a
/// new one, so the writer opens the path again once it names another file.
pub struct LogWriter {
    dir: PathBuf,
    file: File,
    size: u64,
    limit: u64,
    partial: [Vec<u8>; 2],
    /// The kubelet's file, and the inode the writer has open.
    cri: Option<(PathBuf, u64)>,
}

impl LogWriter {
    pub fn open(dir: &Path, limit: u64) -> Result<LogWriter> {
        let path = dir.join(CURRENT);
        let file = append(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(LogWriter {
            dir: dir.to_path_buf(),
            file,
            size,
            limit,
            partial: [Vec::new(), Vec::new()],
            cri: None,
        })
    }

    /// A writer of the kubelet's log format to `path` (its folder is made when missing).
    pub fn open_cri(path: &Path) -> Result<LogWriter> {
        use std::os::unix::fs::MetadataExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| log_error(path, e))?;
        }
        let file = append(path)?;
        let ino = file.metadata().map(|m| m.ino()).unwrap_or(0);
        Ok(LogWriter {
            dir: path.parent().unwrap_or(Path::new("/")).to_path_buf(),
            file,
            size: 0,
            limit: u64::MAX,
            partial: [Vec::new(), Vec::new()],
            cri: Some((path.to_path_buf(), ino)),
        })
    }

    /// The writer for a container's output: the kubelet's file when it has one.
    pub fn for_container(
        dirs: &crate::paths::Dirs,
        spec: &crate::spec::ContainerSpec,
    ) -> Result<LogWriter> {
        match &spec.cri_log {
            Some(path) => LogWriter::open_cri(Path::new(path)),
            None => LogWriter::open(&dirs.container_dir(&spec.id), LOG_FILE_LIMIT),
        }
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
                self.emit_last(stream, &line)?;
            }
        }
        Ok(())
    }

    fn emit(&mut self, stream: Stream, line: &[u8]) -> Result<()> {
        if self.cri.is_some() {
            // A line that reached the length limit without its end goes on in the next.
            let full = line.last() == Some(&b'\n');
            return self.emit_cri(stream, line, full);
        }
        self.emit_json(stream, line)
    }

    /// The last output of a stream, without its newline: a complete line for the kubelet
    /// (containerd's way).
    fn emit_last(&mut self, stream: Stream, line: &[u8]) -> Result<()> {
        if self.cri.is_some() {
            return self.emit_cri(stream, line, true);
        }
        self.emit_json(stream, line)
    }

    fn emit_cri(&mut self, stream: Stream, line: &[u8], full: bool) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let Some((path, ino)) = self.cri.clone() else {
            return Ok(());
        };
        // Rotated by the kubelet: the path names a new file, or none yet.
        let now_at = std::fs::metadata(&path).map(|m| m.ino()).ok();
        if now_at != Some(ino) {
            self.file = append(&path)?;
            let ino = self.file.metadata().map(|m| m.ino()).unwrap_or(0);
            self.cri = Some((path.clone(), ino));
        }
        let (secs, nanos) = now();
        let text = cri_line(&rfc3339(secs, nanos), stream, full, line);
        self.file.write_all(&text).map_err(|e| log_error(&path, e))
    }

    fn emit_json(&mut self, stream: Stream, line: &[u8]) -> Result<()> {
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

fn append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| log_error(path, e))
}

/// One line of the kubelet's log format: `<time> <stream> <F|P> <text>`, where `F` ends a
/// line and `P` is a part of a longer one. The text goes without its newline.
pub fn cri_line(time: &str, stream: Stream, full: bool, text: &[u8]) -> Vec<u8> {
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    let stream = match stream {
        Stream::Stdout => "stdout",
        Stream::Stderr => "stderr",
    };
    let tag = if full { "F" } else { "P" };
    let mut out = Vec::with_capacity(time.len() + text.len() + 12);
    out.extend_from_slice(format!("{time} {stream} {tag} ").as_bytes());
    out.extend_from_slice(text);
    out.push(b'\n');
    out
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
    fn cri_lines_have_time_stream_and_tag() {
        let t = "2024-11-17T13:26:40.123456789Z";
        assert_eq!(
            cri_line(t, Stream::Stdout, true, b"hello\n"),
            b"2024-11-17T13:26:40.123456789Z stdout F hello\n"
        );
        assert_eq!(
            cri_line(t, Stream::Stderr, false, b"part"),
            b"2024-11-17T13:26:40.123456789Z stderr P part\n"
        );
        assert_eq!(
            cri_line(t, Stream::Stdout, true, b"\n"),
            format!("{t} stdout F \n").as_bytes()
        );
    }

    fn cri_texts(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| l.splitn(2, ' ').nth(1).unwrap().to_string())
            .collect()
    }

    #[test]
    fn cri_writer_splits_long_lines_and_ends_the_last() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("web/0.log");
        let mut w = LogWriter::open_cri(&path).unwrap();
        w.write(Stream::Stdout, b"a\nb").unwrap();
        w.write(Stream::Stderr, b"oops\n").unwrap();
        w.write(Stream::Stdout, &vec![b'x'; MAX_LINE + 2]).unwrap();
        w.flush_partial().unwrap();
        let x = "x".repeat(MAX_LINE - 1);
        assert_eq!(
            cri_texts(&path),
            [
                "stdout F a".to_string(),
                "stderr F oops".into(),
                format!("stdout P b{x}"),
                "stdout F xxx".into(),
            ]
        );
        assert!(!tmp.path().join("web/log.json").exists());
    }

    #[test]
    fn cri_writer_follows_the_kubelets_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("0.log");
        let mut w = LogWriter::open_cri(&path).unwrap();
        w.write(Stream::Stdout, b"one\n").unwrap();
        let rotated = tmp.path().join("0.log.20241117");
        std::fs::rename(&path, &rotated).unwrap();
        w.write(Stream::Stdout, b"two\n").unwrap();
        // The kubelet asked for a new file, which exists before the next line.
        std::fs::rename(&path, tmp.path().join("0.log.2")).unwrap();
        std::fs::write(&path, b"").unwrap();
        w.write(Stream::Stdout, b"three\n").unwrap();
        assert_eq!(cri_texts(&rotated), ["stdout F one"]);
        assert_eq!(cri_texts(&tmp.path().join("0.log.2")), ["stdout F two"]);
        assert_eq!(cri_texts(&path), ["stdout F three"]);
    }

    #[test]
    fn time_is_rfc3339_utc() {
        let t = rfc3339(1_731_850_000, 123_456_789);
        assert_eq!(t, "2024-11-17T13:26:40.123456789Z");
        assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00.000000000Z");
    }
}
