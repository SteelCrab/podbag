//! Paths to handlers, and errors to HTTP status codes.

use super::http::Request;
use crate::paths::Dirs;
use crate::{Error, ErrorKind};
use std::os::unix::net::UnixStream;

/// The API version podbag speaks. Clients negotiate down to it.
pub const API_VERSION: &str = "1.47";
/// The oldest version a client may ask for.
pub const MIN_API_VERSION: &str = "1.24";

/// What a handler answers.
pub enum Reply {
    Json(u16, serde_json::Value),
    /// No body (204, 304, or 201 without one).
    Empty(u16),
    Text(u16, String),
    /// A body written until the handler returns; the connection closes afterwards. The
    /// handler gets the socket to see whether the client is still there
    /// ([`super::http::client_gone`]).
    Stream {
        content_type: &'static str,
        body: Box<dyn FnOnce(&mut UnixStream) -> std::io::Result<()> + Send>,
    },
    /// The connection becomes a raw stream (attach, exec): `run` gets the socket, any
    /// bytes the client already sent after the request, and the response head, which it
    /// must send before anything else. It may act first: an attachment registers itself
    /// before the client learns it is attached, so a start sent after that finds it.
    Hijack {
        content_type: &'static str,
        run: Box<dyn FnOnce(UnixStream, Vec<u8>, Upgrade) + Send>,
    },
    /// A body of unknown length in chunks: it ends with the last (empty) chunk only when
    /// `body` succeeds, so a client sees an error instead of a cut-off body that looks
    /// whole. The connection closes afterwards.
    Chunked {
        content_type: &'static str,
        body: Box<dyn FnOnce(&mut dyn std::io::Write) -> std::io::Result<()> + Send>,
    },
    /// Another reply with more headers.
    Headed(Vec<(&'static str, String)>, Box<Reply>),
    /// The handler reads the request body as it arrives ([`streams_body`] left it unread)
    /// and then gives the reply. The connection closes afterwards.
    WithBody(Box<dyn FnOnce(&mut dyn std::io::Read) -> Reply + Send>),
}

/// The response head of a hijacked connection.
pub struct Upgrade {
    /// The client asked to upgrade the connection (101); otherwise 200.
    pub upgrade: bool,
    pub content_type: &'static str,
}

impl Upgrade {
    pub fn send(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        self.send_protocol(w, "tcp")
    }

    pub(super) fn send_protocol(
        &self,
        w: &mut impl std::io::Write,
        protocol: &str,
    ) -> std::io::Result<()> {
        let mut extra = vec![("Content-Type", self.content_type.to_string())];
        if self.upgrade {
            extra.push(("Connection", "Upgrade".to_string()));
            extra.push(("Upgrade", protocol.to_owned()));
        }
        super::http::write_head(w, if self.upgrade { 101 } else { 200 }, &extra)
    }
}

/// What every handler can reach.
pub struct Ctx {
    pub dirs: Dirs,
    volume_lesson: bool,
    #[cfg(target_os = "linux")]
    pub(super) buildkit: Option<super::buildkit::Client>,
    #[cfg(target_os = "linux")]
    pub(super) http_builds: std::sync::Arc<super::HttpBuilds>,
    /// Clients attached to containers that wait for a start, and how starts ended.
    pub attachments: std::sync::Arc<super::stream::Attachments>,
    /// Commands made by `POST /containers/{id}/exec`.
    pub execs: super::exec::Execs,
    /// Host paths a bind mount may come from (the macOS VM's shared folders).
    /// None allows any path; an empty list allows no host paths.
    pub binds_under: Option<Vec<std::path::PathBuf>>,
    /// Only the guest API opened for a Desktop Kind operation allows its kernel mounts.
    pub(crate) kind_node_binds: bool,
}

impl Ctx {
    pub fn new(dirs: Dirs) -> Ctx {
        Ctx {
            dirs,
            volume_lesson: std::env::var_os(crate::volume_lesson::SCOPE).is_some(),
            #[cfg(target_os = "linux")]
            buildkit: None,
            #[cfg(target_os = "linux")]
            http_builds: Default::default(),
            attachments: Default::default(),
            execs: Default::default(),
            binds_under: None,
            kind_node_binds: false,
        }
    }

    #[cfg(test)]
    pub fn for_test(root: &std::path::Path) -> Ctx {
        Ctx::new(Dirs {
            data: root.join("data"),
            runtime: root.join("run"),
            config: root.join("config"),
        })
    }
}

/// `/v1.47/containers/json` → `/containers/json`.
pub fn strip_version(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("/v1.") {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            let after = &rest[digits..];
            if after.is_empty() {
                return "/";
            }
            if after.starts_with('/') {
                return after;
            }
        }
    }
    path
}

/// Requests whose body the handler reads as it arrives, without a size limit: archives
/// of files and of images.
pub fn streams_body(req: &Request) -> bool {
    let path = strip_version(&req.path);
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    if req.method == "POST" && segments == ["images", "create"] {
        return req.query.get("fromSrc") == Some("-");
    }
    matches!(
        (req.method.as_str(), segments.as_slice()),
        ("PUT", ["containers", _, "archive"]) | ("POST", ["images", "load"] | ["build"])
    )
}

/// Send the request to its handler.
pub fn route(req: &Request, ctx: &Ctx) -> Reply {
    let path = strip_version(&req.path);
    if ctx.volume_lesson {
        let read = req.method == "GET"
            && matches!(
                path,
                "/version"
                    | "/_ping"
                    | "/events"
                    | "/containers/json"
                    | "/images/json"
                    | "/networks"
                    | "/volumes"
            );
        let volume = matches!(req.method.as_str(), "GET" | "DELETE")
            && path
                .strip_prefix("/volumes/")
                .is_some_and(|n| !n.is_empty() && !n.contains('/'));
        if !(read || volume || (req.method == "HEAD" && path == "/_ping")) {
            return message(
                403,
                "This session is for inspecting and removing practice volumes.",
            );
        }
        if req.method == "DELETE" && path == "/volumes/my-api" {
            let reply =
                super::objects::volumes(req, ctx, &["my-api"]).expect("DELETE volume route");
            if matches!(reply, Reply::Empty(204))
                && let Err(e) = std::fs::write(ctx.dirs.runtime.join("removed"), [])
            {
                return message(500, &e.to_string());
            }
            return reply;
        }
    }
    if let Some(reply) = path
        .strip_prefix("/images/")
        .and_then(|rest| super::images::route(req, ctx, rest))
    {
        return reply;
    }
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segments.as_slice()) {
        #[cfg(target_os = "linux")]
        ("POST", [endpoint @ ("grpc" | "session")]) => match &ctx.buildkit {
            Some(worker) => super::buildkit::upgrade(req, worker, *endpoint == "session"),
            None => message(404, "page not found"),
        },
        // Keep the classic builder as the default recommendation. BuildKit is opt-in.
        ("GET" | "HEAD", ["_ping"]) => Reply::Headed(
            vec![("Builder-Version", "1".to_string())],
            Box::new(Reply::Text(200, "OK".to_string())),
        ),
        ("GET", ["version"]) => super::system::version(),
        ("GET", ["info"]) => super::system::info(ctx),
        ("GET", ["system", "df"]) => super::system::df(),
        ("POST", ["auth"]) => super::images::login(req),
        ("GET", ["events"]) => super::events::stream(req),
        ("HEAD", ["containers", id, "archive"]) => super::containers::stat_path(req, id),
        ("GET", ["containers", id, "archive"]) => super::containers::copy_out(req, id),
        ("PUT", ["containers", id, "archive"]) => super::containers::copy_in(req, id),
        ("GET", ["containers", id, "export"]) => super::build::export(id),
        ("GET", ["containers", id, "changes"]) => super::build::changes(id),
        (_, ["containers", rest @ ..]) => super::containers::route(req, ctx, rest)
            .unwrap_or_else(|| message(404, "page not found")),
        (_, ["volumes", rest @ ..]) => super::objects::volumes(req, ctx, rest)
            .unwrap_or_else(|| message(404, "page not found")),
        (_, ["networks", rest @ ..]) => super::objects::networks(req, ctx, rest)
            .unwrap_or_else(|| message(404, "page not found")),
        (_, ["exec", rest @ ..]) => {
            super::exec::route(req, ctx, rest).unwrap_or_else(|| message(404, "page not found"))
        }
        ("POST", ["commit"]) => super::build::commit(req),
        ("POST", ["build"]) => super::build::build(req, ctx),
        ("POST", ["build", "prune"]) => super::build::prune_cache(ctx),
        (_, ["build", ..]) => not_supported(
            "BuildKit's build API",
            "use the classic builder (DOCKER_BUILDKIT=0), or buildx with the docker-container driver",
        ),
        (
            _,
            ["swarm", ..]
            | ["services", ..]
            | ["nodes", ..]
            | ["tasks", ..]
            | ["secrets", ..]
            | ["configs", ..],
        ) => crate::swarm::http::route(req, ctx, &segments)
            .unwrap_or_else(|| message(404, "page not found")),
        (_, ["plugins", ..]) => not_supported(
            "plugins",
            "use the built-in local volumes and bridge networks",
        ),
        _ => message(404, "page not found"),
    }
}

