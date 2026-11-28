//! The swarm's routes of the Docker Engine API: `/swarm`, `/nodes`, `/services`,
//! `/tasks`, `/secrets` and `/configs`.

use super::ops::{Engine, host};
use super::remote;
use super::shapes::{self, Counts};
use super::store::Kind;
use crate::api::http::{Request, client_gone};
use crate::api::router::{
    Ctx, Reply, check_filters, labels_match, message, not_supported, reply_error,
};
use crate::{Error, ErrorKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

type Filters = BTreeMap<String, Vec<String>>;

macro_rules! ok_or_reply {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return reply(&e),
        }
    };
}

macro_rules! found {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

/// An error as Docker answers it; a missing swarm is 503, as Docker's.
fn reply(e: &Error) -> Reply {
    if e.what.starts_with("This node is not a swarm manager") {
        return not_manager();
    }
    match e.kind {
        // Docker's own words for these, without podbag's hints.
        ErrorKind::NotFound | ErrorKind::Conflict | ErrorKind::InvalidInput => {
            let status = match e.kind {
                ErrorKind::NotFound => 404,
                ErrorKind::Conflict => 409,
                _ => 400,
            };
            message(status, format!("{}: {}", e.what, e.why))
        }
        _ => reply_error(e),
    }
}

fn not_manager() -> Reply {
    message(
        503,
        "This node is not a swarm manager. Use \"docker swarm init\" or \"docker swarm join\" to connect this node to swarm and try again.",
    )
}

fn body(req: &Request) -> Result<Value, Reply> {
    req.json().map_err(|e| message(400, format!("{e:?}")))
}

fn filters(req: &Request, supported: &[&str]) -> Result<Filters, Reply> {
    let f = req
        .query
        .filters()
        .map_err(|_| message(400, "invalid filters"))?;
    check_filters(&f, supported, &[])?;
    Ok(f)
}

/// Any value of filter `key` matches (no such filter: everything).
fn any(f: &Filters, key: &str, ok: impl Fn(&str) -> bool) -> bool {
    f.get(key).is_none_or(|values| values.iter().any(|v| ok(v)))
}

fn version(req: &Request) -> Option<u64> {
    req.query.get("version").and_then(|v| v.parse().ok())
}

/// The engine, with the swarm active (503 otherwise).
fn engine(ctx: &Ctx) -> Result<(Engine, super::store::Cluster), Reply> {
    let engine = Engine::new(&ctx.dirs);
    match engine.cluster() {
        Ok(Some(c)) => Ok((engine, c)),
        Ok(None) => Err(not_manager()),
        Err(e) => Err(reply(&e)),
    }
}

/// The Swarm part of `/info`.
pub fn info(ctx: &Ctx) -> Value {
    let engine = Engine::new(&ctx.dirs);
    let cluster = engine.cluster().ok().flatten();
    let mut output = shapes::info_swarm(cluster.as_ref());
    if cluster.is_some() {
        let workers = engine.store.workers().unwrap_or_default();
        output["Nodes"] = json!(1 + workers.iter().filter(|w| w.host.is_some()).count());
    } else if let Ok(Some(worker)) = engine.store.membership() {
        output["NodeID"] = json!(worker.node_id);
        output["LocalNodeState"] = json!("active");
    }
    output
}

/// Routes under the swarm's paths; `segments` is the whole path, split at `/`.
pub fn route(req: &Request, ctx: &Ctx, segments: &[&str]) -> Option<Reply> {
    let method = req.method.as_str();
    Some(match (method, segments) {
        ("POST", ["swarm", "init"]) => init(req, ctx),
        ("POST", ["swarm", "join"]) => not_supported(
            "joining another swarm",
            "register a Podbag worker over SSH with podbag swarm worker-add",
        ),
        ("POST", ["swarm", "podbag", "worker"]) => worker_request(req, ctx),
        ("GET", ["swarm"]) => match engine(ctx) {
            Ok((_, c)) => Reply::Json(200, shapes::swarm_json(&c)),
            Err(r) => r,
        },
        ("POST", ["swarm", "leave"]) => leave(req, ctx),
        ("POST", ["swarm", "update"]) => update_swarm(req, ctx),
        ("GET", ["swarm", "unlockkey"]) => match engine(ctx) {
            Ok(_) => Reply::Json(200, json!({"UnlockKey": ""})),
            Err(r) => r,
        },
        (_, ["swarm", "unlock"]) => not_supported("locking a swarm", "podbag does not lock it"),
        ("GET", ["nodes"]) => nodes(req, ctx),
        ("GET", ["nodes", id]) => node(ctx, id),
        ("POST", ["nodes", id, "update"]) => update_node(req, ctx, id),
        ("DELETE", ["nodes", id]) => remove_node(ctx, id),
        ("GET", ["services"]) => services(req, ctx),
        ("POST", ["services", "create"]) => create_service(req, ctx),
        ("GET", ["services", id]) => service(ctx, id),
        ("POST", ["services", id, "update"]) => update_service(req, ctx, id),
        ("DELETE", ["services", id]) => remove_service(ctx, id),
        ("GET", ["services", id, "logs"]) => logs(req, ctx, Of::Service(id.to_string())),
        ("GET", ["tasks"]) => task_list(req, ctx),
        ("GET", ["tasks", id]) => task(ctx, id),
        ("GET", ["tasks", id, "logs"]) => logs(req, ctx, Of::Task(id.to_string())),
        ("GET", ["secrets"]) => objects(req, ctx, Kind::Secret),
        ("POST", ["secrets", "create"]) => create_object(req, ctx, Kind::Secret),
        ("GET", ["secrets", id]) => object(ctx, Kind::Secret, id),
        ("POST", ["secrets", id, "update"]) => update_object(req, ctx, Kind::Secret, id),
        ("DELETE", ["secrets", id]) => remove_object(ctx, Kind::Secret, id),
        ("GET", ["configs"]) => objects(req, ctx, Kind::Config),
        ("POST", ["configs", "create"]) => create_object(req, ctx, Kind::Config),
        ("GET", ["configs", id]) => object(ctx, Kind::Config, id),
        ("POST", ["configs", id, "update"]) => update_object(req, ctx, Kind::Config, id),
        ("DELETE", ["configs", id]) => remove_object(ctx, Kind::Config, id),
        _ => return None,
    })
}

fn worker_request(req: &Request, ctx: &Ctx) -> Reply {
    let body = found!(body(req));
    let request = match serde_json::from_value(body) {
        Ok(request) => request,
        Err(err) => return message(400, format!("invalid Podbag worker request: {err}")),
    };
    match remote::handle(&Engine::new(&ctx.dirs), request) {
        Ok(value) => Reply::Json(200, value),
        Err(e) => reply(&e),
    }
}

// ---- the swarm ----

fn init(req: &Request, ctx: &Ctx) -> Reply {
    let body = found!(body(req));
    let engine = Engine::new(&ctx.dirs);
    match engine.init(&body) {
        Ok(node) => Reply::Json(200, Value::from(node)),
        Err(e) if e.kind == ErrorKind::Conflict => message(
            503,
            "This node is already part of a swarm. Use \"docker swarm leave\" to leave this swarm and join another one.",
        ),
        Err(e) => reply(&e),
    }
}

fn leave(req: &Request, ctx: &Ctx) -> Reply {
    let engine = Engine::new(&ctx.dirs);
    match engine.cluster() {
        Ok(Some(_)) => {}
        Ok(None) => return message(503, "This node is not part of a swarm"),
        Err(e) => return reply(&e),
    }
    if !req.query.bool_param("force") {
        return message(
            503,
            "You are attempting to leave the swarm on a node that is participating as a manager. \
             Removing the last manager erases all current state of the swarm. Use `--force` to ignore this message.",
        );
    }
    ok_or_reply!(engine.leave());
    Reply::Empty(200)
}

fn update_swarm(req: &Request, ctx: &Ctx) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let body = found!(body(req));
    ok_or_reply!(engine.update_swarm(
        version(req),
        &body,
        req.query.bool_param("rotateWorkerToken"),
        req.query.bool_param("rotateManagerToken"),
    ));
    Reply::Empty(200)
}

