//! The streaming server behind Exec, Attach and PortForward: those calls return a URL on
//! this small HTTP server (127.0.0.1, a one-time token in the path, valid for a minute).
//! The kubelet passes the client's upgraded connection on to it, and the streams run over
//! SPDY: `v4.channel.k8s.io`/`v5.channel.k8s.io` for commands, `portforward.k8s.io` for
//! ports.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::spdy::{self, Writer};
use crate::containers::{ContainerHandle, ExecOptions};

/// How long a URL stays valid.
const TOKEN_LIFE: Duration = Duration::from_secs(60);
/// How long the client has to open the streams it asked for.
const STREAMS_WAIT: Duration = Duration::from_secs(30);
/// The protocols for commands, best first.
const COMMAND_PROTOCOLS: [&str; 2] = ["v5.channel.k8s.io", "v4.channel.k8s.io"];
const PORT_FORWARD_PROTOCOL: &str = "portforward.k8s.io";

/// What a URL is for.
#[derive(Debug, Clone)]
pub enum Request {
    Exec {
        container: String,
        cmd: Vec<String>,
        io: Io,
    },
    Attach {
        container: String,
        log: Option<String>,
        io: Io,
        /// The container's input pipe, when it takes input; closed after the first attach
        /// with `stdin_once`.
        stdin: Option<super::runtime::Stdin>,
        stdin_once: bool,
    },
    PortForward {
        sandbox: String,
        netns: Option<String>,
    },
}

/// The streams a command uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Io {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub tty: bool,
}

pub struct Streaming {
    port: u16,
    pending: Mutex<HashMap<String, (Instant, Request)>>,
}

impl Streaming {
    /// Listen on a free port of 127.0.0.1 and serve there from now on.
    pub fn start() -> crate::Result<Arc<Streaming>> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| {
            crate::Error::io(
                "cannot start the streaming server",
                &e,
                "check the loopback interface",
            )
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| crate::Error::io("cannot start the streaming server", &e, ""))?
            .port();
        let me = Arc::new(Streaming {
            port,
            pending: Mutex::new(HashMap::new()),
        });
        let server = Arc::clone(&me);
        std::thread::Builder::new()
            .name("cri-streaming".into())
            .spawn(move || {
                for conn in listener.incoming().flatten() {
                    let server = Arc::clone(&server);
                    let _ = std::thread::Builder::new()
                        .name("cri-stream".into())
                        .spawn(move || server.connection(conn));
                }
            })
            .map_err(|e| crate::Error::io("cannot start the streaming server", &e, ""))?;
        Ok(me)
    }

    /// A one-time URL for `request`.
    pub fn url(&self, request: Request) -> crate::Result<String> {
        let token = crate::id::new_id()?;
        let kind = match request {
            Request::Exec { .. } => "exec",
            Request::Attach { .. } => "attach",
            Request::PortForward { .. } => "portforward",
        };
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        pending.retain(|_, (at, _)| now.duration_since(*at) < TOKEN_LIFE);
        pending.insert(token.clone(), (now, request));
        Ok(format!("http://127.0.0.1:{}/{kind}/{token}", self.port))
    }

    fn take(&self, token: &str) -> Option<Request> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let (at, request) = pending.remove(token)?;
        (at.elapsed() < TOKEN_LIFE).then_some(request)
    }

    fn connection(&self, mut conn: TcpStream) {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
        let Some((path, headers)) = read_request(&mut conn) else {
            return;
        };
        let token = path.rsplit('/').next().unwrap_or_default();
        let token = token.split('?').next().unwrap_or_default();
        let Some(request) = self.take(token) else {
            let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            return;
        };
        let header = |name: &str| -> Vec<String> {
            headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .flat_map(|(_, v)| v.split(',').map(|p| p.trim().to_string()))
                .collect()
        };
        let spdy = header("upgrade")
            .iter()
            .any(|u| u.eq_ignore_ascii_case("SPDY/3.1"));
        if !spdy {
            let _ = conn.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 30\r\n\r\nonly SPDY/3.1 upgrades served\n",
            );
            return;
        }
        let offered = header("x-stream-protocol-version");
        let protocol = match &request {
            Request::PortForward { .. } => offered
                .iter()
                .find(|p| p.as_str() == PORT_FORWARD_PROTOCOL)
                .cloned(),
            _ => offered
                .iter()
                .find(|p| COMMAND_PROTOCOLS.contains(&p.as_str()))
                .cloned(),
        };
        let Some(protocol) = protocol else {
            let _ = conn.write_all(
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 30\r\n\r\nno stream protocol in common\n\n",
            );
            return;
        };
        let reply = format!(
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\
             X-Stream-Protocol-Version: {protocol}\r\n\r\n"
        );
        if conn.write_all(reply.as_bytes()).is_err() {
            return;
        }
        let _ = conn.set_read_timeout(None);
        let Ok(reader) = conn.try_clone() else {
            return;
        };
        let Ok(out) = conn.try_clone() else {
            return;
        };
        let writer = Arc::new(Writer::new(Box::new(out)));
        let (tx, streams) = channel();
        let session = Arc::clone(&writer);
        let read_side = std::thread::spawn(move || spdy::serve(reader, session, tx));
        match request {
            Request::Exec { container, cmd, io } => {
                exec(&writer, &streams, &container, &cmd, io);
            }
            Request::Attach {
                container,
                log,
                io,
                stdin,
                stdin_once,
            } => {
                let input = stdin.map(|s| (s, stdin_once));
                attach(&writer, &streams, &container, log.as_deref(), io, input);
            }
            Request::PortForward { sandbox, netns } => {
                port_forward(&writer, &streams, &sandbox, netns.as_deref());
            }
        }
        // Done: the client sees the streams end, then the connection.
        let _ = writer.goaway(0);
        let _ = conn.shutdown(Shutdown::Write);
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = conn.shutdown(Shutdown::Read);
        let _ = read_side.join();
    }
}

