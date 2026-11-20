//! Where the Docker API is: `DOCKER_HOST` when it names a Unix socket, else podbag's own
//! socket for this platform and user. On Linux, podbag's service is started when nothing
//! listens; on macOS, the VM's standby answers on the socket; on Windows, `podbag.exe
//! system dial-stdio` carries each connection into podbag's WSL distribution.

use super::wire::Conn;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix socket. `own`: podbag's default socket, which podbag may start.
    Socket { path: PathBuf, own: bool },
    /// A program whose stdin and stdout are the connection.
    Command(Vec<OsString>),
}

/// How long a request may wait for an answer (a pull or a log stream has none).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl Endpoint {
    /// `DOCKER_HOST` when it is `unix://…`; otherwise podbag's own socket.
    pub fn find(podbag: &Path) -> Result<Endpoint, String> {
        if let Some(e) = std::env::var("DOCKER_HOST")
            .ok()
            .and_then(|v| Endpoint::from_docker_host(&v))
        {
            return Ok(e);
        }
        Endpoint::podbag_default(podbag)
    }

    /// `unix:///path` as an endpoint; other schemes (tcp, ssh, npipe) are not used.
    pub fn from_docker_host(value: &str) -> Option<Endpoint> {
        let path = value.trim().strip_prefix("unix://")?;
        (!path.is_empty()).then(|| Endpoint::Socket {
            path: PathBuf::from(path),
            own: false,
        })
    }

    /// podbag's socket for this platform and user.
    #[allow(unused_variables)]
    pub fn podbag_default(podbag: &Path) -> Result<Endpoint, String> {
        #[cfg(target_os = "macos")]
        {
            let p = crate::macos::host::paths()?;
            Ok(Endpoint::Socket {
                path: p.docker_sock,
                own: true,
            })
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let dirs = crate::paths::dirs().map_err(|e| e.to_string())?;
            Ok(Endpoint::Socket {
                path: crate::api::default_socket(&dirs),
                own: true,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Endpoint::Command(vec![
                podbag.as_os_str().to_owned(),
                "system".into(),
                "dial-stdio".into(),
            ]))
        }
    }

    /// Whether two selections use the same socket or the same transport command.
    pub fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Socket { path: a, .. }, Self::Socket { path: b, .. }) => {
                a == b
                    || std::fs::canonicalize(a)
                        .ok()
                        .zip(std::fs::canonicalize(b).ok())
                        .is_some_and(|(a, b)| a == b)
            }
            _ => self == other,
        }
    }

    /// What to show the user: the socket path, or the program.
    pub fn describe(&self) -> String {
        match self {
            Endpoint::Socket { path, .. } => format!("unix://{}", path.display()),
            Endpoint::Command(argv) => argv
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// A new connection, and what ends it from another thread. `timeout`: how long a
    /// read may wait (None: no limit).
    pub fn connect(&self, timeout: Option<Duration>) -> io::Result<(Box<dyn Conn>, Closer)> {
        match self {
            #[cfg(unix)]
            Endpoint::Socket { path, .. } => {
                let s = std::os::unix::net::UnixStream::connect(path)?;
                s.set_read_timeout(timeout)?;
                let other = s.try_clone()?;
                let closer = Closer(Arc::new(move || {
                    let _ = other.shutdown(std::net::Shutdown::Both);
                }));
                Ok((Box::new(s), closer))
            }
            #[cfg(not(unix))]
            Endpoint::Socket { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix sockets need Unix",
            )),
            Endpoint::Command(argv) => {
                let _ = timeout;
                let conn = ChildConn::spawn(argv)?;
                let child = conn.child.clone();
                let closer = Closer(Arc::new(move || {
                    let _ = child.lock().unwrap_or_else(|p| p.into_inner()).kill();
                }));
                Ok((Box::new(conn), closer))
            }
        }
    }

    /// Whether something accepts connections. No request is sent, but on macOS a
    /// connection to the standby starts the VM: ask `podbag vm status` there instead.
    pub fn listening(&self) -> bool {
        match self {
            #[cfg(unix)]
            Endpoint::Socket { path, .. } => std::os::unix::net::UnixStream::connect(path).is_ok(),
            _ => true,
        }
    }

    /// Make sure something answers. On Linux, podbag's own socket gets
    /// `podbag system service` (it ends a minute after its last client); on macOS,
    /// `podbag vm start` when `start_vm` and no standby holds the socket.
    #[allow(unused_variables)]
    pub fn ensure_running(&self, podbag: &Path, start_vm: bool) -> Result<(), String> {
        let Endpoint::Socket { path, own } = self else {
            return Ok(());
        };
        if self.listening() {
            return Ok(());
        }
        if !own {
            return Err(format!("nothing answers on {}", path.display()));
        }
        #[cfg(target_os = "macos")]
        {
            if !start_vm {
                return Err("the VM is not running".into());
            }
            let out = std::process::Command::new(podbag)
                .args(["vm", "start"])
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| format!("cannot run {}: {e}", podbag.display()))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(err.trim().trim_start_matches("podbag: ").to_string());
            }
            wait_for(self, Duration::from_secs(10))
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            start_service(podbag)?;
            wait_for(self, Duration::from_secs(10))
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }
}

