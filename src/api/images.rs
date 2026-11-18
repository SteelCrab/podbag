//! `/images/…`: pull, list, inspect, tag, remove, prune.

use super::docker::{docker_arch, docker_image_name, docker_repo, rfc3339};
use super::http::Request;
use super::router::{Ctx, Reply, check_filters, error_text, message, not_supported, reply_error};
use crate::image::reference::Reference;
use crate::image::{Image, ImageConfig, PullEvent};
use crate::store::{ImageRecord, Store};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;

/// Routes under `/images/`; `rest` is the path after it. Image names may hold `/` and `:`,
/// so the action is the last segment. `None`: no such route.
pub fn route(req: &Request, ctx: &Ctx, rest: &str) -> Option<Reply> {
    Some(match (req.method.as_str(), rest) {
        ("GET", "json") => list(req, ctx),
        ("POST", "create") => create(req),
        ("POST", "prune") => prune(req),
        ("GET", "search") => {
            not_supported("searching a registry", "search on the registry's web site")
        }
        ("GET", "get") | ("POST", "load") => {
            not_supported("saving and loading images", "pull images from a registry")
        }
        ("GET", r) if r.ends_with("/json") => inspect(ctx, &r[..r.len() - "/json".len()]),
        ("GET", r) if r.ends_with("/get") => {
            not_supported("saving images", "pull images from a registry")
        }
        ("GET", r) if r.ends_with("/history") => {
            not_supported("image history", "use `docker image inspect`")
        }
        ("POST", r) if r.ends_with("/tag") => tag(req, &r[..r.len() - "/tag".len()]),
        ("POST", r) if r.ends_with("/push") => {
            not_supported("pushing images", "push with another tool, like skopeo")
        }
        ("DELETE", r) if !r.is_empty() => remove(ctx, r),
        _ => return None,
    })
}

/// Turns pull events into Docker's JSON progress messages.
pub struct PullMessages {
    /// The tag (or digest) the client asked for: the id of the first message.
    tag: String,
    /// The digest the name had before this pull.
    before: Option<String>,
    /// The name had this manifest already: no layer lines, like Docker.
    up_to_date: bool,
}

impl PullMessages {
    pub fn new(tag: &str, before: Option<String>) -> PullMessages {
        PullMessages {
            tag: tag.to_string(),
            before,
            up_to_date: false,
        }
    }

    pub fn on(&mut self, ev: &PullEvent) -> Vec<Value> {
        match ev {
            PullEvent::Resolved { name, digest, .. } => {
                self.up_to_date = self.before.as_deref() == Some(digest.as_str());
                let repo = Reference::parse(name)
                    .map(|r| r.repository)
                    .unwrap_or_else(|_| docker_repo(name));
                vec![json!({"status": format!("Pulling from {repo}"), "id": self.tag})]
            }
            _ if self.up_to_date => Vec::new(),
            PullEvent::LayerCached { digest, .. } => vec![layer("Already exists", digest)],
            PullEvent::LayerDone { digest, .. } => vec![layer("Pull complete", digest)],
        }
    }

    pub fn finish(&self, image: &Image) -> Vec<Value> {
        let name = docker_image_name(&image.name);
        let status = if self.up_to_date {
            format!("Status: Image is up to date for {name}")
        } else {
            format!("Status: Downloaded newer image for {name}")
        };
        vec![
            json!({"status": format!("Digest: {}", image.digest)}),
            json!({"status": status}),
        ]
    }
}

fn layer(status: &str, digest: &str) -> Value {
    let short: String = digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .chars()
        .take(12)
        .collect();
    json!({"status": status, "progressDetail": {}, "id": short})
}

/// The last message of a failed pull.
pub fn failure(text: &str) -> Value {
    json!({"errorDetail": {"message": text}, "error": text})
}

fn send(out: &mut impl Write, messages: Vec<Value>) {
    for m in messages {
        let mut line = serde_json::to_vec(&m).unwrap_or_default();
        line.push(b'\n');
        // A client that left does not stop the pull: the image is stored anyway.
        let _ = out.write_all(&line);
    }
    let _ = out.flush();
}

