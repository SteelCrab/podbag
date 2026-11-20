//! The Docker Engine API service: `podbag system service`.
//!
//! One thread per connection, no async runtime. The service keeps nothing of its own:
//! every request is a library call on state files and locks, so the CLI and the API can be
//! used together. It ends after a quiet spell without connections, and can be started by
//! systemd socket activation, so nothing runs at rest.

pub mod build;
pub mod containers;
pub mod create;
pub mod docker;
pub mod events;
pub mod exec;
pub mod http;
pub mod images;
pub mod objects;
pub mod router;
pub mod shapes;
pub mod stream;
pub mod system;

use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use http::{HttpError, write_head};
use router::{Ctx, Reply};
use std::io::{BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A keep-alive connection that sends no request for this long is closed.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(60);

pub struct ServeOptions {
    pub socket: PathBuf,
    /// End after this long without an open connection; None: run until killed.
    pub timeout: Option<Duration>,
    /// The listening socket from systemd socket activation.
    pub activated: Option<OwnedFd>,
}

/// `<runtime>/docker.sock`: `$XDG_RUNTIME_DIR/podbag/docker.sock` rootless,
/// `/run/podbag/docker.sock` as root.
pub fn default_socket(dirs: &Dirs) -> PathBuf {
    dirs.runtime.join("docker.sock")
}

/// The socket systemd passed (`LISTEN_PID` is this process, `LISTEN_FDS` is 1: fd 3).
/// The variables are removed so that helpers do not see them. Call it before any thread.
pub fn activation_listener() -> Option<OwnedFd> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.trim().parse().ok()?;
    if pid != std::process::id() || std::env::var("LISTEN_FDS").ok()?.trim() != "1" {
        return None;
    }
    // SAFETY: called before the service starts threads; nothing else reads the environment.
    unsafe {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    // SAFETY: fd 3 is the listening socket systemd passed; we take ownership of it.
    unsafe {
        libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC);
        Some(OwnedFd::from_raw_fd(3))
    }
}

/// systemd units that start the service on the first connection to `socket`.
pub fn units(socket: &Path, exe: &Path, rootless: bool) -> String {
    let mode = if rootless { "0600" } else { "0660" };
    let (dir, ctl, target) = if rootless {
        (
            "~/.config/systemd/user",
            "systemctl --user",
            "default.target",
        )
    } else {
        ("/etc/systemd/system", "sudo systemctl", "multi-user.target")
    };
    format!(
        "# {dir}/podbag.socket\n\
         [Unit]\nDescription=podbag Docker API socket\n\n\
         [Socket]\nListenStream={socket}\nSocketMode={mode}\n\n\
         [Install]\nWantedBy=sockets.target\n\n\
         # {dir}/podbag.service\n\
         [Unit]\nDescription=podbag Docker API (exits when idle)\nRequires=podbag.socket\nAfter=podbag.socket\n\n\
         [Service]\nExecStart={exe} system service\n\
         # Containers started through the service keep running when it stops.\n\
         KillMode=process\n\n\
         [Install]\nWantedBy={target}\n\n\
         # Then:\n#   {ctl} daemon-reload && {ctl} enable --now podbag.socket\n\
         #   export DOCKER_HOST=unix://{socket}\n",
        socket = socket.display(),
        exe = exe.display(),
    )
}

/// Open connections and the time of the last activity, for the idle exit.
struct Activity(Mutex<(usize, Instant)>);

impl Activity {
    fn opened(&self) {
        let mut a = self.0.lock().unwrap_or_else(|p| p.into_inner());
        a.0 += 1;
        a.1 = Instant::now();
    }

    fn closed(&self) {
        let mut a = self.0.lock().unwrap_or_else(|p| p.into_inner());
        a.0 = a.0.saturating_sub(1);
        a.1 = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        let a = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if a.0 > 0 {
            Duration::ZERO
        } else {
            a.1.elapsed()
        }
    }
}

/// After an upgrade, how long an old service waits without a connection before it ends.
const REPLACED_GRACE: Duration = Duration::from_secs(2);

