//! Paths to handlers, and errors to HTTP status codes.

use super::http::Request;
use crate::paths::Dirs;
use crate::{Error, ErrorKind};
use std::os::unix::net::UnixStream;

/// The API version openpod speaks. Clients negotiate down to it.
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
}

/// The response head of a hijacked connection.
pub struct Upgrade {
    /// The client asked to upgrade the connection (101); otherwise 200.
    pub upgrade: bool,
    pub content_type: &'static str,
}

impl Upgrade {
    pub fn send(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let mut extra = vec![("Content-Type", self.content_type.to_string())];
        if self.upgrade {
            extra.push(("Connection", "Upgrade".to_string()));
            extra.push(("Upgrade", "tcp".to_string()));
        }
        super::http::write_head(w, if self.upgrade { 101 } else { 200 }, &extra)
    }
}

/// What every handler can reach.
pub struct Ctx {
    pub dirs: Dirs,
    /// Clients attached to containers that wait for a start, and how starts ended.
    pub attachments: std::sync::Arc<super::stream::Attachments>,
    /// Commands made by `POST /containers/{id}/exec`.
    pub execs: super::exec::Execs,
}

impl Ctx {
    pub fn new(dirs: Dirs) -> Ctx {
        Ctx {
            dirs,
            attachments: Default::default(),
            execs: Default::default(),
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

/// Send the request to its handler.
pub fn route(req: &Request, ctx: &Ctx) -> Reply {
    let path = strip_version(&req.path);
    if let Some(reply) = path
        .strip_prefix("/images/")
        .and_then(|rest| super::images::route(req, ctx, rest))
    {
        return reply;
    }
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segments.as_slice()) {
        ("GET" | "HEAD", ["_ping"]) => Reply::Text(200, "OK".to_string()),
        ("GET", ["version"]) => super::system::version(),
        ("GET", ["info"]) => super::system::info(ctx),
        ("GET", ["system", "df"]) => super::system::df(),
        ("GET", ["events"]) => super::events::stream(req),
        (_, ["containers", _, "top"]) => not_supported(
            "listing a container's processes (top)",
            "use `docker exec <container> ps`",
        ),
        (_, ["containers", _, "stats"]) => {
            not_supported("stats", "read the container's cgroup files")
        }
        (
            _,
            ["containers", _, "export"]
            | ["containers", _, "archive"]
            | ["containers", _, "changes"],
        ) => not_supported(
            "copying or diffing container files",
            "use a bind mount or `docker exec … tar`",
        ),
        (_, ["containers", rest @ ..]) => super::containers::route(req, ctx, rest)
            .unwrap_or_else(|| message(404, "page not found")),
        (_, ["volumes", rest @ ..]) => super::objects::volumes(req, ctx, rest)
            .unwrap_or_else(|| message(404, "page not found")),
        (_, ["networks", rest @ ..]) => super::objects::networks(req, ctx, rest)
            .unwrap_or_else(|| message(404, "page not found")),
        (_, ["exec", rest @ ..]) => {
            super::exec::route(req, ctx, rest).unwrap_or_else(|| message(404, "page not found"))
        }
        (_, ["commit"]) => not_supported("commit", "build images with buildx (later milestone)"),
        (_, ["build", ..]) => not_supported(
            "the classic builder (POST /build)",
            "use buildx with the docker-container driver (later milestone)",
        ),
        (
            _,
            ["swarm", ..]
            | ["services", ..]
            | ["nodes", ..]
            | ["tasks", ..]
            | ["secrets", ..]
            | ["configs", ..],
        ) => not_supported("swarm", "openpod runs containers on one host"),
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

/// An openpod error as an HTTP reply.
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

/// An openpod error in one line: what failed, why, and what to do.
pub fn error_text(e: &Error) -> String {
    format!("{}: {} ({})", e.what, e.why, e.next)
}

/// Refuse filters openpod does not apply. Docker answers an unknown key with 400;
/// ignoring it would select everything, and `docker rm $(docker ps -aq --filter volume=x)`
/// would remove every container. Keys Docker has and openpod lacks answer 501.
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

/// 501: a Docker feature openpod does not have, and what to do instead.
pub fn not_supported(what: &str, instead: &str) -> Reply {
    message(
        501,
        format!("{what} is not supported by openpod ({instead})"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_prefixes_are_stripped() {
        assert_eq!(strip_version("/v1.24/_ping"), "/_ping");
        assert_eq!(strip_version("/v1.47/containers/json"), "/containers/json");
        assert_eq!(strip_version("/_ping"), "/_ping");
        assert_eq!(strip_version("/v1.47"), "/");
        assert_eq!(strip_version("/v2/x"), "/v2/x");
    }
}