// ---- nodes ----

fn nodes(req: &Request, ctx: &Ctx) -> Reply {
    let (engine, c) = found!(engine(ctx));
    let f = found!(filters(
        req,
        &["id", "name", "role", "membership", "label", "node.label"]
    ));
    let host = host();
    let node = shapes::node_json(&c, &host);
    let labels: BTreeMap<String, String> = c.node.spec["Labels"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    let mut node_labels = Filters::new();
    if let Some(v) = f.get("node.label") {
        node_labels.insert("label".into(), v.clone());
    }
    let keep = any(&f, "id", |v| c.node_id.starts_with(v))
        && any(&f, "name", |v| host.hostname.contains(v))
        && any(&f, "role", |v| v == "manager")
        && any(&f, "membership", |v| v == "accepted")
        && labels_match(&labels, &node_labels)
        && f.get("label").is_none_or(|_| {
            // Engine labels: podbag has none.
            false
        });
    let mut out = if keep { vec![node] } else { Vec::new() };
    if any(&f, "role", |v| v == "worker")
        && any(&f, "membership", |v| v == "accepted")
        && !f.contains_key("node.label")
        && !f.contains_key("label")
    {
        for worker in ok_or_reply!(engine.store.workers()) {
            if any(&f, "id", |v| worker.id.starts_with(v))
                && any(&f, "name", |v| worker.name.contains(v))
            {
                out.push(shapes::worker_json(&worker, worker.ready(&c)));
            }
        }
    }
    Reply::Json(200, Value::Array(out))
}

fn node(ctx: &Ctx, id: &str) -> Reply {
    let (engine, c) = found!(engine(ctx));
    let host = host();
    if id == c.node_id
        || id == "self"
        || id == host.hostname
        || (id.len() >= 3 && c.node_id.starts_with(id))
    {
        Reply::Json(200, shapes::node_json(&c, &host))
    } else if let Some(worker) = ok_or_reply!(remote::find(&engine, id)) {
        Reply::Json(200, shapes::worker_json(&worker, worker.ready(&c)))
    } else {
        message(404, format!("node {id} not found"))
    }
}

fn update_node(req: &Request, ctx: &Ctx, id: &str) -> Reply {
    let (engine, c) = found!(engine(ctx));
    if !(id == c.node_id || id == host().hostname || (id.len() >= 3 && c.node_id.starts_with(id))) {
        if let Some(worker) = ok_or_reply!(remote::find(&engine, id)) {
            let body = found!(body(req));
            ok_or_reply!(remote::update_availability(
                &engine,
                &worker.id,
                version(req),
                &body
            ));
            return Reply::Empty(200);
        }
        return message(404, format!("node {id} not found"));
    }
    let body = found!(body(req));
    ok_or_reply!(engine.update_node(version(req), &body));
    Reply::Empty(200)
}

fn remove_node(ctx: &Ctx, id: &str) -> Reply {
    let (engine, cluster) = found!(engine(ctx));
    if id == cluster.node_id
        || id == "self"
        || id == host().hostname
        || (id.len() >= 3 && cluster.node_id.starts_with(id))
    {
        return message(
            400,
            "cannot remove the only manager: leave the swarm with docker swarm leave --force",
        );
    }
    ok_or_reply!(remote::remove(&engine, id));
    Reply::Empty(200)
}

// ---- services ----

/// Bind mounts must come from under the shared directory in the macOS VM.
fn check_binds(ctx: &Ctx, spec: &Value) -> Result<(), Reply> {
    let Some(roots) = &ctx.binds_under else {
        return Ok(());
    };
    let mounts = &spec["TaskTemplate"]["ContainerSpec"]["Mounts"];
    let body = json!({"HostConfig": {"Mounts": mounts}});
    let sources = crate::api::create::bind_sources(&body).map_err(|e| reply(&e))?;
    for source in sources {
        crate::api::create::check_shared_bind(&source, roots)?;
    }
    Ok(())
}

fn mode_name(spec: &Value) -> &'static str {
    match super::plan::mode_of(spec) {
        Some(super::plan::Mode::Global) => "global",
        _ => "replicated",
    }
}