/// The request line's path and the headers, read up to the empty line.
fn read_request(conn: &mut TcpStream) -> Option<(String, Vec<(String, String)>)> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    // One byte at a time: what follows the headers belongs to the protocol.
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() > 32 * 1024 || conn.read(&mut byte).ok()? == 0 {
            return None;
        }
        buf.push(byte[0]);
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(&buf).ok()?;
    let path = req.path?.to_string();
    let headers = req
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    Some((path, headers))
}

/// The client's streams, by type, once every expected one is open.
fn wait_for_streams(
    streams: &Receiver<spdy::Stream>,
    expected: &[&str],
) -> Option<HashMap<String, spdy::Stream>> {
    let deadline = Instant::now() + STREAMS_WAIT;
    let mut got: HashMap<String, spdy::Stream> = HashMap::new();
    while !expected.iter().all(|e| got.contains_key(*e)) {
        let left = deadline.saturating_duration_since(Instant::now());
        let s = streams.recv_timeout(left).ok()?;
        let kind = s.header("streamtype").unwrap_or_default().to_string();
        got.insert(kind, s);
    }
    Some(got)
}

fn expected(io: Io) -> Vec<&'static str> {
    let mut e = vec!["error"];
    if io.stdin {
        e.push("stdin");
    }
    if io.stdout {
        e.push("stdout");
    }
    if io.stderr && !io.tty {
        e.push("stderr");
    }
    if io.tty {
        e.push("resize");
    }
    e
}

/// The status a command ends with, as Kubernetes encodes it on the error stream.
pub fn status(code: Result<i32, String>, cmd: &[String]) -> String {
    match code {
        Ok(0) => r#"{"metadata":{},"status":"Success"}"#.to_string(),
        Ok(code) => serde_json::json!({
            "metadata": {},
            "status": "Failure",
            "message": format!(
                "command terminated with non-zero exit code: error executing command {cmd:?}, exit code {code}"
            ),
            "reason": "NonZeroExitCode",
            "details": {"causes": [{"reason": "ExitCode", "message": code.to_string()}]},
        })
        .to_string(),
        Err(message) => serde_json::json!({
            "metadata": {},
            "status": "Failure",
            "message": message,
            "reason": "InternalError",
            "code": 500,
        })
        .to_string(),
    }
}

