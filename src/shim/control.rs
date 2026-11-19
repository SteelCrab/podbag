//! The shim's control socket: one JSON request line, one JSON response line, per connection.

use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Send a signal to the container's main process.
    Kill { signal: i32 },
    /// Answer when the container ends.
    Wait,
    /// Set the size of the container's terminal (no terminal: nothing to do).
    Resize { rows: u16, cols: u16 },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum Response {
    Exited { exit_code: i32 },
    Ok { ok: bool },
    Failed { error: Error },
}

const SOCKET: &str = "shim.sock";

/// A socket address holds at most 107 bytes (`sun_path`).
const SUN_PATH_MAX: usize = 107;

/// `<runtime>/containers/<id>`: the directory that holds the control socket.
fn socket_dir(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.runtime.join("containers").join(id)
}

/// Call `f` with a path to the socket in `dir` that fits in a socket address. A long
/// runtime directory (`/run/user/1000/podbag/containers/<64 hex>/shim.sock` is 108 bytes)
/// is reached through an open descriptor: `/proc/self/fd/<n>/shim.sock`.
fn with_socket_path<T>(
    dir: &Path,
    f: impl FnOnce(&Path) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let plain = dir.join(SOCKET);
    if plain.as_os_str().len() <= SUN_PATH_MAX {
        return f(&plain);
    }
    let handle = std::fs::File::open(dir)?;
    let short = PathBuf::from(format!("/proc/self/fd/{}/{SOCKET}", handle.as_raw_fd()));
    f(&short)
}

/// Create the control socket of container `id` (shim side). Its directory is private.
pub fn bind(dirs: &Dirs, id: &str) -> Result<UnixListener> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = socket_dir(dirs, id);
    let fail = |e: std::io::Error| {
        Error::io(
            "cannot create the container's control socket",
            &e,
            format!("check that {} is writable", dirs.runtime.display()),
        )
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(fail)?;
    let _ = std::fs::remove_file(dir.join(SOCKET));
    let listener = with_socket_path(&dir, |p| UnixListener::bind(p)).map_err(fail)?;
    listener.set_nonblocking(true).map_err(fail)?;
    Ok(listener)
}

/// Delete the control socket and its directory (shim side).
pub fn remove(dirs: &Dirs, id: &str) {
    let dir = socket_dir(dirs, id);
    let _ = std::fs::remove_file(dir.join(SOCKET));
    let _ = std::fs::remove_dir(&dir);
}

/// Send one request to the shim of container `id`. `timeout` bounds the wait for the answer.
pub fn send(
    dirs: &Dirs,
    id: &str,
    request: &Request,
    timeout: Option<Duration>,
) -> Result<Response> {
    let mut stream = with_socket_path(&socket_dir(dirs, id), |p| UnixStream::connect(p))
        .map_err(|_| not_running(id))?;
    stream
        .set_read_timeout(timeout)
        .map_err(|e| Error::io("cannot talk to the container", &e, "report this bug"))?;
    let mut line = serde_json::to_string(request).unwrap_or_default();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|_| not_running(id))?;
    let mut answer = String::new();
    match BufReader::new(&stream).read_line(&mut answer) {
        Ok(0) => Err(not_running(id)),
        Ok(_) => serde_json::from_str(answer.trim_end()).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                "the container's shim sent an unreadable answer",
                e.to_string(),
                "report this bug",
            )
        }),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Err(Error::new(
                ErrorKind::Runtime,
                format!(
                    "container {} did not answer in time",
                    crate::id::short_id(id)
                ),
                "the request timed out",
                "try again",
            ))
        }
        Err(_) => Err(not_running(id)),
    }
}

pub fn not_running(id: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!("container {} is not running", crate::id::short_id(id)),
        "its process has ended",
        "start a new container, or run `podbag ps -a` to see its state",
    )
}

/// The longest request line a shim or the keeper takes in.
pub const MAX_REQUEST: usize = 4096;

/// Read one request from a freshly accepted connection (shim side): at most
/// [`MAX_REQUEST`] bytes, and no longer than a second without data.
pub fn read_request(stream: &UnixStream) -> Option<Request> {
    read_request_within(stream, Duration::from_secs(1))
}

/// [`read_request`] with another limit on the time without data.
pub fn read_request_within(stream: &UnixStream, limit: Duration) -> Option<Request> {
    use std::io::Read;
    stream.set_read_timeout(Some(limit)).ok()?;
    let mut line = Vec::new();
    BufReader::new(stream.take(MAX_REQUEST as u64))
        .read_until(b'\n', &mut line)
        .ok()?;
    parse_request(&line)
}

/// The request in the bytes of one line (with or without its line end).
pub fn parse_request(line: &[u8]) -> Option<Request> {
    serde_json::from_slice(line.trim_ascii_end()).ok()
}

/// Write one response (shim side). Errors are ignored: the client may have gone.
pub fn reply(mut stream: &UnixStream, response: &Response) {
    if let Ok(mut line) = serde_json::to_string(response) {
        line.push('\n');
        let _ = stream.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_line_has_a_limit() {
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(b"{\"op\":\"wait\"}\n").unwrap();
        assert_eq!(read_request(&b), Some(Request::Wait));
        // A line that never ends is given up after MAX_REQUEST bytes, not read to its end.
        let (a, b) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            let chunk = [b'x'; 1024];
            let mut sent = 0usize;
            while sent < 256 * 1024 && (&a).write_all(&chunk).is_ok() {
                sent += chunk.len();
            }
            sent
        });
        assert_eq!(read_request(&b), None);
        drop(b);
        assert!(writer.join().unwrap() < 256 * 1024, "all of it was read");
        assert_eq!(
            parse_request(b"{\"op\":\"kill\",\"signal\":9}"),
            Some(Request::Kill { signal: 9 })
        );
        assert_eq!(parse_request(b"{\"op\":\"ki"), None);
    }

    #[test]
    fn wire_format() {
        assert_eq!(
            serde_json::to_string(&Request::Kill { signal: 15 }).unwrap(),
            r#"{"op":"kill","signal":15}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Wait).unwrap(),
            r#"{"op":"wait"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Resize {
                rows: 30,
                cols: 100
            })
            .unwrap(),
            r#"{"op":"resize","rows":30,"cols":100}"#
        );
        assert_eq!(
            serde_json::from_str::<Response>(r#"{"exit_code":3}"#).unwrap(),
            Response::Exited { exit_code: 3 }
        );
        assert_eq!(
            serde_json::from_str::<Response>(r#"{"ok":true}"#).unwrap(),
            Response::Ok { ok: true }
        );
    }
}
