//! A fake engine on a Unix socket, for tests of the client and of the programs that use
//! it (the TUI, the desktop app). Not for anything else.

use super::{Client, Endpoint};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// An answer: the status, the content type and the body.
pub type Answer = (u16, &'static str, Vec<u8>);

type Handler = dyn Fn(&str, &str) -> Answer + Send + Sync;

/// A running fake engine. It serves until it is dropped; its folder goes then.
pub struct FakeEngine {
    dir: PathBuf,
    pub client: Client,
    /// What it got: method, path and body.
    pub seen: Arc<Mutex<Vec<(String, String, String)>>>,
    /// The headers of each request, in the same order as `seen`.
    heads: Arc<Mutex<Vec<Headers>>>,
}

type Headers = Vec<(String, String)>;

impl Drop for FakeEngine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl FakeEngine {
    /// `answer(method, path)` answers each request.
    pub fn start(answer: impl Fn(&str, &str) -> Answer + Send + Sync + 'static) -> FakeEngine {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "podbag-fake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a folder for the fake engine");
        let path = dir.join("docker.sock");
        let listener = UnixListener::bind(&path).expect("the fake engine's socket");
        let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        let heads: Arc<Mutex<Vec<Headers>>> = Arc::default();
        let log = (seen.clone(), heads.clone());
        let answer: Arc<Handler> = Arc::new(answer);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let answer = answer.clone();
                let log = log.clone();
                std::thread::spawn(move || serve_one(conn, &*answer, &log.0, &log.1));
            }
        });
        FakeEngine {
            dir,
            client: Client::new(Endpoint::Socket { path, own: false }),
            seen,
            heads,
        }
    }

    /// The socket it serves on.
    pub fn socket(&self) -> PathBuf {
        self.dir.join("docker.sock")
    }

    /// Header `name` of request `index` (counted from 0), if it was sent.
    pub fn header(&self, index: usize, name: &str) -> Option<String> {
        let heads = self.heads.lock().unwrap_or_else(|p| p.into_inner());
        heads
            .get(index)?
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    /// The requests so far, as `METHOD path`.
    pub fn requests(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.iter().map(|(m, p, _)| format!("{m} {p}")).collect()
    }
}

/// A JSON answer.
pub fn json(status: u16, v: serde_json::Value) -> Answer {
    (
        status,
        "application/json",
        serde_json::to_vec(&v).unwrap_or_default(),
    )
}

/// Two containers (one running with a port, one exited), as the engine lists them.
pub fn containers_json() -> serde_json::Value {
    serde_json::json!([
        {"Id": "a1b2c3d4e5f6a7b8", "Names": ["/web"], "Image": "nginx:alpine",
         "Command": "nginx", "Created": 1700000000, "State": "running", "Status": "Up 2 minutes",
         "Ports": [{"IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"},
                   {"IP": "::", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}]},
        {"Id": "ffeeddccbbaa9988", "Names": ["/job"], "Image": "alpine", "Command": "true",
         "Created": 1700000100, "State": "exited", "Status": "Exited (0) 1 hour ago",
         "Ports": null, "Labels": null}
    ])
}