/// A JSON `{"message": …}` reply.
pub fn message(status: u16, text: impl Into<String>) -> Reply {
    Reply::Json(status, serde_json::json!({ "message": text.into() }))
}

/// A podbag error as an HTTP reply.
pub fn reply_error(e: &Error) -> Reply {
    let status = match e.kind {
        ErrorKind::NotFound => 404,
        ErrorKind::Conflict => 409,
        ErrorKind::InvalidInput => 400,
        ErrorKind::Unsupported => 501,
        _ => 500,
    };
    message(status, error_text(e))
}

/// A podbag error in one line: what failed, why, and what to do.
pub fn error_text(e: &Error) -> String {
    format!("{}: {} ({})", e.what, e.why, e.next)
}

/// Refuse filters podbag does not apply. Docker answers an unknown key with 400;
/// ignoring it would select everything, and `docker rm $(docker ps -aq --filter volume=x)`
/// would remove every container. Keys Docker has and podbag lacks answer 501.
pub fn check_filters(
    filters: &std::collections::BTreeMap<String, Vec<String>>,
    supported: &[&str],
    lacking: &[&str],
) -> Result<(), Reply> {
    for key in filters.keys() {
        if supported.contains(&key.as_str()) {
            continue;
        }
        if lacking.contains(&key.as_str()) {
            return Err(not_supported(
                &format!("the {key} filter"),
                "filter the output on your side",
            ));
        }
        return Err(message(400, format!("invalid filter '{key}'")));
    }
    Ok(())
}