/// Terminal sizes from the resize stream: JSON objects one after the other.
pub fn sizes(bytes: &[u8]) -> (Vec<(u16, u16)>, usize) {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Size {
        width: u16,
        height: u16,
    }
    let mut out = Vec::new();
    let mut used = 0;
    let mut it = serde_json::Deserializer::from_slice(bytes).into_iter::<Size>();
    while let Some(Ok(size)) = it.next() {
        out.push((size.height, size.width));
        used = it.byte_offset();
    }
    (out, used)
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(Into::into)
}

/// Copy a stream's data into a pipe until the client ends it.
fn feed(data: Receiver<Option<Vec<u8>>>, to: OwnedFd) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut to = std::fs::File::from(to);
        while let Ok(Some(chunk)) = data.recv() {
            if to.write_all(&chunk).is_err() {
                break;
            }
        }
    })
}

/// Copy a pipe onto a stream until it ends.
fn drain(from: OwnedFd, writer: Arc<Writer>, stream: u32) -> Receiver<()> {
    let (done, finished) = channel();
    std::thread::spawn(move || {
        let mut from = std::fs::File::from(from);
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.data(stream, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = done.send(());
    });
    finished
}

fn exec(
    writer: &Arc<Writer>,
    streams: &Receiver<spdy::Stream>,
    container: &str,
    cmd: &[String],
    io: Io,
) {
    let Some(mut got) = wait_for_streams(streams, &expected(io)) else {
        return;
    };
    let error = got.remove("error").map(|s| s.id).unwrap_or(0);
    let ids: Vec<u32> = got.values().map(|s| s.id).collect();
    let code = run(writer, &mut got, container, cmd, io);
    let _ = writer.data(error, status(code, cmd).as_bytes());
    let _ = writer.close(error);
    for id in ids {
        let _ = writer.close(id);
    }
}

/// Run the command with the streams; its exit code.
fn run(
    writer: &Arc<Writer>,
    got: &mut HashMap<String, spdy::Stream>,
    container: &str,
    cmd: &[String],
    io: Io,
) -> Result<i32, String> {
    use std::process::Stdio;
    let fail = |e: std::io::Error| e.to_string();
    let mut feeders = Vec::new();
    let stdin = match got.remove("stdin") {
        Some(s) => {
            let (r, w) = pipe().map_err(fail)?;
            feeders.push(feed(s.data, w));
            Stdio::from(r)
        }
        None => Stdio::null(),
    };
    let mut outputs = Vec::new();
    let mut output = |kind: &str| -> Result<Stdio, String> {
        match got.get(kind) {
            Some(s) => {
                let (r, w) = pipe().map_err(fail)?;
                outputs.push(drain(r, Arc::clone(writer), s.id));
                Ok(Stdio::from(w))
            }
            None => Ok(Stdio::null()),
        }
    };
    let stdout = output("stdout")?;
    let stderr = output("stderr")?;
    let resize = match got.remove("resize") {
        Some(s) => {
            let (r, w) = pipe().map_err(fail)?;
            feeders.push(resizer(s.data, w));
            Some(r)
        }
        None => None,
    };
    let options = ExecOptions {
        tty: io.tty,
        interactive: io.stdin,
        ..Default::default()
    };
    let running = ContainerHandle::from_id(container.to_string())
        .exec_with(cmd, &options, stdin, stdout, stderr, resize)
        .map_err(|e| e.to_string())?;
    let code = running.wait().map_err(|e| e.to_string());
    // The output still on its way; a process left in the background may hold the pipe.
    let until = Instant::now() + Duration::from_secs(2);
    for done in outputs {
        let _ = done.recv_timeout(until.saturating_duration_since(Instant::now()));
    }
    drop(feeders);
    code
}

/// Turn resize messages into the records the exec helper reads (rows, columns: two
/// little-endian u16).
fn resizer(data: Receiver<Option<Vec<u8>>>, to: OwnedFd) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut to = std::fs::File::from(to);
        let mut pending: Vec<u8> = Vec::new();
        while let Ok(Some(chunk)) = data.recv() {
            pending.extend_from_slice(&chunk);
            let (found, used) = sizes(&pending);
            pending.drain(..used);
            for (rows, cols) in found {
                let mut rec = rows.to_le_bytes().to_vec();
                rec.extend_from_slice(&cols.to_le_bytes());
                if to.write_all(&rec).is_err() {
                    return;
                }
            }
        }
    })
}

