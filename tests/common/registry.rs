//! A small fake OCI registry for tests, and builders for test images.
//!
//! It speaks just enough of the distribution API: manifests, blobs, and the anonymous
//! Bearer token flow. Each request is answered on its own connection (`Connection: close`).

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const TOKEN: &str = "t0k";

pub fn sha256(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!(
        "sha256:{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
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
    require_token: bool,
}

pub struct FakeRegistry {
    /// "127.0.0.1:<port>"
    pub host: String,
    state: Arc<Mutex<State>>,
}

impl FakeRegistry {
    pub fn start(require_token: bool) -> FakeRegistry {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(State {
            require_token,
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
        self.put_manifest(repo, tag, OCI_MANIFEST, body.clone());
        self.put_manifest(repo, &digest, OCI_MANIFEST, body);
        digest
    }

    /// Push a single-platform image built for `arch` (no index).
    pub fn push_for_arch(&self, repo: &str, tag: &str, image: &TestImage, arch: &str) -> String {
        let (digest, body) = self.store_image(image, arch, None);
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

    /// Requests seen so far, as "GET <path without query>".
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Digests of an image's layer blobs, in order.
    pub fn layer_digests(image: &TestImage) -> Vec<String> {
        image.layers.iter().map(|l| sha256(&gzip(l))).collect()
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

fn serve(stream: TcpStream, state: &Mutex<State>, host: &str) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut authorized = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if line.to_ascii_lowercase().starts_with("authorization:")
            && line.contains(&format!("Bearer {TOKEN}"))
        {
            authorized = true;
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let path_only = path.split('?').next().unwrap_or("").to_string();
    let mut st = state.lock().unwrap();
    st.requests.push(format!("GET {path_only}"));

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
        let _ = s.write_all(body);
    };

    if path_only == "/token" {
        return respond(
            "200 OK",
            &[("Content-Type", "application/json".into())],
            br#"{"token":"t0k"}"#,
        );
    }
    let Some(rest) = path_only.strip_prefix("/v2/") else {
        return respond("404 Not Found", &[], b"");
    };
    let (repo, kind, reference) = match (rest.find("/manifests/"), rest.find("/blobs/")) {
        (Some(i), _) => (&rest[..i], "manifests", &rest[i + "/manifests/".len()..]),
        (None, Some(i)) => (&rest[..i], "blobs", &rest[i + "/blobs/".len()..]),
        _ => return respond("404 Not Found", &[], b""),
    };
    if st.require_token && !authorized {
        let challenge = format!(
            r#"Bearer realm="http://{host}/token",service="fake",scope="repository:{repo}:pull""#
        );
        return respond("401 Unauthorized", &[("WWW-Authenticate", challenge)], b"");
    }
    if kind == "manifests" {
        match st.manifests.get(&(repo.to_string(), reference.to_string())) {
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
    } else {
        if st.stalled.contains(reference) {
            drop(st);
            std::thread::sleep(std::time::Duration::from_secs(600));
            return;
        }
        if let (Some(extra), Some(body)) = (st.padded.get(reference), st.blobs.get(reference)) {
            let mut long = body.clone();
            long.extend(std::iter::repeat_n(0u8, *extra));
            return respond("200 OK", &[], &long);
        }
        match st.blobs.get(reference) {
            Some(body) if st.corrupt.contains(reference) => {
                let mut bad = body.clone();
                let last = bad.len() - 1;
                bad[last] ^= 0xff;
                respond("200 OK", &[], &bad)
            }
            Some(body) => respond("200 OK", &[], body),
            None => respond("404 Not Found", &[], b""),
        }
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
            TarEntry::Opaque(dir) => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                h.set_size(0);
                b.append_data(&mut h, format!("{dir}/.wh..wh..opq"), std::io::empty())
                    .unwrap();
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