/// `POST /images/create?fromImage=…&tag=…`: pull, streaming progress.
fn create(req: &Request) -> Reply {
    if req.query.get("fromSrc").is_some_and(|s| !s.is_empty()) {
        return not_supported(
            "importing a root filesystem (fromSrc)",
            "pull an image, or run a directory with `openpod run --rootfs`",
        );
    }
    let Some(from) = req.query.get("fromImage").filter(|f| !f.is_empty()) else {
        return message(400, "fromImage is required");
    };
    let tag = req.query.get("tag").unwrap_or("");
    let reference = match tag {
        "" => from.to_string(),
        t if t.starts_with("sha256:") => format!("{from}@{t}"),
        t => format!("{from}:{t}"),
    };
    let parsed = match Reference::parse(&reference) {
        Ok(r) => r,
        Err(e) => return reply_error(&e),
    };
    let shown = parsed
        .digest
        .clone()
        .or(parsed.tag.clone())
        .unwrap_or_else(|| "latest".to_string());
    let wrong_platform = req
        .query
        .get("platform")
        .filter(|p| !p.is_empty() && !is_this_platform(p))
        .map(String::from);
    let before = Image::get(&reference).ok().map(|i| i.digest);
    Reply::Stream {
        content_type: "application/json",
        body: Box::new(move |out| {
            if let Some(p) = wrong_platform {
                let here = format!("linux/{}", docker_arch(std::env::consts::ARCH));
                send(
                    out,
                    vec![failure(&format!(
                        "cannot pull {reference} for {p}: openpod pulls images for this machine ({here}) only"
                    ))],
                );
                return Ok(());
            }
            let mut messages = PullMessages::new(&shown, before);
            let result = Image::pull_with_events(&reference, |ev| {
                let m = messages.on(&ev);
                send(out, m);
            });
            match result {
                Ok(image) => {
                    let m = messages.finish(&image);
                    send(out, m);
                    let name = docker_image_name(&image.name);
                    super::events::publish("image", "pull", &name, &[("name", name.clone())]);
                }
                Err(e) => send(out, vec![failure(&error_text(&e))]),
            }
            Ok(())
        }),
    }
}

/// `linux/arm64`, `linux/arm64/v8` on an arm64 machine.
pub(super) fn is_this_platform(p: &str) -> bool {
    let mut parts = p.split('/');
    parts.next() == Some("linux")
        && parts.next() == Some(docker_arch(std::env::consts::ARCH).as_str())
}

/// Every image name, grouped by image (digest).
fn stored(ctx: &Ctx) -> crate::Result<BTreeMap<String, (Vec<String>, ImageRecord)>> {
    let mut images: BTreeMap<String, (Vec<String>, ImageRecord)> = BTreeMap::new();
    if let Some(store) = Store::open_existing(&ctx.dirs) {
        for (name, record, _) in store.list()? {
            images
                .entry(record.digest.clone())
                .or_insert_with(|| (Vec::new(), record))
                .0
                .push(name);
        }
    }
    Ok(images)
}

/// Docker's RepoTags and RepoDigests for these full names.
fn repo_names(names: &[String], digest: &str) -> (Vec<String>, Vec<String>) {
    let tags: Vec<String> = names
        .iter()
        .filter(|n| !n.contains('@'))
        .map(|n| docker_image_name(n))
        .collect();
    let mut digests: Vec<String> = names
        .iter()
        .map(|n| format!("{}@{digest}", docker_repo(n)))
        .collect();
    digests.sort();
    digests.dedup();
    (tags, digests)
}

/// Go's path.Match for `*` and `?` (neither matches `/`), as Docker's reference filter.
fn glob(pattern: &str, text: &str) -> bool {
    match pattern.chars().next() {
        None => text.is_empty(),
        Some('*') => {
            let rest = &pattern[1..];
            let mut t = text;
            loop {
                if glob(rest, t) {
                    return true;
                }
                match t.chars().next() {
                    Some(c) if c != '/' => t = &t[c.len_utf8()..],
                    _ => return false,
                }
            }
        }
        Some(p) => match text.chars().next() {
            Some(c) if (p == '?' && c != '/') || p == c => {
                glob(&pattern[p.len_utf8()..], &text[c.len_utf8()..])
            }
            _ => false,
        },
    }
}

