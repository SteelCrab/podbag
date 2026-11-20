//! A small fake OCI registry for tests, and builders for test images.
//!
//! It speaks just enough of the distribution API: manifests, blobs, uploads (monolithic,
//! chunked and cross-repository mounts), search, Basic authentication and the Bearer token
//! flow, anonymous or with a user and password (or a refresh token). Each request is
//! answered on its own connection (`Connection: close`).

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const TOKEN: &str = "t0k";
/// The refresh token the token server gives at a login (`offline_token=true`).
pub const REFRESH_TOKEN: &str = "refresh-1";

pub fn sha256(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!(
        "sha256:{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// How the fake registry checks who asks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Auth {
    /// Anyone may do anything.
    #[default]
    Open,
    /// Bearer tokens that the token server gives anyone.
    AnonymousToken,
    /// HTTP Basic authentication with this user and password.
    Basic { user: String, pass: String },
    /// Bearer tokens for this user and password (or the refresh token), each good for the
    /// scopes it was asked for.
    Token { user: String, pass: String },
}

impl Auth {
    pub fn basic(user: &str, pass: &str) -> Auth {
        Auth::Basic {
            user: user.into(),
            pass: pass.into(),
        }
    }

    pub fn token(user: &str, pass: &str) -> Auth {
        Auth::Token {
            user: user.into(),
            pass: pass.into(),
        }
    }
}

/// An image to push: uncompressed tar layers and the `config` part of the image config.
pub struct TestImage {
    pub layers: Vec<Vec<u8>>,
    pub config: serde_json::Value,
}

#[derive(Default)]
struct State {
    /// (repository, tag or digest) -> (content type, body)
    manifests: HashMap<(String, String), (String, Vec<u8>)>,
    blobs: HashMap<String, Vec<u8>>,
    corrupt: HashSet<String>,
    stalled: HashSet<String>,
    padded: HashMap<String, usize>,
    requests: Vec<String>,
    auth: Auth,
    /// (repository, digest): the blobs each repository holds, for HEAD, mounts and
    /// manifests.
    held: HashSet<(String, String)>,
    /// Upload sessions: their bytes so far.
    uploads: HashMap<String, Vec<u8>>,
    next_upload: u64,
}

pub struct FakeRegistry {
    /// "127.0.0.1:<port>"
    pub host: String,
    state: Arc<Mutex<State>>,
}

impl FakeRegistry {
    pub fn start(require_token: bool) -> FakeRegistry {
        FakeRegistry::start_with(if require_token {
            Auth::AnonymousToken
        } else {
            Auth::Open
        })
    }

    pub fn start_with(auth: Auth) -> FakeRegistry {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(State {
            auth,
            ..State::default()
        }));
        let (server_state, server_host) = (state.clone(), host.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (s, h) = (server_state.clone(), server_host.clone());
                std::thread::spawn(move || serve(stream, &s, &h));
            }
        });
        FakeRegistry { host, state }
    }

    /// Push an image under `repo:tag` with gzip layers. Returns the manifest digest.
    pub fn push(&self, repo: &str, tag: &str, image: &TestImage) -> String {
        let (arch, variant) = host_arch();
        let (digest, body) = self.store_image(image, arch, variant);
        self.hold_all(repo);
        self.put_manifest(repo, tag, OCI_MANIFEST, body.clone());
        self.put_manifest(repo, &digest, OCI_MANIFEST, body);
        digest
    }

    /// Push a single-platform image built for `arch` (no index).
    pub fn push_for_arch(&self, repo: &str, tag: &str, image: &TestImage, arch: &str) -> String {
        let (digest, body) = self.store_image(image, arch, None);
        self.hold_all(repo);
        self.put_manifest(repo, tag, OCI_MANIFEST, body.clone());
        self.put_manifest(repo, &digest, OCI_MANIFEST, body);
        digest
    }

    /// Push a multi-platform index under `repo:tag`. Each entry is (architecture, variant or "", image).
    pub fn push_index(
        &self,
        repo: &str,
        tag: &str,
        platforms: &[(&str, &str, &TestImage)],
    ) -> String {
        let mut entries = Vec::new();
        for (arch, variant, image) in platforms {
            let variant = (!variant.is_empty()).then_some(*variant);
            let (digest, body) = self.store_image(image, arch, variant);
            self.put_manifest(repo, &digest, OCI_MANIFEST, body.clone());
            let mut platform = serde_json::json!({"os": "linux", "architecture": arch});
            if let Some(v) = variant {
                platform["variant"] = v.into();
            }
            entries.push(serde_json::json!({
                "mediaType": OCI_MANIFEST, "digest": digest, "size": body.len(), "platform": platform
            }));
        }
        self.hold_all(repo);
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": entries
        }))
        .unwrap();
        let digest = sha256(&index);
        self.put_manifest(repo, tag, OCI_INDEX, index.clone());
        self.put_manifest(repo, &digest, OCI_INDEX, index);
        digest
    }

    /// Serve different bytes for this blob from now on.
    pub fn corrupt(&self, digest: &str) {
        self.state
            .lock()
            .unwrap()
            .corrupt
            .insert(digest.to_string());
    }

    /// Never answer requests for this blob (the connection hangs).
    pub fn stall(&self, digest: &str) {
        self.state
            .lock()
            .unwrap()
            .stalled
            .insert(digest.to_string());
    }

    /// Send `extra` bytes more than this blob's real content.
    pub fn pad(&self, digest: &str, extra: usize) {
        self.state
            .lock()
            .unwrap()
            .padded
            .insert(digest.to_string(), extra);
    }

    /// Requests seen so far, as "<METHOD> <path without query>".
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Forget the requests seen so far.
    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }

    /// A manifest as stored: its content type and body.
    pub fn manifest(&self, repo: &str, reference: &str) -> Option<(String, Vec<u8>)> {
        self.state
            .lock()
            .unwrap()
            .manifests
            .get(&(repo.to_string(), reference.to_string()))
            .cloned()
    }

    pub fn blob(&self, digest: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().blobs.get(digest).cloned()
    }

    /// Whether `repo` holds the blob (pushed or mounted there).
    pub fn holds(&self, repo: &str, digest: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .held
            .contains(&(repo.to_string(), digest.to_string()))
    }

    /// Digests of an image's layer blobs, in order.
    pub fn layer_digests(image: &TestImage) -> Vec<String> {
        image.layers.iter().map(|l| sha256(&gzip(l))).collect()
    }

    /// Every blob stored so far is in `repo` too.
    fn hold_all(&self, repo: &str) {
        let mut st = self.state.lock().unwrap();
        let blobs: Vec<String> = st.blobs.keys().cloned().collect();
        for d in blobs {
            st.held.insert((repo.to_string(), d));
        }
    }

    fn store_image(
        &self,
        image: &TestImage,
        arch: &str,
        variant: Option<&str>,
    ) -> (String, Vec<u8>) {
        let mut layers = Vec::new();
        let mut diff_ids = Vec::new();
        for tar in &image.layers {
            let gz = gzip(tar);
            let digest = sha256(&gz);
            layers.push(serde_json::json!({"mediaType": OCI_LAYER_GZIP, "digest": digest, "size": gz.len()}));
            diff_ids.push(sha256(tar));
            self.state.lock().unwrap().blobs.insert(digest, gz);
        }
        let mut config = serde_json::json!({
            "architecture": arch, "os": "linux", "config": image.config,
            "rootfs": {"type": "layers", "diff_ids": diff_ids}
        });
        if let Some(v) = variant {
            config["variant"] = v.into();
        }
        let config = serde_json::to_vec(&config).unwrap();
        let config_digest = sha256(&config);
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2, "mediaType": OCI_MANIFEST,
            "config": {"mediaType": OCI_CONFIG, "digest": config_digest, "size": config.len()},
            "layers": layers
        }))
        .unwrap();
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert(config_digest, config);
        (sha256(&manifest), manifest)
    }

    fn put_manifest(&self, repo: &str, reference: &str, content_type: &str, body: Vec<u8>) {
        self.state.lock().unwrap().manifests.insert(
            (repo.to_string(), reference.to_string()),
            (content_type.to_string(), body),
        );
    }
}