/// Serve until killed, or until `timeout` passes without an open connection. A service
/// that starts on demand (a timeout, or systemd's socket) also ends when its program
/// was replaced (`podbag upgrade`) and no connection is open: the next client starts
/// the new program.
pub fn serve(opts: ServeOptions) -> Result<()> {
    let dirs = crate::paths::dirs()?;
    let on_demand = opts.timeout.is_some() || opts.activated.is_some();
    let program = on_demand.then(crate::upgrade::ProgramWatch::new).flatten();
    let (listener, owned_path) = match opts.activated {
        Some(fd) => (UnixListener::from(fd), None),
        None => (bind(&opts.socket)?, Some(opts.socket.clone())),
    };
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::io("cannot set up the API socket", &e, "report this bug"))?;
    let ctx = Arc::new(Ctx::new(dirs));
    let activity = Arc::new(Activity(Mutex::new((0, Instant::now()))));
    // Containers this service starts must not get a Ctrl+C or hangup meant for it.
    crate::helper::helpers_in_own_group();
    crate::helper::raise_descriptor_limit();
    loop {
        if wait_readable(listener.as_raw_fd(), 250) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    activity.opened();
                    let (ctx, worker_activity) = (ctx.clone(), activity.clone());
                    let worker = std::thread::Builder::new().spawn(move || {
                        handle_connection(stream, &ctx);
                        worker_activity.closed();
                    });
                    if worker.is_err() {
                        // The connection went with the closure: the client sees it close.
                        activity.closed();
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(
                            libc::EMFILE
                                | libc::ENFILE
                                | libc::ENOBUFS
                                | libc::ENOMEM
                                | libc::ECONNABORTED
                        )
                    ) =>
                {
                    // Out of descriptors or memory for now: the connection waits in the
                    // backlog until others close.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => {
                    return Err(Error::io(
                        "cannot accept API connections",
                        &e,
                        "restart the service",
                    ));
                }
            }
        }
        if opts.timeout.is_some_and(|t| activity.idle_for() >= t) {
            break;
        }
        if activity.idle_for() >= REPLACED_GRACE && program.as_ref().is_some_and(|p| p.replaced()) {
            break;
        }
    }
    if let Some(path) = owned_path {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn wait_readable(fd: i32, millis: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut p, 1, millis) > 0 }
}

/// Listen on `path`: a leftover socket nobody answers on is replaced; a live one, or a
/// path that is not a socket, is an error.
fn bind(path: &Path) -> Result<UnixListener> {
    let fail = |what: String, e: std::io::Error| {
        Error::io(
            what,
            &e,
            "check the runtime directory, or give another socket with --socket",
        )
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| fail(format!("cannot create {}", parent.display()), e))?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cannot start the podbag API service",
                format!("{} exists and is not a socket", path.display()),
                "remove it, or give another socket with --socket",
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cannot start the podbag API service",
                format!("another service already listens on {}", path.display()),
                "use that service, stop it, or give another socket with --socket",
            ));
        }
        std::fs::remove_file(path)
            .map_err(|e| fail(format!("cannot remove the stale {}", path.display()), e))?;
    }
    let listener = UnixListener::bind(path)
        .map_err(|e| fail(format!("cannot listen on {}", path.display()), e))?;
    // SAFETY: geteuid has no preconditions.
    let mode = if unsafe { libc::geteuid() } == 0 {
        0o660
    } else {
        0o600
    };
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| fail(format!("cannot set the mode of {}", path.display()), e))?;
    Ok(listener)
}

