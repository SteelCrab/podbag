//! A client for the Docker Engine API on podbag's socket, for `podbag tui` and the
//! desktop app. One connection per request and no async runtime: a stream (logs, events,
//! a pull) holds its own connection while it is read.

mod endpoint;
#[cfg(unix)]
#[doc(hidden)]
pub mod fake;
mod wire;

pub use endpoint::{Closer, Endpoint, REQUEST_TIMEOUT, podbag_program};
pub use wire::Demux;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, BufReader, Read};

/// The most a JSON answer may take.
const MAX_JSON: usize = 64 * 1024 * 1024;

/// What a request failed with: the HTTP status (0: no answer) and the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    fn io(e: io::Error) -> ApiError {
        ApiError {
            status: 0,
            message: e.to_string(),
        }
    }

    /// The API does not have this (501, or 404 for the path itself): hide the action.
    pub fn unsupported(&self) -> bool {
        self.status == 501
            || self.status == 405
            || (self.status == 404 && !self.message.to_ascii_lowercase().contains("no such"))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

pub type Result<T> = std::result::Result<T, ApiError>;

/// The four kinds of objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Containers,
    Images,
    Volumes,
    Networks,
}

impl Kind {
    pub const ALL: [Kind; 4] = [
        Kind::Containers,
        Kind::Images,
        Kind::Volumes,
        Kind::Networks,
    ];

    /// The path segment: `containers`, `images`, …
    pub fn segment(self) -> &'static str {
        match self {
            Kind::Containers => "containers",
            Kind::Images => "images",
            Kind::Volumes => "volumes",
            Kind::Networks => "networks",
        }
    }

    /// The event type (`container`, `image`, …) of this kind.
    pub fn event_type(self) -> &'static str {
        match self {
            Kind::Containers => "container",
            Kind::Images => "image",
            Kind::Volumes => "volume",
            Kind::Networks => "network",
        }
    }
}