#[cfg(unix)]
fn wait_for(e: &Endpoint, limit: Duration) -> Result<(), String> {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if e.listening() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("nothing answers on {}", e.describe()))
}

/// `podbag system service` in the background, in a group of its own (a Ctrl+C in this
/// terminal is not for it). Its output goes nowhere: it ends by itself when idle.
#[cfg(all(unix, not(target_os = "macos")))]
fn start_service(podbag: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut child = Command::new(podbag)
        .args(["system", "service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start {} system service: {e}", podbag.display()))?;
    // Reaped by a thread, so that it leaves no zombie while this program runs.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// The podbag program: `PODBAG_BIN`, this program if it is podbag, one next to it,
/// then `PATH` and the usual install folders (a desktop app gets a short `PATH`).
pub fn podbag_program() -> PathBuf {
    let name = if cfg!(windows) {
        "podbag.exe"
    } else {
        "podbag"
    };
    if let Some(bin) = std::env::var_os("PODBAG_BIN").filter(|b| !b.is_empty()) {
        return PathBuf::from(bin);
    }
    let exe = std::env::current_exe().ok();
    if let Some(exe) = exe
        .as_ref()
        .filter(|e| e.file_name().is_some_and(|n| n == name))
    {
        return exe.clone();
    }
    let mut dirs: Vec<PathBuf> = exe
        .iter()
        .filter_map(|e| e.parent().map(Path::to_path_buf))
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs.extend(["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"].map(PathBuf::from));
    dirs.into_iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Ends a connection from another thread: a read that waits on it returns.
#[derive(Clone)]
pub struct Closer(Arc<dyn Fn() + Send + Sync>);

impl Closer {
    pub fn close(&self) {
        (self.0)()
    }
}

impl Default for Closer {
    fn default() -> Closer {
        Closer(Arc::new(|| {}))
    }
}

impl std::fmt::Debug for Closer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Closer")
    }
}

/// A child process as a connection: writes go to its stdin, reads come from its stdout.
/// It is killed when the connection is dropped.
struct ChildConn {
    child: Arc<Mutex<std::process::Child>>,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
}

impl ChildConn {
    fn spawn(argv: &[OsString]) -> io::Result<ChildConn> {
        use std::process::{Command, Stdio};
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program"))?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console window flashes up for each request.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn()?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err(io::Error::other("the program has no stdin or stdout"));
        };
        Ok(ChildConn {
            child: Arc::new(Mutex::new(child)),
            stdin,
            stdout,
        })
    }
}

impl io::Read for ChildConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buf)
    }
}

impl io::Write for ChildConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stdin.flush()
    }
}

impl Drop for ChildConn {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_host_must_be_a_unix_socket() {
        assert_eq!(
            Endpoint::from_docker_host("unix:///run/user/1000/podbag/docker.sock"),
            Some(Endpoint::Socket {
                path: "/run/user/1000/podbag/docker.sock".into(),
                own: false
            })
        );
        assert_eq!(Endpoint::from_docker_host("tcp://127.0.0.1:2375"), None);
        assert_eq!(Endpoint::from_docker_host("unix://"), None);
        assert_eq!(Endpoint::from_docker_host(""), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_program_is_a_connection() {
        use std::io::{Read, Write};
        let e = Endpoint::Command(vec!["cat".into()]);
        let (mut c, _) = e.connect(None).unwrap();
        c.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
    }

    #[cfg(unix)]
    #[test]
    fn a_closer_ends_a_waiting_read() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // The server accepts and then sends nothing.
        let server = std::thread::spawn(move || listener.accept().map(|(s, _)| s));
        let e = Endpoint::Socket { path, own: false };
        let (mut conn, closer) = e.connect(None).unwrap();
        let _held = server.join().unwrap().unwrap();
        let reader = std::thread::spawn(move || conn.read(&mut [0u8; 16]).map_err(|e| e.kind()));
        std::thread::sleep(Duration::from_millis(50));
        closer.close();
        assert_eq!(reader.join().unwrap(), Ok(0));
    }

    #[cfg(unix)]
    #[test]
    fn a_foreign_socket_is_not_started() {
        let e = Endpoint::Socket {
            path: "/nonexistent/podbag-test.sock".into(),
            own: false,
        };
        assert!(!e.listening());
        let err = e
            .ensure_running(Path::new("/bin/false"), false)
            .unwrap_err();
        assert!(err.contains("nothing answers"), "{err}");
    }
}
