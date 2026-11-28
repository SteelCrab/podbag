//! `/images/…`: pull, list, inspect, tag, remove, prune.

use super::docker::{docker_arch, docker_image_name, docker_repo, rfc3339};
use super::http::Request;
use super::router::{Ctx, Reply, check_filters, error_text, message, reply_error};
use crate::image::auth::AuthConfig;
use crate::image::reference::Reference;
use crate::image::{Image, ImageConfig, ImportOptions, PullEvent, PushEvent};
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
        ("GET", "search") => search(req),
        ("POST", "load") => load(),
        ("GET", "get") => save(req.query.all("names")),
        ("GET", r) if r.ends_with("/json") => inspect(ctx, &r[..r.len() - "/json".len()]),
        ("GET", r) if r.ends_with("/get") => save(vec![&r[..r.len() - "/get".len()]]),
        ("GET", r) if r.ends_with("/history") => history(ctx, &r[..r.len() - "/history".len()]),
        ("POST", r) if r.ends_with("/tag") => tag(req, &r[..r.len() - "/tag".len()]),
        ("POST", r) if r.ends_with("/push") => push(req, &r[..r.len() - "/push".len()]),
        ("DELETE", r) if !r.is_empty() => remove(ctx, r),
        _ => return None,
    })
}

/// `GET /images/get?names=…` and `GET /images/{name}/get`: the images as one archive.
fn save(names: Vec<&str>) -> Reply {
    if names.is_empty() {
        return super::router::message(400, "no image names given");
    }
    let names: Vec<String> = names.into_iter().map(str::to_string).collect();
    for name in &names {
        if let Err(e) = crate::Image::get(name) {
            return super::router::reply_error(&e);
        }
    }
    Reply::Chunked {
        content_type: "application/x-tar",
        body: Box::new(move |w| {
            crate::Image::save(&names, w).map_err(|e| std::io::Error::other(e.to_string()))
        }),
    }
}