/// `null` as the default value (Docker sends `null` for empty lists and maps).
fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    d: D,
) -> std::result::Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Port {
    #[serde(rename = "IP")]
    pub ip: String,
    pub private_port: u16,
    pub public_port: u16,
    #[serde(rename = "Type")]
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ContainerSummary {
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    pub names: Vec<String>,
    pub image: String,
    pub command: String,
    pub created: i64,
    /// `running`, `exited`, `created`, `restarting`, `paused`, …
    pub state: String,
    /// `Up 5 minutes`, `Exited (0) 2 hours ago`, …
    pub status: String,
    #[serde(deserialize_with = "nullable")]
    pub ports: Vec<Port>,
}

impl ContainerSummary {
    /// The first name, without its slash.
    pub fn name(&self) -> &str {
        self.names
            .first()
            .map(|n| n.trim_start_matches('/'))
            .unwrap_or("")
    }

    pub fn running(&self) -> bool {
        self.state == "running" || self.state == "restarting"
    }

    /// Published ports: `8080->80/tcp, 53/udp`.
    pub fn ports_text(&self) -> String {
        let mut ports: Vec<&Port> = self.ports.iter().collect();
        ports.sort_by_key(|p| (p.private_port, p.public_port));
        let mut out: Vec<String> = Vec::new();
        for p in ports {
            let kind = if p.kind.is_empty() { "tcp" } else { &p.kind };
            let text = if p.public_port > 0 {
                format!("{}->{}/{kind}", p.public_port, p.private_port)
            } else {
                format!("{}/{kind}", p.private_port)
            };
            // IPv4 and IPv6 bindings of one port are shown once.
            if !out.contains(&text) {
                out.push(text);
            }
        }
        out.join(", ")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ImageSummary {
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    pub repo_tags: Vec<String>,
    pub created: i64,
    pub size: i64,
    pub containers: i64,
}

impl ImageSummary {
    /// The first tag, or the short id when it has none.
    pub fn name(&self) -> String {
        match self
            .repo_tags
            .iter()
            .find(|t| t.as_str() != "<none>:<none>")
        {
            Some(t) => t.clone(),
            None => short_id(&self.id).to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct VolumeSummary {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct NetworkSummary {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub created: String,
    #[serde(rename = "IPAM")]
    pub ipam: Ipam,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Ipam {
    #[serde(deserialize_with = "nullable")]
    pub config: Vec<IpamConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct IpamConfig {
    pub subnet: String,
    pub gateway: String,
}

impl NetworkSummary {
    pub fn subnet(&self) -> &str {
        self.ipam
            .config
            .first()
            .map(|c| c.subnet.as_str())
            .unwrap_or("")
    }

    /// The networks every engine has, which cannot be removed.
    pub fn builtin(&self) -> bool {
        matches!(self.name.as_str(), "bridge" | "host" | "none")
    }
}

/// One event from `GET /events`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(rename = "Action")]
    pub action: String,
    #[serde(rename = "Actor")]
    pub actor: Actor,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Actor {
    #[serde(rename = "ID")]
    pub id: String,
}

/// What `POST /…/prune` removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Pruned {
    pub count: usize,
    pub bytes: u64,
}

/// Docker API parts that podbag may lack, found by asking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Features {
    pub stats: bool,
    pub top: bool,
    pub pause: bool,
    pub rename: bool,
}

/// The first 12 characters of an id, without `sha256:`.
pub fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    &id[..id.len().min(12)]
}

/// Percent-encode a path segment or query value.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A Docker API client.
#[derive(Debug, Clone)]
pub struct Client {
    pub endpoint: Endpoint,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Client {
        Client { endpoint }
    }

    /// A request; an error status becomes an [`ApiError`] with the server's message.
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Option<std::time::Duration>,
    ) -> Result<(wire::Response, Closer)> {
        let (conn, closer) = self.endpoint.connect(timeout).map_err(|e| ApiError {
            status: 0,
            message: format!("cannot connect to {}: {e}", self.endpoint.describe()),
        })?;
        let body = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
        let resp = wire::send(conn, method, path, body.as_deref()).map_err(ApiError::io)?;
        if resp.status < 400 {
            return Ok((resp, closer));
        }
        let status = resp.status;
        let text = resp.bytes(1 << 20).unwrap_or_default();
        let message = serde_json::from_slice::<Value>(&text)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&text).trim().to_string());
        let message = if message.is_empty() {
            format!("the engine answered {status}")
        } else {
            message
        };
        Err(ApiError { status, message })
    }

    /// A request whose answer is JSON (or nothing: `null`).
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let (resp, _) = self.request(method, path, body, Some(REQUEST_TIMEOUT))?;
        let bytes = resp.bytes(MAX_JSON).map_err(ApiError::io)?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| ApiError {
            status: 0,
            message: format!("the engine sent invalid JSON: {e}"),
        })
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        serde_json::from_value(self.call("GET", path, None)?).map_err(|e| ApiError {
            status: 0,
            message: format!("unexpected answer from the engine: {e}"),
        })
    }

    pub fn ping(&self) -> Result<()> {
        let (resp, _) = self.request("GET", "/_ping", None, Some(REQUEST_TIMEOUT))?;
        drop(resp);
        Ok(())
    }

    /// `GET /version`.
    pub fn version(&self) -> Result<Value> {
        self.call("GET", "/version", None)
    }

    pub fn containers(&self) -> Result<Vec<ContainerSummary>> {
        self.get("/containers/json?all=1")
    }

    pub fn images(&self) -> Result<Vec<ImageSummary>> {
        self.get("/images/json")
    }

    pub fn volumes(&self) -> Result<Vec<VolumeSummary>> {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "PascalCase", default)]
        struct List {
            #[serde(deserialize_with = "nullable")]
            volumes: Vec<VolumeSummary>,
        }
        Ok(self.get::<List>("/volumes")?.volumes)
    }

    pub fn networks(&self) -> Result<Vec<NetworkSummary>> {
        self.get("/networks")
    }

    /// The full details of one object, as the engine gives them.
    pub fn inspect(&self, kind: Kind, id: &str) -> Result<Value> {
        let path = match kind {
            Kind::Containers | Kind::Images => {
                format!("/{}/{}/json", kind.segment(), encode(id))
            }
            Kind::Volumes | Kind::Networks => format!("/{}/{}", kind.segment(), encode(id)),
        };
        self.call("GET", &path, None)
    }

    /// `start`, `stop`, `restart`, `kill`, `pause` or `unpause` a container.
    pub fn container_action(&self, id: &str, action: &str) -> Result<()> {
        self.call(
            "POST",
            &format!("/containers/{}/{action}", encode(id)),
            None,
        )
        .map(drop)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<()> {
        self.call(
            "POST",
            &format!("/containers/{}/rename?name={}", encode(id), encode(name)),
            None,
        )
        .map(drop)
    }

    /// Remove an object. `force`: a running container, or an image in use.
    pub fn remove(&self, kind: Kind, id: &str, force: bool) -> Result<()> {
        let mut path = format!("/{}/{}", kind.segment(), encode(id));
        if force && matches!(kind, Kind::Containers | Kind::Images) {
            path.push_str("?force=1");
        }
        self.call("DELETE", &path, None).map(drop)
    }

    /// Remove what is not in use: stopped containers, images no container uses, volumes no
    /// container uses (named ones too), networks without containers.
    pub fn prune(&self, kind: Kind) -> Result<Pruned> {
        let query = match kind {
            Kind::Images => "?filters=%7B%22dangling%22%3A%5B%22false%22%5D%7D",
            Kind::Volumes => "?filters=%7B%22all%22%3A%5B%22true%22%5D%7D",
            _ => "",
        };
        let v = self.call("POST", &format!("/{}/prune{query}", kind.segment()), None)?;
        let key = match kind {
            Kind::Containers => "ContainersDeleted",
            Kind::Images => "ImagesDeleted",
            Kind::Volumes => "VolumesDeleted",
            Kind::Networks => "NetworksDeleted",
        };
        Ok(Pruned {
            count: v[key].as_array().map_or(0, Vec::len),
            bytes: v["SpaceReclaimed"].as_u64().unwrap_or(0),
        })
    }

    /// Pull an image; `progress` gets each status line. The pull's own error (sent in
    /// the stream, after a 200) is an error here too.
    pub fn pull(&self, image: &str, mut progress: impl FnMut(&str)) -> Result<()> {
        let (name, tag) = split_tag(image);
        let mut path = format!("/images/create?fromImage={}", encode(name));
        if let Some(tag) = tag {
            path.push_str(&format!("&tag={}", encode(tag)));
        }
        let (resp, _) = self.request("POST", &path, None, None)?;
        for msg in serde_json::Deserializer::from_reader(BufReader::new(resp)).into_iter::<Value>()
        {
            let msg = msg.map_err(|e| ApiError {
                status: 0,
                message: format!("the pull stopped: {e}"),
            })?;
            if let Some(err) = msg["error"]
                .as_str()
                .or(msg["errorDetail"]["message"].as_str())
            {
                return Err(ApiError {
                    status: 0,
                    message: err.to_string(),
                });
            }
            let mut line = msg["status"].as_str().unwrap_or("").to_string();
            if let Some(id) = msg["id"].as_str() {
                line = format!("{id}: {line}");
            }
            if let Some(p) = msg["progress"].as_str() {
                line = format!("{line} {p}");
            }
            if !line.is_empty() {
                progress(&line);
            }
        }
        Ok(())
    }

    /// A container's log, as lines. `follow`: the stream stays open for new lines.
    pub fn logs(&self, id: &str, tail: usize, follow: bool) -> Result<LogLines> {
        let path = format!(
            "/containers/{}/logs?stdout=1&stderr=1&tail={tail}&follow={}",
            encode(id),
            u8::from(follow)
        );
        let (resp, closer) =
            self.request("GET", &path, None, (!follow).then_some(REQUEST_TIMEOUT))?;
        let raw = resp
            .header("Content-Type")
            .is_some_and(|t| t.contains("raw-stream"));
        let reader: Box<dyn Read + Send> = if raw {
            Box::new(resp)
        } else {
            Box::new(Demux::new(resp))
        };
        Ok(LogLines(BufReader::new(reader), closer))
    }

    /// Changes as they happen (`GET /events`): the stream stays open.
    pub fn events(&self) -> Result<Events> {
        let (resp, closer) = self.request("GET", "/events", None, None)?;
        Ok(Events(
            serde_json::Deserializer::from_reader(BufReader::new(resp)).into_iter(),
            closer,
        ))
    }

    /// `GET /containers/{id}/stats` once, or `GET /containers/{id}/top`.
    pub fn container_json(&self, id: &str, what: &str) -> Result<Value> {
        let path = match what {
            "stats" => format!("/containers/{}/stats?stream=0", encode(id)),
            _ => format!("/containers/{}/{what}", encode(id)),
        };
        self.call("GET", &path, None)
    }

    /// Ask which optional parts the engine has. Read-only requests where they exist (stats
    /// and top, on `sample`, a container); otherwise a container that does not exist.
    pub fn features(&self, sample: Option<&str>) -> Features {
        let missing = "podbag-feature-check-0000";
        let has = |r: Result<Value>| !matches!(r, Err(e) if e.unsupported() || e.status == 0);
        let target = sample.unwrap_or(missing);
        Features {
            stats: has(self.container_json(target, "stats")),
            top: has(self.container_json(target, "top")),
            pause: has(self
                .container_action(missing, "pause")
                .map(|()| Value::Null)),
            rename: has(self.rename(missing, missing).map(|()| Value::Null)),
        }
    }
}