fn host_arch() -> (&'static str, Option<&'static str>) {
    match std::env::consts::ARCH {
        "aarch64" => ("arm64", Some("v8")),
        "x86_64" => ("amd64", None),
        other => (other, None),
    }
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, Vec<String>>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.query
            .get(name)
            .and_then(|v| v.first())
            .map(String::as_str)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 && i + 3 <= bytes.len() {
            if let Ok(b) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.entry(percent_decode(k))
            .or_default()
            .push(percent_decode(v));
    }
    out
}

fn read_request(stream: &TcpStream) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let mut body = Vec::new();
    if find("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let n = usize::from_str_radix(size.trim(), 16).ok()?;
            if n == 0 {
                let mut end = String::new();
                let _ = reader.read_line(&mut end);
                break;
            }
            let mut chunk = vec![0; n];
            reader.read_exact(&mut chunk).ok()?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).ok()?;
        }
    } else if let Some(n) = find("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body = vec![0; n];
        reader.read_exact(&mut body).ok()?;
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(HttpRequest {
        method,
        path: path.to_string(),
        query: parse_query(query),
        headers,
        body,
    })
}

/// Whether a token of the fake token server allows `action` on `repo`.
fn token_grants(token: &str, repo: &str, action: &str) -> bool {
    let prefix = format!("repository:{repo}:");
    token
        .strip_prefix("tok.")
        .unwrap_or("")
        .split('|')
        .filter_map(|scope| scope.strip_prefix(prefix.as_str()))
        .any(|actions| actions.split(',').any(|a| a == action))
}

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for group in data.chunks(3) {
        let b = [
            group[0],
            *group.get(1).unwrap_or(&0),
            *group.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= group.len() {
                out.push(ALPHABET[(n >> shift & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn serve(stream: TcpStream, state: &Mutex<State>, host: &str) {
    let Some(req) = read_request(&stream) else {
        return;
    };
    let mut st = state.lock().unwrap();
    st.requests.push(format!("{} {}", req.method, req.path));

    let respond = |status: &str, headers: &[(&str, String)], body: &[u8]| {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let mut s = &stream;
        let _ = s.write_all(out.as_bytes());
        if req.method != "HEAD" {
            let _ = s.write_all(body);
        }
    };
    let json_type = || ("Content-Type", "application/json".to_string());
    let basic_ok = |user: &str, pass: &str| {
        let want = format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()));
        req.header("authorization") == Some(want.as_str())
    };

    if req.path == "/token" {
        let scopes: Vec<String> = if req.method == "POST" {
            let form = parse_query(&String::from_utf8_lossy(&req.body));
            let first = |k: &str| form.get(k).and_then(|v| v.first()).cloned();
            let ok = matches!(&st.auth, Auth::Token { .. })
                && first("grant_type").as_deref() == Some("refresh_token")
                && first("refresh_token").as_deref() == Some(REFRESH_TOKEN);
            if !ok {
                return respond("401 Unauthorized", &[], b"");
            }
            first("scope")
                .map(|s| s.split(' ').map(String::from).collect())
                .unwrap_or_default()
        } else {
            if let Auth::Token { user, pass } = &st.auth {
                if !basic_ok(user, pass) || req.param("account") != Some(user.as_str()) {
                    return respond("401 Unauthorized", &[], b"");
                }
            }
            req.query.get("scope").cloned().unwrap_or_default()
        };
        let token = match &st.auth {
            Auth::Token { .. } => format!("tok.{}", scopes.join("|")),
            _ => TOKEN.to_string(),
        };
        let mut reply = serde_json::json!({ "token": token });
        if req.param("offline_token") == Some("true") && matches!(&st.auth, Auth::Token { .. }) {
            reply["refresh_token"] = REFRESH_TOKEN.into();
        }
        return respond("200 OK", &[json_type()], reply.to_string().as_bytes());
    }
    if req.path == "/v1/search" {
        let q = req.param("q").unwrap_or("").to_string();
        let n: usize = req.param("n").and_then(|n| n.parse().ok()).unwrap_or(25);
        let results: Vec<serde_json::Value> = (0..3)
            .map(|i| {
                serde_json::json!({
                    "name": format!("{q}-{i}"), "description": format!("image {i}"),
                    "star_count": 10 - i, "is_official": i == 0, "is_automated": false
                })
            })
            .take(n)
            .collect();
        let reply = serde_json::json!({
            "num_results": results.len(), "query": q, "results": results
        });
        return respond("200 OK", &[json_type()], reply.to_string().as_bytes());
    }
    let Some(rest) = req.path.strip_prefix("/v2/") else {
        return respond("404 Not Found", &[], b"");
    };
    // (repository, kind, reference); None for `/v2/` itself.
    let route: Option<(String, String, String)> = if rest.is_empty() {
        None
    } else if let Some(repo) = rest.strip_suffix("/blobs/uploads/") {
        Some((repo.to_string(), "uploads".into(), String::new()))
    } else {
        let found = ["/manifests/", "/blobs/uploads/", "/blobs/"]
            .iter()
            .find_map(|k| rest.find(k).map(|i| (i, *k)));
        match found {
            Some((i, k)) => Some((
                rest[..i].to_string(),
                match k {
                    "/manifests/" => "manifests",
                    "/blobs/uploads/" => "uploads",
                    _ => "blobs",
                }
                .to_string(),
                rest[i + k.len()..].to_string(),
            )),
            None => return respond("404 Not Found", &[], b""),
        }
    };

    // Who may do this.
    let writes = matches!(req.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
    let authorized = match &st.auth {
        Auth::Open => true,
        Auth::AnonymousToken => req
            .header("authorization")
            .is_some_and(|h| h == format!("Bearer {TOKEN}")),
        Auth::Basic { user, pass } => basic_ok(user, pass),
        Auth::Token { .. } => {
            let token = req
                .header("authorization")
                .and_then(|h| h.strip_prefix("Bearer "))
                .unwrap_or("");
            match &route {
                None => token.starts_with("tok."),
                Some((repo, _, _)) => {
                    let action = if writes { "push" } else { "pull" };
                    let mount_ok = match req.param("from") {
                        Some(from) => token_grants(token, from, "pull"),
                        None => true,
                    };
                    token_grants(token, repo, action) && mount_ok
                }
            }
        }
    };
    if !authorized {
        let challenge = match (&st.auth, &route) {
            (Auth::Basic { .. }, _) => r#"Basic realm="fake""#.to_string(),
            (_, None) => format!(r#"Bearer realm="http://{host}/token",service="fake""#),
            (_, Some((repo, _, _))) => {
                let actions = if writes { "pull,push" } else { "pull" };
                format!(
                    r#"Bearer realm="http://{host}/token",service="fake",scope="repository:{repo}:{actions}""#
                )
            }
        };
        let body = br#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}"#;
        return respond(
            "401 Unauthorized",
            &[("WWW-Authenticate", challenge), json_type()],
            body,
        );
    }
    let Some((repo, kind, reference)) = route else {
        return respond("200 OK", &[json_type()], b"{}");
    };

    match (req.method.as_str(), kind.as_str()) {
        ("GET" | "HEAD", "manifests") => {
            match st.manifests.get(&(repo.clone(), reference.clone())) {
                Some((ct, body)) => respond(
                    "200 OK",
                    &[
                        ("Content-Type", ct.clone()),
                        ("Docker-Content-Digest", sha256(body)),
                    ],
                    body,
                ),
                None => respond("404 Not Found", &[], b""),
            }
        }
        ("PUT", "manifests") => {
            let ct = req.header("content-type").unwrap_or("").to_string();
            let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&req.body) else {
                return respond("400 Bad Request", &[], b"bad manifest");
            };
            // Every blob the manifest names must be in the repository.
            let mut named: Vec<String> = parsed["layers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l["digest"].as_str().map(String::from))
                .collect();
            named.extend(parsed["config"]["digest"].as_str().map(String::from));
            if let Some(missing) = named
                .iter()
                .find(|d| !st.held.contains(&(repo.clone(), (*d).clone())))
            {
                let body = format!(
                    r#"{{"errors":[{{"code":"MANIFEST_BLOB_UNKNOWN","message":"blob unknown to registry: {missing}"}}]}}"#
                );
                return respond("400 Bad Request", &[json_type()], body.as_bytes());
            }
            let digest = sha256(&req.body);
            st.manifests.insert(
                (repo.clone(), reference.clone()),
                (ct.clone(), req.body.clone()),
            );
            st.manifests
                .insert((repo.clone(), digest.clone()), (ct, req.body.clone()));
            respond(
                "201 Created",
                &[
                    ("Docker-Content-Digest", digest.clone()),
                    ("Location", format!("/v2/{repo}/manifests/{digest}")),
                ],
                b"",
            )
        }
        ("HEAD", "blobs") => {
            if st.held.contains(&(repo.clone(), reference.clone())) {
                let len = st.blobs.get(&reference).map_or(0, Vec::len);
                let mut s = &stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nDocker-Content-Digest: {reference}\r\nConnection: close\r\n\r\n"
                );
            } else {
                respond("404 Not Found", &[], b"")
            }
        }
        ("GET", "blobs") => {
            if st.stalled.contains(&reference) {
                drop(st);
                std::thread::sleep(std::time::Duration::from_secs(600));
                return;
            }
            if let (Some(extra), Some(body)) = (st.padded.get(&reference), st.blobs.get(&reference))
            {
                let mut long = body.clone();
                long.extend(std::iter::repeat_n(0u8, *extra));
                return respond("200 OK", &[], &long);
            }
            match st.blobs.get(&reference) {
                Some(body) if st.corrupt.contains(&reference) => {
                    let mut bad = body.clone();
                    let last = bad.len() - 1;
                    bad[last] ^= 0xff;
                    respond("200 OK", &[], &bad)
                }
                Some(body) => respond("200 OK", &[], body),
                None => respond("404 Not Found", &[], b""),
            }
        }
        ("POST", "uploads") => {
            if let (Some(d), Some(from)) = (req.param("mount"), req.param("from")) {
                let (d, from) = (d.to_string(), from.to_string());
                if st.held.contains(&(from, d.clone())) {
                    st.held.insert((repo.clone(), d.clone()));
                    return respond(
                        "201 Created",
                        &[
                            ("Docker-Content-Digest", d.clone()),
                            ("Location", format!("/v2/{repo}/blobs/{d}")),
                        ],
                        b"",
                    );
                }
            }
            st.next_upload += 1;
            let id = format!("upload-{}", st.next_upload);
            st.uploads.insert(id.clone(), Vec::new());
            respond(
                "202 Accepted",
                &[
                    (
                        "Location",
                        format!("/v2/{repo}/blobs/uploads/{id}?_state=x"),
                    ),
                    ("Docker-Upload-UUID", id),
                    ("Range", "0-0".into()),
                ],
                b"",
            )
        }
        ("PATCH", "uploads") => match st.uploads.get_mut(&reference) {
            Some(data) => {
                data.extend_from_slice(&req.body);
                let end = data.len().saturating_sub(1);
                respond(
                    "202 Accepted",
                    &[
                        ("Location", format!("/v2/{repo}/blobs/uploads/{reference}")),
                        ("Range", format!("0-{end}")),
                    ],
                    b"",
                )
            }
            None => respond("404 Not Found", &[], b""),
        },
        ("PUT", "uploads") => {
            let Some(mut data) = st.uploads.remove(&reference) else {
                return respond("404 Not Found", &[], b"");
            };
            data.extend_from_slice(&req.body);
            let Some(digest) = req.param("digest").map(String::from) else {
                return respond("400 Bad Request", &[], b"no digest");
            };
            if sha256(&data) != digest {
                let body = r#"{"errors":[{"code":"DIGEST_INVALID","message":"provided digest did not match uploaded content"}]}"#;
                return respond("400 Bad Request", &[json_type()], body.as_bytes());
            }
            st.blobs.insert(digest.clone(), data);
            st.held.insert((repo.clone(), digest.clone()));
            respond(
                "201 Created",
                &[
                    ("Docker-Content-Digest", digest.clone()),
                    ("Location", format!("/v2/{repo}/blobs/{digest}")),
                ],
                b"",
            )
        }
        ("DELETE", "uploads") => {
            st.uploads.remove(&reference);
            respond("204 No Content", &[], b"")
        }
        _ => respond("405 Method Not Allowed", &[], b""),
    }
}

/// One entry of a test layer.
pub enum TarEntry {
    File {
        path: String,
        data: Vec<u8>,
        mode: u32,
        uid: u64,
        gid: u64,
    },
    Dir {
        path: String,
        mode: u32,
        uid: u64,
        gid: u64,
    },
    Symlink {
        path: String,
        target: String,
    },
    Hardlink {
        path: String,
        target: String,
    },
    Whiteout(String),
    Opaque(String),
    Fifo {
        path: String,
        mode: u32,
    },
    /// A character device.
    Device {
        path: String,
        major: u32,
        minor: u32,
        mode: u32,
    },
    /// A root-owned file with extended attributes (PAX `SCHILY.xattr.*` records).
    WithXattrs {
        path: String,
        data: Vec<u8>,
        mode: u32,
        xattrs: Vec<(String, Vec<u8>)>,
    },
    /// A root-owned 0644 file with a modification time (the other entries have 0).
    Timed {
        path: String,
        data: Vec<u8>,
        mtime: u64,
    },
}

impl TarEntry {
    pub fn file(path: &str, data: &[u8]) -> TarEntry {
        TarEntry::File {
            path: path.into(),
            data: data.to_vec(),
            mode: 0o644,
            uid: 0,
            gid: 0,
        }
    }
}

/// Build an uncompressed tar layer.
pub fn layer(entries: &[TarEntry]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        match e {
            TarEntry::File {
                path,
                data,
                mode,
                uid,
                gid,
            } => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(*mode);
                h.set_uid(*uid);
                h.set_gid(*gid);
                h.set_size(data.len() as u64);
                b.append_data(&mut h, path, data.as_slice()).unwrap();
            }
            TarEntry::Dir {
                path,
                mode,
                uid,
                gid,
            } => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(*mode);
                h.set_uid(*uid);
                h.set_gid(*gid);
                h.set_size(0);
                b.append_data(&mut h, path, std::io::empty()).unwrap();
            }
            TarEntry::Symlink { path, target } => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_mode(0o777);
                h.set_size(0);
                b.append_link(&mut h, path, target).unwrap();
            }
            TarEntry::Hardlink { path, target } => {
                h.set_entry_type(tar::EntryType::Link);
                h.set_mode(0o644);
                h.set_size(0);
                b.append_link(&mut h, path, target).unwrap();
            }
            TarEntry::Whiteout(path) => {
                let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
                let wh = if dir.is_empty() {
                    format!(".wh.{name}")
                } else {
                    format!("{dir}/.wh.{name}")
                };
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                h.set_size(0);
                b.append_data(&mut h, wh, std::io::empty()).unwrap();
            }
            TarEntry::Fifo { path, mode } => {
                h.set_entry_type(tar::EntryType::Fifo);
                h.set_mode(*mode);
                h.set_size(0);
                b.append_data(&mut h, path, std::io::empty()).unwrap();
            }
            TarEntry::Device {
                path,
                major,
                minor,
                mode,
            } => {
                h.set_entry_type(tar::EntryType::Char);
                h.set_mode(*mode);
                h.set_size(0);
                h.set_device_major(*major).unwrap();
                h.set_device_minor(*minor).unwrap();
                b.append_data(&mut h, path, std::io::empty()).unwrap();
            }
            TarEntry::WithXattrs {
                path,
                data,
                mode,
                xattrs,
            } => {
                let keys: Vec<String> = xattrs
                    .iter()
                    .map(|(k, _)| format!("SCHILY.xattr.{k}"))
                    .collect();
                b.append_pax_extensions(
                    keys.iter()
                        .map(String::as_str)
                        .zip(xattrs.iter().map(|(_, v)| v.as_slice())),
                )
                .unwrap();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(*mode);
                h.set_size(data.len() as u64);
                b.append_data(&mut h, path, data.as_slice()).unwrap();
            }
            TarEntry::Opaque(dir) => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                h.set_size(0);
                b.append_data(&mut h, format!("{dir}/.wh..wh..opq"), std::io::empty())
                    .unwrap();
            }
            TarEntry::Timed { path, data, mtime } => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                h.set_mtime(*mtime);
                h.set_size(data.len() as u64);
                b.append_data(&mut h, path, data.as_slice()).unwrap();
            }
        }
    }
    b.into_inner().unwrap()
}

/// The Alpine test root filesystem as one layer, owned by root. Built once.
pub fn alpine_layer() -> Vec<u8> {
    static LAYER: OnceLock<Vec<u8>> = OnceLock::new();
    LAYER
        .get_or_init(|| {
            let root = std::path::PathBuf::from(super::rootfs());
            let mut b = tar::Builder::new(Vec::new());
            add_tree(&mut b, &root, &root);
            b.into_inner().unwrap()
        })
        .clone()
}

fn add_tree(b: &mut tar::Builder<Vec<u8>>, root: &std::path::Path, dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    for path in names {
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(meta.permissions().mode() & 0o7777);
        if meta.file_type().is_symlink() {
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, &rel, std::fs::read_link(&path).unwrap())
                .unwrap();
        } else if meta.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            b.append_data(&mut h, &rel, std::io::empty()).unwrap();
            add_tree(b, root, &path);
        } else if meta.is_file() {
            let data = std::fs::read(&path).unwrap();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(data.len() as u64);
            b.append_data(&mut h, &rel, data.as_slice()).unwrap();
        }
    }
}