/// Attach: the container's output from now on (read from its log), its input (through its
/// input pipe, for a container without a terminal), and its terminal size.
fn attach(
    writer: &Arc<Writer>,
    streams: &Receiver<spdy::Stream>,
    container: &str,
    log: Option<&str>,
    io: Io,
    input: Option<(super::runtime::Stdin, bool)>,
) {
    let Some(mut got) = wait_for_streams(streams, &expected(io)) else {
        return;
    };
    let error = got.remove("error");
    let handle = ContainerHandle::from_id(container.to_string());
    if let Some(resize) = got.remove("resize") {
        let handle = handle.clone();
        std::thread::spawn(move || {
            let mut pending = Vec::new();
            while let Ok(Some(chunk)) = resize.data.recv() {
                pending.extend_from_slice(&chunk);
                let (found, used) = sizes(&pending);
                pending.drain(..used);
                for (rows, cols) in found {
                    let _ = handle.resize(rows, cols);
                }
            }
        });
    }
    if let Some(stdin) = got.remove("stdin") {
        std::thread::spawn(move || {
            while let Ok(Some(chunk)) = stdin.data.recv() {
                let Some((pipe, _)) = &input else {
                    // No pipe (a terminal): the input is dropped.
                    continue;
                };
                let mut held = pipe.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(file) = held.as_mut() {
                    let _ = file.write_all(&chunk);
                }
            }
            // The client's input ended: with stdin_once, the container's ends too.
            if let Some((pipe, true)) = &input {
                pipe.lock().unwrap_or_else(|e| e.into_inner()).take();
            }
        });
    }
    let out = got.get("stdout").map(|s| s.id);
    let err = got.get("stderr").map(|s| s.id).or(out);
    let mut follower = log.map(super::logs::Follower::at_end);
    loop {
        if let Some(f) = follower.as_mut() {
            for (stream, text) in f.next_lines() {
                let to = if stream == crate::shim::logbook::Stream::Stderr && !io.tty {
                    err
                } else {
                    out
                };
                if let Some(id) = to {
                    if writer.data(id, &text).is_err() {
                        return;
                    }
                }
            }
        }
        let running = handle
            .info()
            .is_ok_and(|i| i.state.status == crate::state::Status::Running);
        // The client went away: its streams have ended.
        let gone = error.as_ref().is_some_and(|e| {
            matches!(
                e.data.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            )
        });
        if !running || gone {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some(f) = follower.as_mut() {
        for (_, text) in f.next_lines() {
            if let Some(id) = out {
                let _ = writer.data(id, &text);
            }
        }
    }
    if let Some(e) = error {
        let _ = writer.data(e.id, status(Ok(0), &[]).as_bytes());
        let _ = writer.close(e.id);
    }
    for s in got.values() {
        let _ = writer.close(s.id);
    }
}

/// Port forwarding: each pair of streams (error, data) with a request ID is one connection
/// to a port of the pod, made inside its network namespace.
fn port_forward(
    writer: &Arc<Writer>,
    streams: &Receiver<spdy::Stream>,
    sandbox: &str,
    netns: Option<&str>,
) {
    let mut errors: HashMap<String, u32> = HashMap::new();
    while let Ok(s) = streams.recv() {
        let request = s.header("requestid").unwrap_or_default().to_string();
        match s.header("streamtype") {
            Some("error") => {
                errors.insert(request, s.id);
                // The client never writes to it; its end needs no reading.
            }
            Some("data") => {
                let error = errors.remove(&request);
                let port = s.header("port").and_then(|p| p.parse::<u16>().ok());
                let writer = Arc::clone(writer);
                let sandbox = sandbox.to_string();
                let netns = netns.map(str::to_string);
                std::thread::spawn(move || {
                    let result = match port {
                        Some(port) => forward(&writer, s, port, netns.as_deref()),
                        None => Err("no valid port".to_string()),
                    };
                    if let Some(e) = error {
                        if let Err(why) = result {
                            let message = format!(
                                "error forwarding port {} to pod {sandbox}: {why}",
                                port.unwrap_or(0)
                            );
                            let _ = writer.data(e, message.as_bytes());
                        }
                        let _ = writer.close(e);
                    }
                });
            }
            _ => {
                let _ = writer.reset(s.id, 1);
            }
        }
    }
}

/// One forwarded connection, until both sides end.
fn forward(
    writer: &Arc<Writer>,
    s: spdy::Stream,
    port: u16,
    netns: Option<&str>,
) -> Result<(), String> {
    let tcp = connect(netns, port).map_err(|e| e.to_string())?;
    let mut to_pod = tcp.try_clone().map_err(|e| e.to_string())?;
    let id = s.id;
    let data = s.data;
    let up = std::thread::spawn(move || {
        while let Ok(Some(chunk)) = data.recv() {
            if to_pod.write_all(&chunk).is_err() {
                break;
            }
        }
        let _ = to_pod.shutdown(Shutdown::Write);
    });
    let mut from_pod = tcp;
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match from_pod.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if writer.data(id, &buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = writer.close(id);
    let _ = up.join();
    Ok(())
}

/// A TCP connection to `port` on the pod's loopback: made by a thread that joins the pod's
/// network namespace (a socket stays in the namespace it was made in).
fn connect(netns: Option<&str>, port: u16) -> std::io::Result<TcpStream> {
    let Some(path) = netns else {
        return TcpStream::connect(("127.0.0.1", port));
    };
    let path = path.to_string();
    std::thread::spawn(move || -> std::io::Result<TcpStream> {
        let ns = std::fs::File::open(path)?;
        // SAFETY: setns on a namespace file we opened; only this thread changes, and it ends.
        if unsafe { libc::setns(std::os::fd::AsRawFd::as_raw_fd(&ns), libc::CLONE_NEWNET) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(10),
        )
    })
    .join()
    .unwrap_or_else(|_| Err(std::io::Error::other("the connecting thread failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_kubernetes_status_objects() {
        assert_eq!(status(Ok(0), &[]), r#"{"metadata":{},"status":"Success"}"#);
        let v: serde_json::Value =
            serde_json::from_str(&status(Ok(3), &["sh".into(), "-c".into(), "exit 3".into()]))
                .unwrap();
        assert_eq!(v["status"], "Failure");
        assert_eq!(v["reason"], "NonZeroExitCode");
        assert_eq!(v["details"]["causes"][0]["reason"], "ExitCode");
        assert_eq!(v["details"]["causes"][0]["message"], "3");
        let v: serde_json::Value = serde_json::from_str(&status(Err("boom".into()), &[])).unwrap();
        assert_eq!(v["message"], "boom");
        assert_eq!(v["reason"], "InternalError");
    }

    #[test]
    fn resize_messages_may_come_split_or_together() {
        let (all, used) = sizes(br#"{"Width":80,"Height":24}{"Width":100,"Height":40}"#);
        assert_eq!(all, [(24, 80), (40, 100)]);
        assert_eq!(used, 50);
        let (part, used) = sizes(br#"{"Width":80,"Height":24}{"Wid"#);
        assert_eq!(part, [(24, 80)]);
        assert_eq!(used, 25);
    }

    #[test]
    fn the_expected_streams_follow_the_request() {
        let io = Io {
            stdin: true,
            stdout: true,
            stderr: true,
            tty: true,
        };
        assert_eq!(expected(io), ["error", "stdin", "stdout", "resize"]);
        let io = Io {
            stdout: true,
            stderr: true,
            ..Default::default()
        };
        assert_eq!(expected(io), ["error", "stdout", "stderr"]);
    }

    #[test]
    fn urls_are_one_time() {
        let s = Streaming {
            port: 1,
            pending: Mutex::new(HashMap::new()),
        };
        let url = s
            .url(Request::PortForward {
                sandbox: "p".into(),
                netns: None,
            })
            .unwrap();
        assert!(url.starts_with("http://127.0.0.1:1/portforward/"), "{url}");
        let token = url.rsplit('/').next().unwrap();
        assert!(s.take(token).is_some());
        assert!(s.take(token).is_none());
    }
}