fn labels_of(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn services(req: &Request, ctx: &Ctx) -> Reply {
    let (engine, c) = found!(engine(ctx));
    let f = found!(filters(req, &["id", "name", "label", "mode"]));
    let _ = engine.reconcile();
    let all = ok_or_reply!(engine.store.services());
    let counts: Option<BTreeMap<String, Counts>> = req.query.bool_param("status").then(|| {
        let tasks = engine.task_list().unwrap_or_default();
        all.iter()
            .map(|s| {
                let mine = tasks
                    .iter()
                    .map(|(t, ..)| t)
                    .filter(|t| t.service_id == s.id);
                let local = remote::placement(s, &c).ok() == Some(remote::Placement::Local);
                let desired = match super::plan::mode_of(&s.spec) {
                    _ if local && !c.active() => 0,
                    Some(super::plan::Mode::Replicated(n)) => n,
                    _ => 1,
                };
                let running = mine.clone().filter(|t| t.state == "running").count() as u64;
                let completed = mine.filter(|t| t.state == "complete").count() as u64;
                (
                    s.id.clone(),
                    Counts {
                        running,
                        desired,
                        completed,
                    },
                )
            })
            .collect()
    });
    let out: Vec<Value> = all
        .iter()
        .filter(|s| any(&f, "id", |v| s.id.starts_with(v)))
        .filter(|s| any(&f, "name", |v| s.name().starts_with(v)))
        .filter(|s| any(&f, "mode", |v| v == mode_name(&s.spec)))
        .filter(|s| labels_match(&labels_of(&s.spec["Labels"]), &f))
        .map(|s| {
            let count = counts.as_ref().and_then(|c| c.get(&s.id).copied());
            shapes::service_json(s, &super::net::vips_json(s), count)
        })
        .collect();
    Reply::Json(200, Value::Array(out))
}

fn service(ctx: &Ctx, id: &str) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let s = ok_or_reply!(engine.find_service(id));
    if super::update::active(&s).is_some() && !super::update::running(&s.id) {
        let _ = engine.resume_pinned_update(&s.id);
    }
    Reply::Json(
        200,
        shapes::service_json(&s, &super::net::vips_json(&s), None),
    )
}

