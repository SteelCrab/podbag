//! The client side: reach the keeper (starting it when none answers) and ask it for a
//! starter with this process's descriptors.

use super::proto::{self, Reply, Request};
use crate::helper::{HelperChild, Io, ShimMode};
use crate::paths::Dirs;
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// How long a new keeper may take to answer.
const START_LIMIT: Duration = Duration::from_secs(5);

/// The three streams as descriptors the keeper can receive.
struct Streams {
    /// stdin, stdout, stderr.
    raw: [RawFd; 3],
    /// Descriptors made here (`/dev/null`, pipe ends for the starter), closed after the
    /// request so that only the starter holds them.
    _owned: Vec<OwnedFd>,
    stdout: Option<File>,
    stderr: Option<File>,
}

impl Streams {
    fn new(io: &[Io; 3]) -> std::io::Result<Streams> {
        let mut streams = Streams {
            raw: [-1; 3],
            _owned: Vec::new(),
            stdout: None,
            stderr: None,
        };
        let null = |streams: &mut Streams| -> std::io::Result<RawFd> {
            let f: OwnedFd = File::options()
                .read(true)
                .write(true)
                .open("/dev/null")?
                .into();
            let raw = f.as_raw_fd();
            streams._owned.push(f);
            Ok(raw)
        };
        for (n, io) in io.iter().enumerate() {
            let fd = match io {
                // A closed stream of this process stays "nothing" for the container.
                // SAFETY: F_GETFD on a number has no memory effects.
                Io::Inherit if unsafe { libc::fcntl(n as RawFd, libc::F_GETFD) } != -1 => {
                    n as RawFd
                }
                Io::Inherit | Io::Null => null(&mut streams)?,
                Io::Piped if n == 0 => null(&mut streams)?,
                Io::Piped => {
                    let (r, w) = std::io::pipe()?;
                    let w: OwnedFd = w.into();
                    let raw = w.as_raw_fd();
                    streams._owned.push(w);
                    let ours = Some(File::from(OwnedFd::from(r)));
                    if n == 1 {
                        streams.stdout = ours;
                    } else {
                        streams.stderr = ours;
                    }
                    raw
                }
                Io::Fd(fd) => fd.as_raw_fd(),
            };
            if let Some(slot) = streams.raw.get_mut(n) {
                *slot = fd;
            }
        }
        Ok(streams)
    }
}

/// What asking the keeper for a start gave.
pub enum Asked {
    /// A starter runs.
    Started(HelperChild),
    /// No keeper can be used (none starts, it refuses, the directory is not private): the
    /// caller starts a shim itself.
    Unavailable,
    /// The keeper took the request and did not answer in time. Its starter may run: a
    /// shim of the caller's own could start the container a second time.
    Unanswered,
}

/// How long the keeper may take to answer a request it has in full.
const REPLY_LIMIT: Duration = Duration::from_secs(10);

/// Ask the keeper of `dirs` to start container `id` with these streams. `scope`: the
/// container wants a systemd scope of its own (a keeper without a cgroup refuses).
pub fn start(
    dirs: &Dirs,
    id: &str,
    io: &[Io; 3],
    mode: ShimMode,
    keep_on_failure: bool,
    scope: bool,
) -> Asked {
    // The keeper must be the program that this client would run as a shim.
    let Some(build) = crate::helper::locate_helper()
        .ok()
        .and_then(|program| super::build_of(&program))
    else {
        return Asked::Unavailable;
    };
    start_within(
        dirs,
        id,
        io,
        mode,
        keep_on_failure,
        scope,
        build,
        REPLY_LIMIT,
    )
}

