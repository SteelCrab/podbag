//! The classic builder (`POST /build`), `POST /commit`, and a container's changes and
//! files (`/containers/{id}/changes`, `/containers/{id}/export`).

use super::http::Request;
use super::router::{Ctx, Reply, message, reply_error};
use crate::{ContainerHandle, ErrorKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;

/// A container by name or ID, or a 404 in Docker's words.
fn find(name: &str) -> Result<ContainerHandle, Reply> {
    ContainerHandle::get(name).map_err(|e| match e.kind {
        ErrorKind::NotFound => message(404, format!("No such container: {name}")),
        _ => reply_error(&e),
    })
}

/// A query parameter that holds a JSON object of strings (`buildargs`, `labels`). A
/// `null` value (an argument given without one) is left out.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn json_map(req: &Request, key: &str) -> Result<BTreeMap<String, String>, Reply> {
    let Some(text) = req.query.get(key).filter(|t| !t.is_empty()) else {
        return Ok(BTreeMap::new());
    };
    let map: BTreeMap<String, Option<String>> =
        serde_json::from_str(text).map_err(|e| message(400, format!("invalid {key}: {e}")))?;
    Ok(map
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn send(out: &mut dyn Write, value: &Value) {
    let mut line = serde_json::to_vec(value).unwrap_or_default();
    line.push(b'\n');
    // A client that left does not stop the build: its image is stored anyway.
    let _ = out.write_all(&line);
    let _ = out.flush();
}

/// `POST /build`: the context tar in the body; progress as Docker's JSON messages.
#[cfg(target_os = "linux")]
pub fn build(req: &Request) -> Reply {
    use crate::build::engine::{self, BuildOptions};
    let q = &req.query;
    if q.get("remote").is_some_and(|r| !r.is_empty()) {
        return super::router::not_supported(
            "a remote build context",
            "send the context in the request body",
        );
    }
    if q.get("version") == Some("2") || q.get("outputs").is_some_and(|o| !o.is_empty()) {
        return super::router::not_supported(
            "BuildKit builds",
            "use the classic builder (DOCKER_BUILDKIT=0), or buildx with the docker-container driver",
        );
    }
    let other_platform = q
        .get("platform")
        .filter(|p| !p.is_empty() && !crate::build::steps::is_this_platform(p));
    if let Some(p) = other_platform {
        return super::router::not_supported(
            &format!("building for {p}"),
            "build for this machine's platform, or use buildx",
        );
    }
    let (build_args, labels) = match (json_map(req, "buildargs"), json_map(req, "labels")) {
        (Ok(a), Ok(l)) => (a, l),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let registry_logins = match req
        .header("X-Registry-Config")
        .map(crate::image::auth::logins_from_config_header)
        .transpose()
    {
        Ok(logins) => logins.unwrap_or_default(),
        Err(e) => return reply_error(&e),
    };
    let dockerfile = q
        .get("dockerfile")
        .filter(|d| !d.is_empty())
        .unwrap_or("Dockerfile")
        .to_string();
    let options = BuildOptions {
        tags: q.all("t").into_iter().map(String::from).collect(),
        build_args,
        labels,
        target: q.get("target").filter(|t| !t.is_empty()).map(String::from),
        no_cache: q.bool_param("nocache"),
        pull: q.bool_param("pull"),
        network: q
            .get("networkmode")
            .filter(|n| !n.is_empty())
            .map(String::from),
        quiet: q.bool_param("q"),
        registry_logins,
        ..BuildOptions::default()
    };
    Reply::WithBody(Box::new(move |body| {
        let context = match engine::unpack_context(body) {
            Ok(c) => c,
            Err(e) => return reply_error(&e),
        };
        let (text, ignore) = match engine::read_dockerfile(&context.dir, &dockerfile) {
            Ok(found) => found,
            Err(e) => return reply_error(&e),
        };
        let options = BuildOptions {
            context: context.dir.clone(),
            dockerfile: text,
            dockerfile_name: dockerfile,
            ignore,
            ..options
        };
        Reply::Stream {
            content_type: "application/json",
            body: Box::new(move |w| {
                let result = engine::build(&options, &mut |text| send(w, &json!({"stream": text})));
                match result {
                    Ok(built) => {
                        send(w, &json!({"aux": {"ID": built.id}}));
                        for name in &built.names {
                            let short = super::docker::docker_image_name(name);
                            super::events::publish("image", "tag", &built.id, &[("name", short)]);
                        }
                    }
                    Err(e) => {
                        let text = super::router::error_text(&e);
                        send(w, &json!({"errorDetail": {"message": text}, "error": text}));
                    }
                }
                drop(context);
                Ok(())
            }),
        }
    }))
}

#[cfg(not(target_os = "linux"))]
pub fn build(_req: &Request) -> Reply {
    reply_error(&crate::Error::unsupported_os())
}

/// `POST /build/prune`: forget the build cache. The images it named stay until
/// `image prune`.
pub fn prune_cache(ctx: &Ctx) -> Reply {
    let dir = ctx.dirs.data.join("store").join("build-cache");
    let mut deleted = Vec::new();
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        if std::fs::remove_file(entry.path()).is_ok() {
            deleted.push(name.trim_end_matches(".json").to_string());
        }
    }
    Reply::Json(
        200,
        json!({"CachesDeleted": deleted, "SpaceReclaimed": bytes}),
    )
}

/// `POST /commit?container=…&repo=…&tag=…&changes=…`.
pub fn commit(req: &Request) -> Reply {
    let q = &req.query;
    let Some(container) = q.get("container").filter(|c| !c.is_empty()) else {
        return message(400, "container is required");
    };
    let c = match find(container) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let name = q.get("repo").filter(|r| !r.is_empty()).map(|repo| {
        match q.get("tag").filter(|t| !t.is_empty()) {
            Some(t) => format!("{repo}:{t}"),
            None => repo.to_string(),
        }
    });
    // Each `changes` value may hold several instructions, one per line.
    let changes = q
        .all("changes")
        .into_iter()
        .flat_map(|c| {
            c.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect();
    match c.commit_with_metadata(
        &crate::build::CommitOptions { name, changes },
        q.get("author").unwrap_or_default(),
        q.get("comment").unwrap_or_default(),
    ) {
        Ok(built) => {
            for n in &built.names {
                let short = super::docker::docker_image_name(n);
                super::events::publish("image", "tag", &built.id, &[("name", short)]);
            }
            Reply::Json(201, json!({"Id": built.id}))
        }
        Err(e) => reply_error(&e),
    }
}

/// `GET /containers/{id}/changes`.
pub fn changes(id: &str) -> Reply {
    let c = match find(id) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match c.changes() {
        Ok(list) => Reply::Json(
            200,
            Value::Array(
                list.into_iter()
                    .map(|c| json!({"Path": c.path, "Kind": c.kind}))
                    .collect(),
            ),
        ),
        Err(e) => reply_error(&e),
    }
}

/// `GET /containers/{id}/export`.
pub fn export(id: &str) -> Reply {
    let c = match find(id) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match c.export() {
        Ok(mut tar) => Reply::Chunked {
            content_type: "application/x-tar",
            body: Box::new(move |w| std::io::copy(&mut tar, w).map(drop)),
        },
        Err(e) => reply_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(query: &str) -> Request {
        let raw = format!("POST /build?{query} HTTP/1.1\r\nHost: x\r\n\r\n");
        super::super::http::read_request(&mut raw.as_bytes())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn build_args_and_labels_from_json() {
        let req = request("buildargs=%7B%22A%22%3A%221%22%2C%22B%22%3Anull%7D&labels=%7B%7D");
        let args = json_map(&req, "buildargs").ok().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args.get("A").map(String::as_str), Some("1"));
        assert!(json_map(&req, "labels").ok().unwrap().is_empty());
        assert!(json_map(&request("buildargs=%5B1%5D"), "buildargs").is_err());
    }
}