/// Docker's `reference` filter: the pattern matches the short name with its tag, or
/// without it when the pattern has no tag.
fn matches_reference(pattern: &str, full: &str) -> bool {
    glob(pattern, &docker_image_name(full)) || glob(pattern, &docker_repo(full))
}

fn check_list_filters(filters: &BTreeMap<String, Vec<String>>) -> Result<(), Reply> {
    check_filters(
        filters,
        &["dangling", "label", "reference"],
        &["before", "since", "until"],
    )
}

/// `GET /images/json`.
fn list(req: &Request, ctx: &Ctx) -> Reply {
    let filters = match req.query.filters() {
        Ok(f) => f,
        Err(_) => return message(400, "invalid filters"),
    };
    if let Err(reply) = check_list_filters(&filters) {
        return reply;
    }
    let wants = |key: &str, value: &str| {
        filters
            .get(key)
            .is_some_and(|v| v.iter().any(|x| x == value))
    };
    // openpod keeps no image without a name, and no image labels.
    if wants("dangling", "true") || wants("dangling", "1") || filters.contains_key("label") {
        return Reply::Json(200, json!([]));
    }
    let images = match stored(ctx) {
        Ok(i) => i,
        Err(e) => return reply_error(&e),
    };
    let mut out: Vec<(u64, Value)> = Vec::new();
    for (digest, (names, record)) in images {
        let names: Vec<String> = match filters.get("reference") {
            Some(patterns) => names
                .into_iter()
                .filter(|n| patterns.iter().any(|p| matches_reference(p, n)))
                .collect(),
            None => names,
        };
        if names.is_empty() {
            continue;
        }
        let (tags, digests) = repo_names(&names, &digest);
        out.push((
            record.created,
            json!({
                "Id": digest,
                "ParentId": "",
                "RepoTags": tags,
                "RepoDigests": digests,
                "Created": record.created,
                "Size": record.size,
                "SharedSize": -1,
                "VirtualSize": record.size,
                "Labels": {},
                "Containers": -1,
            }),
        ));
    }
    // Newest first, like Docker.
    out.sort_by(|a, b| b.0.cmp(&a.0));
    Reply::Json(200, Value::Array(out.into_iter().map(|(_, v)| v).collect()))
}

/// Docker's image `Config`.
fn config_json(c: &ImageConfig) -> Value {
    let list = |v: &[String]| {
        if v.is_empty() { Value::Null } else { json!(v) }
    };
    let env: Vec<String> = c.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let volumes: serde_json::Map<String, Value> =
        c.volumes.iter().map(|v| (v.clone(), json!({}))).collect();
    json!({
        "Hostname": "",
        "Domainname": "",
        "User": c.user.clone().unwrap_or_default(),
        "AttachStdin": false,
        "AttachStdout": false,
        "AttachStderr": false,
        "Tty": false,
        "OpenStdin": false,
        "StdinOnce": false,
        "Env": list(&env),
        "Cmd": list(&c.cmd),
        "Image": "",
        "Volumes": if volumes.is_empty() { Value::Null } else { Value::Object(volumes) },
        "WorkingDir": c.workdir.clone().unwrap_or_default(),
        "Entrypoint": list(&c.entrypoint),
        "OnBuild": null,
        "Labels": null,
    })
}

/// A stored image by name or digest prefix, with all its names.
fn find(ctx: &Ctx, name: &str) -> Result<(ImageRecord, Vec<String>), Reply> {
    let missing = || message(404, format!("No such image: {name}"));
    let store = Store::open_existing(&ctx.dirs).ok_or_else(missing)?;
    let (_, record) = store.find(name).map_err(|e| match e.kind {
        crate::ErrorKind::NotFound => missing(),
        _ => reply_error(&e),
    })?;
    let names = store
        .list()
        .map_err(|e| reply_error(&e))?
        .into_iter()
        .filter(|(_, r, _)| r.digest == record.digest)
        .map(|(n, _, _)| n)
        .collect();
    Ok((record, names))
}