/// A small engine: two containers, two images, a volume and the networks; actions
/// succeed, and stats, top, pause and rename are not supported (as in podbag).
pub fn small_engine(method: &str, path: &str) -> Answer {
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        ("GET", "/_ping") => (200, "text/plain", b"OK".to_vec()),
        ("GET", "/containers/json") => json(200, containers_json()),
        ("GET", "/images/json") => json(
            200,
            serde_json::json!([
                {"Id": "sha256:1111111111111111", "RepoTags": ["nginx:alpine"], "Size": 52_000_000,
                 "Created": 1700000000, "Containers": 1},
                {"Id": "sha256:2222222222222222", "RepoTags": ["alpine:latest"], "Size": 8_000_000,
                 "Created": 1690000000, "Containers": 1}
            ]),
        ),
        ("GET", "/volumes") => json(
            200,
            serde_json::json!({"Volumes": [{"Name": "pgdata", "Driver": "local",
                "Mountpoint": "/var/lib/podbag/volumes/pgdata/_data",
                "CreatedAt": "2026-11-01T10:00:00Z"}], "Warnings": null}),
        ),
        ("GET", "/networks") => json(
            200,
            serde_json::json!([
                {"Name": "bridge", "Id": "b0", "Driver": "bridge", "Scope": "local",
                 "IPAM": {"Config": [{"Subnet": "10.77.0.0/16"}]}},
                {"Name": "app", "Id": "a1", "Driver": "bridge", "Scope": "local",
                 "IPAM": {"Config": [{"Subnet": "10.77.1.0/24"}]}},
                {"Name": "host", "Id": "h0", "Driver": "host", "Scope": "local", "IPAM": {"Config": []}},
                {"Name": "none", "Id": "n0", "Driver": "null", "Scope": "local", "IPAM": {"Config": null}}
            ]),
        ),
        ("GET", p) if p.ends_with("/stats") || p.ends_with("/top") => json(
            501,
            serde_json::json!({"message": "stats is not supported by podbag"}),
        ),
        (_, p) if p.ends_with("/pause") || p.ends_with("/rename") => json(
            501,
            serde_json::json!({"message": "pausing containers is not supported by podbag"}),
        ),
        ("GET", p) if p.starts_with("/containers/") && p.ends_with("/logs") => {
            let mut raw = Vec::new();
            for line in ["listening on :80\n", "GET / 200\n"] {
                raw.extend([1u8, 0, 0, 0]);
                raw.extend((line.len() as u32).to_be_bytes());
                raw.extend(line.as_bytes());
            }
            (200, "application/vnd.docker.multiplexed-stream", raw)
        }
        ("GET", p) if p.ends_with("/json") || p.starts_with("/volumes/") => json(
            200,
            serde_json::json!({"Id": "a1b2c3d4e5f6a7b8", "Name": "/web",
                "Config": {"Image": "nginx:alpine", "Tty": false}}),
        ),
        ("GET", "/events") => (200, "application/json", Vec::new()),
        ("POST", p) if p.ends_with("/prune") => json(
            200,
            serde_json::json!({"ContainersDeleted": ["ffeeddccbbaa9988"], "SpaceReclaimed": 0}),
        ),
        ("POST", "/images/create") => (
            200,
            "application/json",
            b"{\"status\":\"Pulling\"}\n{\"status\":\"Done\"}\n".to_vec(),
        ),
        ("POST" | "DELETE", _) => (204, "text/plain", Vec::new()),
        _ => json(404, serde_json::json!({"message": "page not found"})),
    }
}

fn serve_one(
    conn: UnixStream,
    answer: &Handler,
    seen: &Mutex<Vec<(String, String, String)>>,
    heads: &Mutex<Vec<Headers>>,
) {
    let Ok(read_side) = conn.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_side);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers: Headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.to_ascii_lowercase())
    };
    let body = if find("Transfer-Encoding").is_some_and(|v| v.contains("chunked")) {
        match read_chunked(&mut reader) {
            Some(b) => b,
            None => return,
        }
    } else {
        let length = find("Content-Length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        body
    };
    {
        // One lock for both, so their order stays the same.
        let mut seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        heads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(headers);
        seen.push((
            method.clone(),
            path.clone(),
            String::from_utf8_lossy(&body).into(),
        ));
    }
    let (status, content_type, body) = answer(&method, &path);
    let mut out = conn;
    // Chunked, in small pieces, as the engine sends streams.
    let _ = write!(
        out,
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    for piece in body.chunks(7) {
        let _ = write!(out, "{:x}\r\n", piece.len());
        let _ = out.write_all(piece);
        let _ = out.write_all(b"\r\n");
    }
    let _ = out.write_all(b"0\r\n\r\n");
}

/// A chunked request body; None when it is cut off.
fn read_chunked(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let size = usize::from_str_radix(line.trim().split(';').next()?, 16).ok()?;
        if size == 0 {
            // Trailers, up to the empty line.
            loop {
                let mut t = String::new();
                if reader.read_line(&mut t).ok()? == 0 || t.trim().is_empty() {
                    return Some(body);
                }
            }
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..]).ok()?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).ok()?;
    }
}