fn create_service(req: &Request, ctx: &Ctx) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let body = found!(body(req));
    found!(check_binds(ctx, &body));
    let (id, warnings) = ok_or_reply!(engine.create_service(&body));
    Reply::Json(201, json!({"ID": id, "Warnings": warnings}))
}

fn update_service(req: &Request, ctx: &Ctx, id: &str) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let body = found!(body(req));
    found!(check_binds(ctx, &body));
    let rollback = req.query.get("rollback") == Some("previous");
    let warnings = ok_or_reply!(engine.update_service(id, version(req), &body, rollback));
    Reply::Json(200, json!({"Warnings": warnings}))
}

fn remove_service(ctx: &Ctx, id: &str) -> Reply {
    let (engine, _) = found!(engine(ctx));
    ok_or_reply!(engine.remove_service(id));
    Reply::Empty(200)
}

// ---- tasks ----

fn task_list(req: &Request, ctx: &Ctx) -> Reply {
    let (engine, c) = found!(engine(ctx));
    let f = found!(filters(
        req,
        &[
            "id",
            "name",
            "service",
            "node",
            "label",
            "desired-state",
            "_up-to-date"
        ]
    ));
    let _ = engine.reconcile();
    let services = ok_or_reply!(engine.store.services());
    let all = ok_or_reply!(engine.task_list());
    let workers = ok_or_reply!(engine.store.workers());
    let hostname = host().hostname;
    let out: Vec<Value> = all
        .iter()
        .filter(|(t, ..)| any(&f, "id", |v| t.id.starts_with(v)))
        .filter(|(t, _, _, service)| {
            any(&f, "name", |v| {
                let short = format!("{service}.{}", t.slot);
                t.name.starts_with(v) || short.starts_with(v)
            })
        })
        .filter(|(t, ..)| {
            any(&f, "service", |v| {
                t.service_id.starts_with(v)
                    || services
                        .iter()
                        .any(|s| s.id == t.service_id && s.name() == v)
            })
        })
        .filter(|(t, ..)| {
            any(&f, "node", |v| {
                t.node_id.starts_with(v)
                    || (t.node_id == c.node_id && (v == hostname || v == "self"))
                    || workers
                        .iter()
                        .any(|worker| worker.id == t.node_id && worker.name == v)
            })
        })
        .filter(|(t, ..)| any(&f, "desired-state", |v| v == t.desired_state))
        .filter(|(_, labels, ..)| labels_match(labels, &f))
        .filter(|(_, _, up, _)| any(&f, "_up-to-date", |v| (v == "true" || v == "1") == *up))
        .map(|(t, ..)| shapes::task_json(t))
        .collect();
    Reply::Json(200, Value::Array(out))
}