fn start_within(
    dirs: &Dirs,
    id: &str,
    io: &[Io; 3],
    mode: ShimMode,
    keep_on_failure: bool,
    scope: bool,
    build: String,
    limit: Duration,
) -> Asked {
    // The settings and the descriptors go to whoever listens in this directory.
    if !super::private_dir(&dirs.runtime) {
        return Asked::Unavailable;
    }
    let request = Request::Start {
        id: id.to_string(),
        mode: match mode {
            ShimMode::Attached => "attached",
            ShimMode::Detached => "detached",
        }
        .to_string(),
        keep_on_failure,
        env: super::settings(),
        build,
        scope,
    };
    let Ok(line) = serde_json::to_vec(&request) else {
        return Asked::Unavailable;
    };
    if line.len() > proto::MAX_LINE {
        return Asked::Unavailable;
    }
    let (Ok(mut streams), Ok((status_r, status_w))) = (Streams::new(io), std::io::pipe()) else {
        return Asked::Unavailable;
    };
    let fds = [
        streams.raw[0],
        streams.raw[1],
        streams.raw[2],
        status_w.as_raw_fd(),
    ];
    // A keeper that leaves between the connect and its answer is replaced once or twice.
    for _ in 0..3 {
        let Some(conn) = connect(dirs) else {
            return Asked::Unavailable;
        };
        if !super::peer_is_us(&conn) {
            return Asked::Unavailable;
        }
        let _ = conn.set_read_timeout(Some(limit));
        let _ = conn.set_write_timeout(Some(limit));
        // A request that did not go out whole is not served: ask again.
        if proto::send_with_fds(&conn, &line, &fds).is_err() {
            continue;
        }
        match proto::recv_with_fds(&conn, 0) {
            Ok((answer, _)) => match serde_json::from_slice::<Reply>(&answer) {
                Ok(Reply::Started { pid }) => {
                    let _ = conn.set_read_timeout(None);
                    return Asked::Started(HelperChild::kept(
                        pid,
                        conn,
                        File::from(OwnedFd::from(status_r)),
                        streams.stdout.take(),
                        streams.stderr.take(),
                    ));
                }
                Ok(Reply::Refused { .. }) => return Asked::Unavailable,
                // Not an answer to a start: what the keeper did with it is unknown.
                _ => return Asked::Unanswered,
            },
            // The keeper has the request and is silent. Asking again, or starting a shim,
            // could run the container twice.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Asked::Unanswered;
            }
            // The keeper went away without an answer; a starter it made ends with it.
            Err(_) => continue,
        }
    }
    Asked::Unavailable
}

fn try_connect(dirs: &Dirs) -> std::io::Result<UnixStream> {
    super::with_socket_path(&dirs.runtime, super::SOCKET, |p| UnixStream::connect(p))
}

/// Connect to the keeper; start one when none answers.
fn connect(dirs: &Dirs) -> Option<UnixStream> {
    if let Ok(conn) = try_connect(dirs) {
        return Some(conn);
    }
    spawn(dirs).ok()?;
    let deadline = Instant::now() + START_LIMIT;
    loop {
        if let Ok(conn) = try_connect(dirs) {
            return Some(conn);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Start `podbag __keeper`. Rootless with a systemd user session: in a delegated scope
/// of its own, where it makes the cgroups of its containers. It reports when it listens
/// (or when another keeper has the directory).
fn spawn(dirs: &Dirs) -> crate::Result<()> {
    // SAFETY: geteuid has no preconditions.
    let rootless = unsafe { libc::geteuid() } != 0;
    let scope = rootless
        .then(|| crate::cgroup::systemd_run().filter(|_| crate::cgroup::user_manager()))
        .flatten()
        .map(|systemd_run| crate::helper::Scope {
            systemd_run,
            slice: format!(
                "{}.slice",
                crate::cgroup::parent_name().unwrap_or_else(|_| "podbag".into())
            ),
            unit: None,
            delegate: true,
        });
    let runtime = dirs.runtime.to_string_lossy();
    // Only this client's settings for podbag: the keeper lives on with what it gets.
    let mut helper =
        crate::helper::spawn_with_env(&["__keeper", &runtime], scope.as_ref(), super::settings())?;
    helper.wait_started()?;
    let _ = helper.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_request_without_an_answer_is_sent_once() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            data: dir.path().join("data"),
            runtime: dir.path().join("run"),
            config: dir.path().join("config"),
        };
        std::fs::create_dir_all(&dirs.runtime).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let private = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&dirs.runtime, private).unwrap();
        }
        // A keeper that takes requests and never answers.
        let listener = UnixListener::bind(dirs.runtime.join(crate::keeper::SOCKET)).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for conn in listener.incoming().flatten() {
                if proto::recv_with_fds(&conn, 4).is_ok() {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                held.push(conn);
            }
        });
        let io = [Io::Null, Io::Null, Io::Null];
        let asked = start_within(
            &dirs,
            &"a".repeat(64),
            &io,
            ShimMode::Detached,
            true,
            false,
            "a build".into(),
            Duration::from_millis(300),
        );
        assert!(matches!(asked, Asked::Unanswered));
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "the request was sent again"
        );
    }

    #[test]
    fn a_keeper_that_closes_without_an_answer_is_asked_again_then_left() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            data: dir.path().join("data"),
            runtime: dir.path().join("run"),
            config: dir.path().join("config"),
        };
        std::fs::create_dir_all(&dirs.runtime).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let private = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&dirs.runtime, private).unwrap();
        }
        let listener = UnixListener::bind(dirs.runtime.join(crate::keeper::SOCKET)).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                if proto::recv_with_fds(&conn, 4).is_ok() {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                // Closed without a word: this keeper made no starter.
            }
        });
        let io = [Io::Null, Io::Null, Io::Null];
        let asked = start_within(
            &dirs,
            &"a".repeat(64),
            &io,
            ShimMode::Detached,
            true,
            false,
            "a build".into(),
            Duration::from_secs(2),
        );
        assert!(matches!(asked, Asked::Unavailable));
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }
}