/// Serve requests on one connection until it closes, streams or is hijacked.
pub(crate) fn handle_connection(stream: UnixStream, ctx: &Ctx) {
    let _ = stream.set_read_timeout(Some(KEEPALIVE_IDLE));
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    loop {
        let req = match http::read_request_with(&mut reader, &router::streams_body) {
            Ok(Some(r)) => r,
            Ok(None) | Err(HttpError::Io(_)) => return,
            Err(HttpError::Bad(why)) => {
                let _ = write_fixed(&mut writer, false, router::message(400, why), &[]);
                return;
            }
        };
        let head_only = req.method == "HEAD";
        let mut reply = router::route(&req, ctx);
        // A body left unread for its handler: the handler reads it (an empty one when the
        // request gives no length), or it is skipped. What follows it cannot be told from a
        // new request, so the connection then closes.
        let streamed = req.framing != http::Framing::None || matches!(reply, Reply::WithBody(_));
        if streamed {
            if req
                .header("expect")
                .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
            {
                let _ = writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            }
            let mut body = http::BodyReader::new(&mut reader, req.framing);
            if let Reply::WithBody(run) = reply {
                reply = run(&mut body);
            }
            // The rest, up to a limit, so that a client still sending reads the reply.
            let _ = std::io::copy(&mut body.take(DRAIN_LIMIT), &mut std::io::sink());
        }
        let mut headers = Vec::new();
        while let Reply::Headed(more, inner) = reply {
            headers.extend(more);
            reply = *inner;
        }
        if streamed {
            headers.push(("Connection", "close".to_string()));
        }
        match reply {
            Reply::Hijack { content_type, run } => {
                let head = router::Upgrade {
                    upgrade: req.wants_upgrade(),
                    content_type,
                };
                let leftover = reader.buffer().to_vec();
                let stream = reader.into_inner();
                let _ = stream.set_read_timeout(None);
                run(stream, leftover, head);
                return;
            }
            Reply::Chunked { content_type, body } => {
                let mut extra = vec![
                    ("Content-Type", content_type.to_string()),
                    ("Transfer-Encoding", "chunked".to_string()),
                    ("Connection", "close".to_string()),
                ];
                extra.extend(headers.into_iter().filter(|(k, _)| *k != "Connection"));
                if write_head(&mut writer, 200, &extra).is_ok() && !head_only {
                    let mut chunks = std::io::BufWriter::with_capacity(
                        64 * 1024,
                        http::ChunkedWriter::new(&mut writer),
                    );
                    if body(&mut chunks).is_ok() && chunks.flush().is_ok() {
                        if let Ok(mut inner) = chunks.into_inner() {
                            let _ = inner.finish();
                        }
                    }
                }
                return;
            }
            Reply::Stream { content_type, body } => {
                let mut extra = vec![
                    ("Content-Type", content_type.to_string()),
                    ("Connection", "close".to_string()),
                ];
                extra.extend(headers.into_iter().filter(|(k, _)| *k != "Connection"));
                if write_head(&mut writer, 200, &extra).is_ok() && !head_only {
                    let _ = body(&mut writer);
                }
                return;
            }
            reply => {
                if write_fixed(&mut writer, head_only, reply, &headers).is_err()
                    || !req.keep_alive()
                    || streamed
                {
                    return;
                }
            }
        }
    }
}

/// How much of an unread request body is skipped before the reply.
const DRAIN_LIMIT: u64 = 1024 * 1024;

/// A reply with a known length, with `headers` too.
fn write_fixed(
    w: &mut impl Write,
    head_only: bool,
    reply: Reply,
    headers: &[(&'static str, String)],
) -> std::io::Result<()> {
    let (status, content_type, body) = match reply {
        Reply::Json(s, v) => (
            s,
            Some("application/json"),
            serde_json::to_vec(&v).unwrap_or_default(),
        ),
        Reply::Text(s, t) => (s, Some("text/plain; charset=utf-8"), t.into_bytes()),
        Reply::Empty(s) => (s, None, Vec::new()),
        Reply::Stream { .. }
        | Reply::Chunked { .. }
        | Reply::Hijack { .. }
        | Reply::Headed(..)
        | Reply::WithBody(_) => unreachable!("handled by the caller"),
    };
    let mut extra: Vec<(&str, String)> = headers.to_vec();
    if let Some(ct) = content_type {
        extra.push(("Content-Type", ct.to_string()));
    }
    if status != 204 && status != 304 {
        extra.push(("Content-Length", body.len().to_string()));
    }
    write_head(w, status, &extra)?;
    if !head_only {
        w.write_all(&body)?;
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn keep_alive_serves_two_requests_on_one_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = std::sync::Arc::new(router::Ctx::for_test(tmp.path()));
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || handle_connection(server, &ctx));
        client
            .write_all(b"HEAD /_ping HTTP/1.1\r\nHost: d\r\n\r\nGET /v1.47/_ping HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        worker.join().unwrap();
        let first = out.find("HTTP/1.1 200 OK").unwrap();
        let second = out[first + 1..].find("HTTP/1.1 200 OK").unwrap() + first + 1;
        let head = &out[..second];
        assert!(head.contains("Api-Version: 1.47"), "{out}");
        assert!(!head.ends_with("OK\r\n\r\nOK"), "HEAD has no body: {out}");
        assert!(out[second..].ends_with("\r\n\r\nOK"), "{out}");
    }
}