fn task(ctx: &Ctx, id: &str) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let all = ok_or_reply!(engine.task_list());
    let matching: Vec<_> = all
        .iter()
        .filter(|(t, ..)| t.id == id || (!id.is_empty() && t.id.starts_with(id)))
        .collect();
    match matching.as_slice() {
        [(t, ..)] => Reply::Json(200, shapes::task_json(t)),
        [] => message(404, format!("task {id} not found")),
        _ => message(400, format!("task {id} is ambiguous")),
    }
}

// ---- logs ----

enum Of {
    Service(String),
    Task(String),
}

fn logs(req: &Request, ctx: &Ctx, of: Of) -> Reply {
    use crate::Stream;
    let (engine, c) = found!(engine(ctx));
    let stdout = req.query.bool_param("stdout");
    let stderr = req.query.bool_param("stderr");
    if !stdout && !stderr {
        return message(400, "Bad parameters: you must choose at least one stream");
    }
    let (service_id, only_task, tty) = match &of {
        Of::Service(key) => {
            let s = ok_or_reply!(engine.find_service(key));
            if ok_or_reply!(remote::placement(&s, &c)) != remote::Placement::Local {
                return not_supported(
                    "streaming logs from SSH workers",
                    "read logs on that worker",
                );
            }
            let tty = s.spec["TaskTemplate"]["ContainerSpec"]["TTY"]
                .as_bool()
                .unwrap_or(false);
            (s.id, None, tty)
        }
        Of::Task(key) => {
            let all = super::tasks::list(&ctx.dirs, &c.node_id);
            let Some(t) = all.iter().find(|t| {
                t.task_id == *key || (!key.is_empty() && t.task_id.starts_with(key.as_str()))
            }) else {
                if ok_or_reply!(engine.task_list())
                    .iter()
                    .any(|(t, ..)| t.id.starts_with(key))
                {
                    return not_supported(
                        "streaming logs from SSH workers",
                        "read logs on that worker",
                    );
                }
                return message(404, format!("task {key} not found"));
            };
            let tty = crate::state::read_config(&ctx.dirs, &t.container_id).is_ok_and(|s| s.tty);
            (t.service_id.clone(), Some(t.task_id.clone()), tty)
        }
    };
    let follow = req.query.bool_param("follow");
    let timestamps = req.query.bool_param("timestamps");
    let details = req.query.bool_param("details");
    let tail = match req.query.get("tail") {
        None | Some("" | "all") => None,
        Some(t) => t.parse::<usize>().ok(),
    };
    let since = req
        .query
        .get("since")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0);
    let dirs = ctx.dirs.clone();
    let node = c.node_id.clone();
    Reply::Stream {
        content_type: if tty {
            "application/vnd.docker.raw-stream"
        } else {
            "application/vnd.docker.multiplexed-stream"
        },
        body: Box::new(move |out| {
            use std::io::Write;
            let mut followers: BTreeMap<String, (crate::LogFollower, String)> = BTreeMap::new();
            let mut first = true;
            loop {
                let tasks: Vec<super::tasks::Task> = super::tasks::list(&dirs, &node)
                    .into_iter()
                    .filter(|t| t.service_id == service_id)
                    .filter(|t| only_task.as_ref().is_none_or(|id| *id == t.task_id))
                    .collect();
                let mut lines: Vec<(String, crate::LogLine)> = Vec::new();
                for t in &tasks {
                    if !followers.contains_key(&t.container_id) {
                        let handle = crate::ContainerHandle::from_id(t.container_id.clone());
                        if let Ok(f) = handle.follow_logs() {
                            followers.insert(t.container_id.clone(), (f, t.task_id.clone()));
                        }
                    }
                    let Some((f, task_id)) = followers.get_mut(&t.container_id) else {
                        continue;
                    };
                    let mut mine: Vec<crate::LogLine> = f
                        .next_lines()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|l| match l.stream {
                            Stream::Stdout => stdout,
                            Stream::Stderr => stderr,
                        })
                        .filter(|l| {
                            since.is_none_or(|s| {
                                crate::api::docker::epoch_of(&l.time).unwrap_or(0.0) >= s
                            })
                        })
                        .collect();
                    if let (true, Some(n)) = (first, tail) {
                        let skip = mine.len().saturating_sub(n);
                        mine.drain(..skip);
                    }
                    lines.extend(mine.into_iter().map(|l| (task_id.clone(), l)));
                }
                first = false;
                lines.sort_by(|a, b| a.1.time.cmp(&b.1.time));
                let mut buf = Vec::new();
                for (task_id, l) in &lines {
                    let mut text = String::new();
                    if timestamps {
                        text.push_str(&l.time);
                        text.push(' ');
                    }
                    if details {
                        text.push_str(&format!(
                            "com.docker.swarm.node.id={node},com.docker.swarm.service.id={service_id},com.docker.swarm.task.id={task_id} "
                        ));
                    }
                    text.push_str(&l.log);
                    if tty {
                        buf.extend_from_slice(text.as_bytes());
                    } else {
                        let id = match l.stream {
                            Stream::Stdout => 1,
                            Stream::Stderr => 2,
                        };
                        buf.extend(crate::api::stream::frame(id, text.as_bytes()));
                    }
                }
                if !buf.is_empty() {
                    out.write_all(&buf)?;
                    out.flush()?;
                }
                if !follow || client_gone(out) {
                    return Ok(());
                }
                if lines.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }),
    }
}