/// `GET /images/{name}/json`.
fn inspect(ctx: &Ctx, name: &str) -> Reply {
    let (record, names) = match find(ctx, name) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let (tags, digests) = repo_names(&names, &record.digest);
    Reply::Json(
        200,
        json!({
            "Id": record.digest,
            "RepoTags": tags,
            "RepoDigests": digests,
            "Parent": "",
            "Comment": "",
            "Created": rfc3339(Some(record.created)),
            "DockerVersion": "",
            "Author": "",
            "Config": config_json(&record.config),
            "Architecture": docker_arch(std::env::consts::ARCH),
            "Os": "linux",
            "Size": record.size,
            "VirtualSize": record.size,
            "GraphDriver": {"Name": "openpod", "Data": null},
            "RootFS": {"Type": "layers", "Layers": record.diff_ids},
            "Metadata": {"LastTagTime": "0001-01-01T00:00:00Z"},
        }),
    )
}

/// `DELETE /images/{name}`: `[{"Untagged": …}, {"Deleted": …}]`.
fn remove(ctx: &Ctx, name: &str) -> Reply {
    let digest = match find(ctx, name) {
        Ok((record, _)) => record.digest,
        Err(reply) => return reply,
    };
    let removed = match Image::remove(name) {
        Ok(r) => r,
        Err(e) => return reply_error(&e),
    };
    let mut out = Vec::new();
    for n in &removed {
        let short = docker_image_name(n);
        super::events::publish("image", "untag", &digest, &[("name", short.clone())]);
        out.push(json!({ "Untagged": short }));
    }
    let gone = stored(ctx).is_ok_and(|images| !images.contains_key(&digest));
    if gone {
        super::events::publish("image", "delete", &digest, &[]);
        out.push(json!({ "Deleted": digest }));
    }
    Reply::Json(200, Value::Array(out))
}

/// `POST /images/prune`: dangling images by default (openpod has none), every unused
/// image with `dangling=false`.
fn prune(req: &Request) -> Reply {
    let filters = match req.query.filters() {
        Ok(f) => f,
        Err(_) => return message(400, "invalid filters"),
    };
    if let Err(reply) = check_filters(&filters, &["dangling", "label"], &["until"]) {
        return reply;
    }
    let all = filters
        .get("dangling")
        .is_some_and(|v| v.iter().any(|d| d == "false" || d == "0"));
    if !all || filters.contains_key("label") {
        return Reply::Json(200, json!({"ImagesDeleted": null, "SpaceReclaimed": 0}));
    }
    match crate::image::prune() {
        Ok(report) => {
            let deleted: Vec<Value> = report
                .removed_images
                .iter()
                .map(|n| {
                    let short = docker_image_name(n);
                    super::events::publish("image", "untag", &short, &[("name", short.clone())]);
                    json!({ "Untagged": short })
                })
                .collect();
            Reply::Json(
                200,
                json!({"ImagesDeleted": deleted, "SpaceReclaimed": report.freed_bytes}),
            )
        }
        Err(e) => reply_error(&e),
    }
}