/// One `label` filter value against an object's labels: `key` (any value) or `key=value`
/// (the value is all that follows the first `=`).
pub fn label_matches(want: &str, labels: &std::collections::BTreeMap<String, String>) -> bool {
    match want.split_once('=') {
        Some((k, v)) => labels.get(k).is_some_and(|x| x == v),
        None => labels.contains_key(want),
    }
}

/// Docker's `label` and `label!` filters: every wanted label is there, and no unwanted
/// one.
pub fn labels_match(
    labels: &std::collections::BTreeMap<String, String>,
    filters: &std::collections::BTreeMap<String, Vec<String>>,
) -> bool {
    let values = |key: &str| filters.get(key).map(Vec::as_slice).unwrap_or_default();
    values("label").iter().all(|w| label_matches(w, labels))
        && !values("label!").iter().any(|w| label_matches(w, labels))
}

/// 501: a Docker feature podbag does not have, and what to do instead.
pub fn not_supported(what: &str, instead: &str) -> Reply {
    message(
        501,
        format!("{what} is not supported by podbag ({instead})"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_match_in_every_form() {
        let labels: std::collections::BTreeMap<String, String> =
            [("a", "b"), ("empty", ""), ("eq", "x=y")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let filters =
            |pairs: &[(&str, &[&str])]| -> std::collections::BTreeMap<String, Vec<String>> {
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
                    .collect()
            };
        let matches = |pairs: &[(&str, &[&str])]| labels_match(&labels, &filters(pairs));
        // No label filter: everything.
        assert!(matches(&[]));
        assert!(matches(&[("name", &["x"])]));
        // A key alone, a key with its value, a value with an equals sign, an empty value.
        assert!(matches(&[("label", &["a"])]));
        assert!(matches(&[("label", &["a=b"])]));
        assert!(!matches(&[("label", &["a=c"])]));
        assert!(matches(&[("label", &["eq=x=y"])]));
        assert!(!matches(&[("label", &["eq=x"])]));
        assert!(matches(&[("label", &["empty"])]));
        assert!(matches(&[("label", &["empty="])]));
        assert!(!matches(&[("label", &["a="])]));
        assert!(!matches(&[("label", &["missing"])]));
        // All wanted labels, and none of the unwanted.
        assert!(matches(&[("label", &["a", "eq=x=y"])]));
        assert!(!matches(&[("label", &["a", "missing"])]));
        assert!(matches(&[("label!", &["missing"])]));
        assert!(!matches(&[("label!", &["a"])]));
        assert!(!matches(&[("label!", &["missing", "a=b"])]));
        assert!(matches(&[("label!", &["a=c"])]));
        assert!(!matches(&[("label", &["a"]), ("label!", &["eq"])]));
        // An object without labels.
        let none = std::collections::BTreeMap::new();
        assert!(!labels_match(&none, &filters(&[("label", &["a"])])));
        assert!(labels_match(&none, &filters(&[("label!", &["a"])])));
    }

    #[test]
    fn version_prefixes_are_stripped() {
        assert_eq!(strip_version("/v1.24/_ping"), "/_ping");
        assert_eq!(strip_version("/v1.47/containers/json"), "/containers/json");
        assert_eq!(strip_version("/_ping"), "/_ping");
        assert_eq!(strip_version("/v1.47"), "/");
        assert_eq!(strip_version("/v2/x"), "/v2/x");
    }
}