// ---- secrets and configs ----

fn objects(req: &Request, ctx: &Ctx, kind: Kind) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let f = found!(filters(req, &["id", "name", "names", "label"]));
    let all = ok_or_reply!(engine.store.objects(kind));
    let out: Vec<Value> = all
        .iter()
        .filter(|o| any(&f, "id", |v| o.id.starts_with(v)))
        .filter(|o| any(&f, "name", |v| o.name().starts_with(v)))
        .filter(|o| any(&f, "names", |v| o.name() == v))
        .filter(|o| labels_match(&labels_of(&o.spec["Labels"]), &f))
        .map(|o| {
            let data = (kind == Kind::Config)
                .then(|| engine.store.object_data(kind, &o.id).ok())
                .flatten();
            shapes::object_json(kind, o, data.as_deref())
        })
        .collect();
    Reply::Json(200, Value::Array(out))
}

fn object(ctx: &Ctx, kind: Kind, id: &str) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let o = ok_or_reply!(engine.find_object(kind, id));
    let data = (kind == Kind::Config)
        .then(|| engine.store.object_data(kind, &o.id).ok())
        .flatten();
    Reply::Json(200, shapes::object_json(kind, &o, data.as_deref()))
}

fn create_object(req: &Request, ctx: &Ctx, kind: Kind) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let body = found!(body(req));
    let id = ok_or_reply!(engine.create_object(kind, &body));
    Reply::Json(201, json!({"ID": id}))
}

fn update_object(req: &Request, ctx: &Ctx, kind: Kind, id: &str) -> Reply {
    let (engine, _) = found!(engine(ctx));
    let body = found!(body(req));
    ok_or_reply!(engine.update_object(kind, id, version(req), &body));
    Reply::Empty(200)
}

fn remove_object(ctx: &Ctx, kind: Kind, id: &str) -> Reply {
    let (engine, _) = found!(engine(ctx));
    ok_or_reply!(engine.remove_object(kind, id));
    Reply::Empty(204)
}

// ---- networks ----

/// `POST /networks/create` with the `overlay` driver: a bridge network marked as an
/// overlay network, while the swarm is active. Returns None for other drivers.
pub fn create_network(ctx: &Ctx, body: &Value) -> Option<Reply> {
    if body["Driver"].as_str() != Some("overlay") {
        return None;
    }
    let (engine, _) = match engine(ctx) {
        Ok(found) => found,
        Err(r) => return Some(r),
    };
    let Some(name) = body["Name"].as_str().filter(|n| !n.is_empty()) else {
        return Some(message(400, "the network needs a Name"));
    };
    let subnet = body["IPAM"]["Config"][0]["Subnet"]
        .as_str()
        .filter(|s| !s.is_empty());
    let labels: crate::network::Labels = labels_of(&body["Labels"]);
    let attachable = body["Attachable"].as_bool().unwrap_or(false);
    Some(
        match engine.create_overlay(name, subnet, labels, attachable) {
            Ok(n) => {
                let id = n.docker_id();
                crate::api::events::publish(
                    "network",
                    "create",
                    &id,
                    &[("name", n.name.clone()), ("type", "overlay".to_string())],
                );
                Reply::Json(201, json!({"Id": id, "Warning": ""}))
            }
            Err(e) => reply(&e),
        },
    )
}