/// `POST /images/{name}/tag?repo=…&tag=…`.
fn tag(req: &Request, source: &str) -> Reply {
    let Some(repo) = req.query.get("repo").filter(|r| !r.is_empty()) else {
        return message(400, "repo is required");
    };
    let target = match req.query.get("tag").filter(|t| !t.is_empty()) {
        Some(t) => format!("{repo}:{t}"),
        None => repo.to_string(),
    };
    match Image::tag(source, &target) {
        Ok(full) => {
            let short = docker_image_name(&full);
            let id = Image::get(&full).map(|i| i.digest).unwrap_or_default();
            super::events::publish("image", "tag", &id, &[("name", short)]);
            Reply::Empty(201)
        }
        Err(e) => reply_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:aaaa000000000000000000000000000000000000000000000000000000000000";
    const LAYER: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn image() -> Image {
        Image {
            name: "127.0.0.1:5000/x:v1".to_string(),
            digest: DIGEST.to_string(),
            size: 1,
            config: Default::default(),
        }
    }

    fn resolved() -> PullEvent {
        PullEvent::Resolved {
            name: "127.0.0.1:5000/x:v1".to_string(),
            digest: DIGEST.to_string(),
            layers: 2,
        }
    }

    #[test]
    fn pull_events_become_docker_messages() {
        let mut m = PullMessages::new("v1", None);
        assert_eq!(
            m.on(&resolved()),
            vec![json!({"status": "Pulling from x", "id": "v1"})]
        );
        assert_eq!(
            m.on(&PullEvent::LayerCached {
                index: 0,
                digest: LAYER.to_string()
            }),
            vec![json!({"status": "Already exists", "progressDetail": {}, "id": "0123456789ab"})]
        );
        assert_eq!(
            m.on(&PullEvent::LayerDone {
                index: 1,
                digest: LAYER.to_string(),
                bytes: 10
            }),
            vec![json!({"status": "Pull complete", "progressDetail": {}, "id": "0123456789ab"})]
        );
        assert_eq!(
            m.finish(&image()),
            vec![
                json!({"status": format!("Digest: {DIGEST}")}),
                json!({"status": "Status: Downloaded newer image for 127.0.0.1:5000/x:v1"}),
            ]
        );
    }

    #[test]
    fn a_pull_of_the_same_manifest_is_up_to_date() {
        let mut m = PullMessages::new("v1", Some(DIGEST.to_string()));
        assert_eq!(m.on(&resolved()).len(), 1);
        assert!(
            m.on(&PullEvent::LayerCached {
                index: 0,
                digest: LAYER.to_string()
            })
            .is_empty()
        );
        assert_eq!(
            m.finish(&image())[1],
            json!({"status": "Status: Image is up to date for 127.0.0.1:5000/x:v1"})
        );
    }

    #[test]
    fn docker_hub_names_are_short() {
        let mut m = PullMessages::new("latest", None);
        let ev = PullEvent::Resolved {
            name: "docker.io/library/alpine:latest".to_string(),
            digest: DIGEST.to_string(),
            layers: 1,
        };
        assert_eq!(
            m.on(&ev),
            vec![json!({"status": "Pulling from library/alpine", "id": "latest"})]
        );
        let mut img = image();
        img.name = "docker.io/library/alpine:latest".to_string();
        assert_eq!(
            m.finish(&img)[1],
            json!({"status": "Status: Downloaded newer image for alpine:latest"})
        );
    }

    #[test]
    fn reference_filters_match_like_docker() {
        let full = "docker.io/library/alpine:3.22";
        assert!(matches_reference("alpine", full));
        assert!(matches_reference("alpine:3.22", full));
        assert!(matches_reference("alp*", full));
        assert!(matches_reference("alpine:*", full));
        assert!(!matches_reference("alpine:3.21", full));
        assert!(
            !matches_reference("*", "ghcr.io/a/b:1"),
            "* stops at a slash"
        );
        assert!(matches_reference("ghcr.io/*/b", "ghcr.io/a/b:1"));
    }

    #[test]
    fn names_split_into_tags_and_digests() {
        let names = vec![
            "docker.io/library/alpine:3.22".to_string(),
            "docker.io/library/alpine:latest".to_string(),
            format!("docker.io/library/alpine@{DIGEST}"),
        ];
        let (tags, digests) = repo_names(&names, DIGEST);
        assert_eq!(tags, vec!["alpine:3.22", "alpine:latest"]);
        assert_eq!(digests, vec![format!("alpine@{DIGEST}")]);
    }

    #[test]
    fn image_filters_openpod_lacks_are_refused() {
        let keys = |k: &str| BTreeMap::from([(k.to_string(), vec!["x".to_string()])]);
        assert!(check_list_filters(&keys("reference")).is_ok());
        assert!(check_list_filters(&keys("dangling")).is_ok());
        for key in ["before", "since", "until", "bogus"] {
            assert!(check_list_filters(&keys(key)).is_err(), "{key}");
        }
    }

    #[test]
    fn a_failure_has_both_error_fields() {
        assert_eq!(
            failure("cannot pull x: no such image (check the name)"),
            json!({
                "errorDetail": {"message": "cannot pull x: no such image (check the name)"},
                "error": "cannot pull x: no such image (check the name)"
            })
        );
    }
}