/// `alpine:3.20` → (`alpine`, `3.20`); `localhost:5000/app` has no tag; a digest stays
/// in the name.
pub fn split_tag(image: &str) -> (&str, Option<&str>) {
    if image.contains('@') {
        return (image, None);
    }
    match image.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name, Some(tag)),
        _ => (image, None),
    }
}

/// A log as lines (a last line without a newline included).
pub struct LogLines(BufReader<Box<dyn Read + Send>>, Closer);

impl LogLines {
    /// Ends the stream from another thread.
    pub fn closer(&self) -> Closer {
        self.1.clone()
    }
}

impl Iterator for LogLines {
    type Item = io::Result<String>;

    fn next(&mut self) -> Option<io::Result<String>> {
        let mut line = Vec::new();
        match self.0.read_until(b'\n', &mut line) {
            Ok(0) => None,
            Ok(_) => {
                while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    line.pop();
                }
                Some(Ok(String::from_utf8_lossy(&line).into_owned()))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

/// The events stream.
pub struct Events(
    serde_json::StreamDeserializer<
        'static,
        serde_json::de::IoRead<BufReader<wire::Response>>,
        Event,
    >,
    Closer,
);

impl Events {
    /// Ends the stream from another thread.
    pub fn closer(&self) -> Closer {
        self.1.clone()
    }
}

impl Iterator for Events {
    type Item = io::Result<Event>;

    fn next(&mut self) -> Option<io::Result<Event>> {
        match self.0.next()? {
            Ok(e) => Some(Ok(e)),
            Err(e) if e.is_eof() => None,
            Err(e) => Some(Err(io::Error::new(io::ErrorKind::InvalidData, e))),
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