/// `POST /images/load`: store the images of the archive in the body, then report their
/// names as Docker does.
fn load() -> Reply {
    Reply::WithBody(Box::new(|body| match crate::Image::load(body) {
        Ok(report) => {
            let mut lines = String::new();
            for name in &report.names {
                let text = format!("Loaded image: {}\n", super::docker::docker_image_name(name));
                lines.push_str(&serde_json::json!({ "stream": text }).to_string());
                lines.push('\n');
            }
            for id in &report.untagged {
                let text = format!("Loaded image ID: {id}\n");
                lines.push_str(&serde_json::json!({ "stream": text }).to_string());
                lines.push('\n');
            }
            Reply::Stream {
                content_type: "application/json",
                body: Box::new(move |w| std::io::Write::write_all(w, lines.as_bytes())),
            }
        }
        Err(e) => super::router::reply_error(&e),
    }))
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

/// `GET /images/{name}/history`: newest entry first.
fn history(ctx: &Ctx, name: &str) -> Reply {
    let Some(store) = Store::open_existing(&ctx.dirs) else {
        return message(404, format!("No such image: {name}"));
    };
    let record = match store.find(name) {
        Ok((_, record)) => record,
        Err(e) if e.kind == crate::ErrorKind::NotFound => {
            return message(404, format!("No such image: {name}"));
        }
        Err(e) => return reply_error(&e),
    };
    match store.image_history(&record) {
        Ok(mut rows) => {
            for entry in &mut rows {
                entry
                    .tags
                    .iter_mut()
                    .for_each(|name| *name = docker_image_name(name));
            }
            Reply::Json(200, json!(rows))
        }
        Err(e) => reply_error(&e),
    }
}

/// `POST /images/create?fromSrc=…`: import a root filesystem from the body or an URL.
fn import(req: &Request, source: &str) -> Reply {
    let q = &req.query;
    if let Some(platform) = q
        .get("platform")
        .filter(|p| !p.is_empty() && !is_this_platform(p))
    {
        return message(
            400,
            format!("cannot import an image for {platform}: podbag uses this machine's platform"),
        );
    }
    let name = q.get("repo").filter(|s| !s.is_empty()).map(|repo| {
        match q.get("tag").filter(|s| !s.is_empty()) {
            Some(tag) => format!("{repo}:{tag}"),
            None => repo.to_string(),
        }
    });
    let options = ImportOptions {
        name,
        message: q.get("message").unwrap_or_default().to_string(),
        changes: q.all("changes").into_iter().map(String::from).collect(),
    };
    let finish = move |result: crate::Result<String>| match result {
        Ok(id) => {
            super::events::publish("image", "import", &id, &[]);
            Reply::Stream {
                content_type: "application/json",
                body: Box::new(move |out| {
                    send(out, vec![json!({"status": id})]);
                    Ok(())
                }),
            }
        }
        Err(e) => reply_error(&e),
    };
    if source == "-" {
        return Reply::WithBody(Box::new(move |body| finish(Image::import(body, options))));
    }
    let source = source.to_string();
    Reply::WithBody(Box::new(move |_| {
        finish(Image::import_url(&source, options))
    }))
}

/// `POST /images/create?fromImage=…&tag=…`: pull, streaming progress.
fn create(req: &Request) -> Reply {
    if let Some(source) = req.query.get("fromSrc").filter(|s| !s.is_empty()) {
        return import(req, source);
    }
    let Some(from) = req.query.get("fromImage").filter(|f| !f.is_empty()) else {
        return message(400, "fromImage is required");
    };
    let login = match registry_login(req) {
        Ok(l) => l,
        Err(reply) => return reply,
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
                        "cannot pull {reference} for {p}: podbag pulls images for this machine ({here}) only"
                    ))],
                );
                return Ok(());
            }
            let mut messages = PullMessages::new(&shown, before);
            let result = Image::pull_with_login(&reference, login.as_ref(), |ev| {
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

/// The login of a request's `X-Registry-Auth` header (base64url JSON), if any.
pub fn registry_login(req: &Request) -> Result<Option<AuthConfig>, Reply> {
    match req.header("x-registry-auth") {
        None => Ok(None),
        Some(value) => AuthConfig::from_header(value).map_err(|e| reply_error(&e)),
    }
}

/// `POST /auth`: check a login against its registry, as `docker login` asks. The client
/// keeps it; podbag keeps nothing.
pub fn login(req: &Request) -> Reply {
    let login: AuthConfig = match serde_json::from_slice(&req.body) {
        Ok(l) => l,
        Err(e) => return message(400, format!("invalid login: {e}")),
    };
    let login = login.normalized();
    if login.is_empty() {
        return message(
            400,
            "a user name and a password (or an identity token) are required",
        );
    }
    let registry = crate::image::auth::registry_of(&login.serveraddress);
    let checked = crate::paths::dirs()
        .and_then(|dirs| crate::image::registry::Registry::for_engine(&dirs, Some(login)))
        .and_then(|mut client| client.login(&registry));
    match checked {
        Ok(token) => Reply::Json(
            200,
            json!({"Status": "Login Succeeded", "IdentityToken": token.unwrap_or_default()}),
        ),
        Err(e) if crate::image::registry::is_login_failure(&e) => message(401, error_text(&e)),
        Err(e) => reply_error(&e),
    }
}

/// Turns push events into Docker's JSON progress messages.
pub fn push_messages(ev: &PushEvent) -> Vec<Value> {
    match ev {
        PushEvent::Started { repository, layers } => {
            let mut m = vec![json!({
                "status": format!("The push refers to repository [{repository}]")
            })];
            m.extend(layers.iter().map(|id| layer_id("Preparing", id)));
            m
        }
        PushEvent::LayerExists { id } => vec![layer_id("Layer already exists", id)],
        PushEvent::LayerMounted { id, from } => {
            vec![layer_id(&format!("Mounted from {from}"), id)]
        }
        PushEvent::LayerPushed { id, .. } => vec![layer_id("Pushed", id)],
        PushEvent::Done { tag, digest, size } => vec![
            json!({"status": format!("{tag}: digest: {digest} size: {size}")}),
            json!({"progressDetail": {}, "aux": {"Tag": tag, "Digest": digest, "Size": size}}),
        ],
    }
}

fn layer_id(status: &str, id: &str) -> Value {
    json!({"status": status, "progressDetail": {}, "id": id})
}

/// `POST /images/{name}/push?tag=…`: push, streaming progress. Without a tag, every tag
/// of the repository (`docker push --all-tags`).
fn push(req: &Request, name: &str) -> Reply {
    let login = match registry_login(req) {
        Ok(l) => l,
        Err(reply) => return reply,
    };
    let tag = req.query.get("tag").unwrap_or("");
    let repository = match Reference::parse(name) {
        Ok(r) if r.digest.is_none() && !name.contains('@') => {
            format!("{}/{}", r.registry, r.repository)
        }
        Ok(_) => return message(400, format!("cannot push {name}: give a tag, not a digest")),
        Err(e) => return reply_error(&e),
    };
    let names: Vec<String> = if tag.is_empty() {
        let prefix = format!("{repository}:");
        match Image::list() {
            Ok(list) => list
                .into_iter()
                .map(|i| i.name)
                .filter(|n| n.starts_with(&prefix))
                .collect(),
            Err(e) => return reply_error(&e),
        }
    } else {
        vec![format!("{repository}:{tag}")]
    };
    if names.is_empty() || Image::get(&names[0]).is_err() {
        return message(
            404,
            format!(
                "No such image: {}",
                docker_image_name(names.first().unwrap_or(&repository))
            ),
        );
    }
    Reply::Stream {
        content_type: "application/json",
        body: Box::new(move |out| {
            for name in names {
                let result = Image::push_with_login(&name, login.as_ref(), |ev| {
                    send(out, push_messages(&ev));
                });
                match result {
                    Ok(_) => {
                        let short = docker_image_name(&name);
                        super::events::publish("image", "push", &short, &[("name", short.clone())]);
                    }
                    Err(e) => {
                        send(out, vec![failure(&error_text(&e))]);
                        break;
                    }
                }
            }
            Ok(())
        }),
    }
}

/// `GET /images/search?term=…&limit=…&filters=…`: Docker Hub's search (or the registry's
/// that the term names).
fn search(req: &Request) -> Reply {
    let Some(term) = req.query.get("term").filter(|t| !t.is_empty()) else {
        return message(400, "term is required");
    };
    let filters = match req.query.filters() {
        Ok(f) => f,
        Err(_) => return message(400, "invalid filters"),
    };
    if let Err(reply) = check_filters(&filters, &["is-official", "is-automated", "stars"], &[]) {
        return reply;
    }
    let limit = match req.query.get("limit").filter(|l| !l.is_empty()) {
        None => 25,
        Some(l) => match l.parse::<u32>() {
            Ok(n) if (1..=100).contains(&n) => n,
            _ => return message(400, "limit must be between 1 and 100"),
        },
    };
    let truthy = |key: &str| -> Result<Option<bool>, Reply> {
        match filters.get(key).and_then(|v| v.first()).map(String::as_str) {
            None => Ok(None),
            Some("true" | "1") => Ok(Some(true)),
            Some("false" | "0") => Ok(Some(false)),
            Some(other) => Err(message(400, format!("invalid filter '{key}={other}'"))),
        }
    };
    let (official, automated) = match (truthy("is-official"), truthy("is-automated")) {
        (Ok(o), Ok(a)) => (o, a),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let stars = match filters.get("stars").and_then(|v| v.first()) {
        None => 0,
        Some(s) => match s.parse::<u64>() {
            Ok(n) => n,
            Err(_) => return message(400, format!("invalid filter 'stars={s}'")),
        },
    };
    match crate::image::registry::search(term, limit) {
        Ok(found) => Reply::Json(
            200,
            Value::Array(
                found
                    .into_iter()
                    .filter(|r| official.is_none_or(|o| r.is_official == o))
                    .filter(|r| automated.is_none_or(|a| r.is_automated == a))
                    .filter(|r| r.star_count >= stars)
                    .map(|r| {
                        json!({
                            "name": r.name, "description": r.description,
                            "star_count": r.star_count, "is_official": r.is_official,
                            "is_automated": r.is_automated,
                        })
                    })
                    .collect(),
            ),
        ),
        Err(e) => reply_error(&e),
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
    match Store::open_existing(&ctx.dirs) {
        Some(store) => by_image(&store),
        None => Ok(BTreeMap::new()),
    }
}

fn by_image(store: &Store) -> crate::Result<BTreeMap<String, (Vec<String>, ImageRecord)>> {
    let mut images: BTreeMap<String, (Vec<String>, ImageRecord)> = BTreeMap::new();
    for (name, record, _) in store.list()? {
        images
            .entry(record.digest.clone())
            .or_insert_with(|| (Vec::new(), record))
            .0
            .push(name);
    }
    Ok(images)
}

/// podbag's own field for an image with compressed objects: their size on disk and the
/// plain size, over all the image's files. `None` (no field) for any other image, and
/// when the numbers cannot be read: the listing does not fail for them. No lock is
/// taken: `docker images` never waits for a removal or a freeze.
fn compression_field(store: &Store, digest: &str) -> Option<Value> {
    let stats = store.compression_of(digest).ok()??;
    Some(json!({"Size": stats.stored, "OriginalSize": stats.original}))
}

/// Add `PodbagCompression` to an image's JSON when there is one.
fn with_compression(mut image: Value, compression: Option<Value>) -> Value {
    if let (Some(c), Some(map)) = (compression, image.as_object_mut()) {
        map.insert("PodbagCompression".to_string(), c);
    }
    image
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
    // Images without a name are not listed.
    if wants("dangling", "true") || wants("dangling", "1") {
        return Reply::Json(200, json!([]));
    }
    let Some(store) = Store::open_existing(&ctx.dirs) else {
        return Reply::Json(200, json!([]));
    };
    let images = match by_image(&store) {
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
        if names.is_empty() || !super::router::labels_match(&record.config.labels, &filters) {
            continue;
        }
        let (tags, digests) = repo_names(&names, &digest);
        let compression = compression_field(&store, &digest);
        let image = json!({
            "Id": digest,
            "ParentId": "",
            "RepoTags": tags,
            "RepoDigests": digests,
            "Created": record.created,
            "Size": record.size,
            "SharedSize": -1,
            "VirtualSize": record.size,
            "Labels": record.config.labels,
            "Containers": -1,
        });
        out.push((record.created, with_compression(image, compression)));
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
    let ports: serde_json::Map<String, Value> = c
        .exposed_ports
        .iter()
        .map(|p| (p.clone(), json!({})))
        .collect();
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
        "OnBuild": list(&c.onbuild),
        "Labels": if c.labels.is_empty() { Value::Null } else { json!(c.labels) },
        "ExposedPorts": if ports.is_empty() { Value::Null } else { Value::Object(ports) },
        "StopSignal": c.stop_signal,
        "Shell": list(&c.shell),
    })
}

/// A stored image by name or digest prefix, with all its names.
fn find(ctx: &Ctx, name: &str) -> Result<(ImageRecord, Vec<String>), Reply> {
    let missing = || message(404, format!("No such image: {name}"));
    let store = Store::open_existing(&ctx.dirs).ok_or_else(missing)?;
    find_in(&store, name)
}

fn find_in(store: &Store, name: &str) -> Result<(ImageRecord, Vec<String>), Reply> {
    let missing = || message(404, format!("No such image: {name}"));
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
    let Some(store) = Store::open_existing(&ctx.dirs) else {
        return message(404, format!("No such image: {name}"));
    };
    let (record, names) = match find_in(&store, name) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let compression = compression_field(&store, &record.digest);
    let (tags, digests) = repo_names(&names, &record.digest);
    let image = json!({
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
        "GraphDriver": {"Name": "podbag", "Data": null},
        "RootFS": {"Type": "layers", "Layers": record.diff_ids},
        "Metadata": {"LastTagTime": "0001-01-01T00:00:00Z"},
    });
    Reply::Json(200, with_compression(image, compression))
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

/// `POST /images/prune`: dangling images by default (podbag has none), every unused
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
    // podbag does not prune by label: a label filter removes nothing.
    if filters.contains_key("label") {
        return Reply::Json(200, json!({"ImagesDeleted": null, "SpaceReclaimed": 0}));
    }
    let pruned = if all {
        crate::image::prune()
    } else {
        crate::image::prune_dangling()
    };
    match pruned {
        Ok(report) => {
            let mut deleted: Vec<Value> = report
                .removed_images
                .iter()
                .map(|n| {
                    let short = docker_image_name(n);
                    super::events::publish("image", "untag", &short, &[("name", short.clone())]);
                    json!({ "Untagged": short })
                })
                .collect();
            deleted.extend(report.removed_ids.iter().map(|id| {
                super::events::publish("image", "delete", id, &[]);
                json!({ "Deleted": id })
            }));
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
    fn image_filters_podbag_lacks_are_refused() {
        let keys = |k: &str| BTreeMap::from([(k.to_string(), vec!["x".to_string()])]);
        assert!(check_list_filters(&keys("reference")).is_ok());
        assert!(check_list_filters(&keys("dangling")).is_ok());
        for key in ["before", "since", "until", "bogus"] {
            assert!(check_list_filters(&keys(key)).is_err(), "{key}");
        }
    }

    #[test]
    fn only_a_compressed_image_gets_the_podbag_field() {
        let c = json!({"Size": 1, "OriginalSize": 2});
        assert_eq!(
            with_compression(json!({"Id": "x"}), Some(c.clone())),
            json!({"Id": "x", "PodbagCompression": c})
        );
        assert_eq!(
            with_compression(json!({"Id": "x"}), None),
            json!({"Id": "x"})
        );
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
