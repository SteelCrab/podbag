//! `/containers/…`: create, list, inspect, start, stop, kill, wait, logs, attach, remove,
//! and files in and out (`docker cp`).

use super::create::container_from_create;
use super::docker::{docker_image_name, docker_repo, epoch_of, state_label};
use super::http::{Request, client_gone, peer_closed};
use super::router::{
    Ctx, Reply, check_filters, label_matches, labels_match, message, not_supported, reply_error,
};
use super::shapes::{self, Facts};
use super::stream::{self, Exit, Wanted, frame};
use crate::containers::ContainerInfo;
use crate::network::NetworkMode;
use crate::shim::logbook::{LogFollower, LogLine, Stream};
use crate::spec::ContainerSpec;
use crate::state::{self, Status};
use crate::{ContainerHandle, ErrorKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(100);

/// Routes under `/containers/`; `rest` is the path after it, split at `/`.
pub fn route(req: &Request, ctx: &Ctx, rest: &[&str]) -> Option<Reply> {
    if let Some(reply) = super::manage::route(req, rest) {
        return Some(reply);
    }
    Some(match (req.method.as_str(), rest) {
        ("POST", ["create"]) => create(req, ctx),
        ("GET", ["json"]) => list(req, ctx),
        ("POST", ["prune"]) => prune(req),
        ("GET", [id, "json"]) => inspect(ctx, id),
        ("POST", [id, "start"]) => start(ctx, id),
        ("POST", [id, "stop"]) => stop(req, id),
        ("POST", [id, "restart"]) => restart(req, id),
        ("POST", [id, "kill"]) => kill(req, id),
        ("POST", [id, "wait"]) => wait(req, ctx, id),
        ("DELETE", [id]) => remove(req, id),
        ("GET", [id, "logs"]) => logs(req, ctx, id),
        ("POST", [id, "attach"]) => attach(req, ctx, id),
        ("POST", [id, "resize"]) => resize(req, id),
        ("POST", [id, "exec"]) => super::exec::create(req, ctx, id),
        (_, [_, "attach", "ws"]) => not_supported(
            "attaching over a websocket",
            "use POST /containers/{id}/attach",
        ),
        _ => return None,
    })
}

/// The header that carries a path's details, base64-encoded JSON.
const STAT_HEADER: &str = "X-Docker-Container-Path-Stat";

fn path_param(req: &Request) -> Result<String, Reply> {
    match req.query.get("path") {
        Some(p) if !p.is_empty() => Ok(p.to_string()),
        _ => Err(message(400, "the path parameter is required")),
    }
}

fn stat_header(stat: &crate::archive::PathStat) -> (&'static str, String) {
    let json = serde_json::to_vec(stat).unwrap_or_default();
    (STAT_HEADER, crate::base64::encode(&json))
}

/// `HEAD /containers/{id}/archive?path=`: the path's details, in a header.
pub fn stat_path(req: &Request, id: &str) -> Reply {
    let (c, path) = match find(id).and_then(|c| path_param(req).map(|p| (c, p))) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    match c.stat_path(&path) {
        Ok(stat) => Reply::Headed(vec![stat_header(&stat)], Box::new(Reply::Empty(200))),
        Err(e) => reply_error(&e),
    }
}

/// `GET /containers/{id}/archive?path=`: the path as a tar, its details in a header.
pub fn copy_out(req: &Request, id: &str) -> Reply {
    let (c, path) = match find(id).and_then(|c| path_param(req).map(|p| (c, p))) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    match c.copy_out(&path) {
        Ok((stat, mut tar)) => Reply::Headed(
            vec![stat_header(&stat)],
            Box::new(Reply::Chunked {
                content_type: "application/x-tar",
                body: Box::new(move |w| std::io::copy(&mut tar, w).map(|_| ())),
            }),
        ),
        Err(e) => reply_error(&e),
    }
}

/// `PUT /containers/{id}/archive?path=`: unpack the tar in the body into the directory.
pub fn copy_in(req: &Request, id: &str) -> Reply {
    let (c, path) = match find(id).and_then(|c| path_param(req).map(|p| (c, p))) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let options = crate::archive::PutOptions {
        no_overwrite_dir_non_dir: req.query.bool_param("noOverwriteDirNonDir"),
        copy_uid_gid: req.query.bool_param("copyUIDGID"),
    };
    Reply::WithBody(Box::new(move |body| {
        match c.copy_in(&path, body, &options) {
            Ok(()) => Reply::Empty(200),
            Err(e) => reply_error(&e),
        }
    }))
}

/// A container by name or ID, or a 404 in Docker's words.
fn find(name: &str) -> Result<ContainerHandle, Reply> {
    ContainerHandle::get(name).map_err(|e| match e.kind {
        ErrorKind::NotFound => message(404, format!("No such container: {name}")),
        _ => reply_error(&e),
    })
}

fn status(handle: &ContainerHandle) -> Option<Status> {
    handle.info().ok().map(|i| i.state.status)
}

macro_rules! found {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(reply) => return reply,
        }
    };
}

macro_rules! ok_or_reply {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return reply_error(&e),
        }
    };
}

/// `POST /containers/create?name=…`.
fn create(req: &Request, ctx: &Ctx) -> Reply {
    let name = req.query.get("name").filter(|n| !n.is_empty());
    if let Some(n) = name {
        ok_or_reply!(crate::container::check_name(n.trim_start_matches('/')));
    }
    if let Some(p) = req.query.get("platform").filter(|p| !p.is_empty()) {
        if !super::images::is_this_platform(p) {
            return message(
                400,
                format!("cannot run {p} images: podbag runs images for this machine only"),
            );
        }
    }
    let body = match req.json() {
        Ok(b) => b,
        Err(e) => return message(400, format!("{e:?}")),
    };
    if let Some(root) = &ctx.binds_under {
        for source in ok_or_reply!(super::create::bind_sources(&body)) {
            if !super::create::lies_under(std::path::Path::new(&source), root) {
                return message(
                    400,
                    format!(
                        "mounts denied: {source} is not under {}, the only Mac directory \
                         the VM sees; move it there, or use a named volume",
                        root.display()
                    ),
                );
            }
        }
    }
    let (c, warnings) = ok_or_reply!(container_from_create(&body, name));
    let handle = ok_or_reply!(c.create());
    Reply::Json(201, json!({"Id": handle.id(), "Warnings": warnings}))
}

/// The container's Docker shape (`shapes::list_entry` or `shapes::inspect`).
pub(super) fn shape(
    ctx: &Ctx,
    info: &ContainerInfo,
    spec: &ContainerSpec,
    f: fn(&Facts) -> Value,
) -> Value {
    let alone = |name| shapes::Attached {
        name,
        network: None,
        addresses: None,
        aliases: &[],
    };
    let networks: Vec<shapes::Attached> = match &spec.network {
        NetworkMode::Bridge { .. } => spec
            .bridge_networks()
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let (network, addresses) =
                    match crate::network::with_addresses_of(&ctx.dirs, name, &info.id) {
                        Ok((network, ip)) => (Some(network), ip),
                        Err(_) => (None, None),
                    };
                shapes::Attached {
                    name,
                    network,
                    addresses,
                    aliases: spec.aliases_on(index),
                }
            })
            .collect(),
        NetworkMode::Host => vec![alone("host")],
        NetworkMode::None => vec![alone("none")],
    };
    let dir = ctx.dirs.container_dir(&info.id);
    let volumes = ctx.dirs.data.join("volumes");
    let image = docker_image_name(&info.image);
    f(&Facts {
        spec,
        state: &info.state,
        image: &image,
        mounts: &info.mounts,
        networks: &networks,
        dir: &dir,
        volumes: &volumes,
        now: state::now(),
        restart_count: crate::restart::record(&ctx.dirs, &info.id).count,
        health: info.health.as_ref(),
    })
}

/// Docker's `name` filter is a regular expression on `/name`; the anchors `^` and `$`
/// and plain text cover how clients use it.
fn name_matches(pattern: &str, name: &str) -> bool {
    let (start, p) = match pattern.strip_prefix('^') {
        Some(p) => (true, p),
        None => (false, pattern),
    };
    let (end, p) = match p.strip_suffix('$') {
        Some(p) => (true, p),
        None => (false, p),
    };
    let p = p.trim_start_matches('/');
    match (start, end) {
        (true, true) => name == p,
        (true, false) => name.starts_with(p),
        (false, true) => name.ends_with(p),
        (false, false) => name.contains(p),
    }
}

/// Docker's container filters: labels must all match; other values of one key are
/// alternatives.
fn matches(
    filters: &BTreeMap<String, Vec<String>>,
    info: &ContainerInfo,
    spec: &ContainerSpec,
) -> bool {
    filters.iter().all(|(key, values)| match key.as_str() {
        "label" => values.iter().all(|w| label_matches(w, &spec.labels)),
        "name" => values.iter().any(|v| name_matches(v, &spec.name)),
        "id" => values.iter().any(|v| info.id.starts_with(v.as_str())),
        "status" => values.iter().any(|v| v == state_label(&info.state)),
        "health" => {
            let health = info.health.as_ref().map_or("none", |h| h.status.name());
            values.iter().any(|v| v == health)
        }
        "exited" => values
            .iter()
            .any(|v| info.state.status == Status::Exited && v.parse().ok() == info.state.exit_code),
        "ancestor" => values.iter().any(|v| {
            docker_image_name(&info.image) == *v
                || docker_repo(&info.image) == *v
                || spec.image.as_deref() == Some(v.as_str())
        }),
        _ => true,
    })
}

fn check_list_filters(filters: &BTreeMap<String, Vec<String>>) -> Result<(), Reply> {
    check_filters(
        filters,
        &[
            "label", "name", "id", "status", "exited", "ancestor", "health",
        ],
        &[
            "before",
            "since",
            "volume",
            "network",
            "publish",
            "expose",
            "isolation",
            "is-task",
        ],
    )
}

fn check_prune_filters(filters: &BTreeMap<String, Vec<String>>) -> Result<(), Reply> {
    check_filters(filters, &["until", "label", "label!"], &[])
}

/// `GET /containers/json?all&limit&filters`.
fn list(req: &Request, ctx: &Ctx) -> Reply {
    let filters = match req.query.filters() {
        Ok(f) => f,
        Err(_) => return message(400, "invalid filters"),
    };
    if let Err(reply) = check_list_filters(&filters) {
        return reply;
    }
    let all = req.query.bool_param("all")
        || filters.contains_key("status")
        || filters.contains_key("exited");
    let limit = req.query.int_param("limit").filter(|l| *l > 0);
    let infos = ok_or_reply!(ContainerHandle::list(true));
    let mut out = Vec::new();
    for info in infos {
        if !all && info.state.status != Status::Running {
            continue;
        }
        let Ok(spec) = state::read_config(&ctx.dirs, &info.id) else {
            continue;
        };
        if matches(&filters, &info, &spec) {
            out.push(shape(ctx, &info, &spec, shapes::list_entry));
        }
        if limit.is_some_and(|l| out.len() as i64 >= l) {
            break;
        }
    }
    Reply::Json(200, Value::Array(out))
}

/// `GET /containers/{id}/json`.
fn inspect(ctx: &Ctx, name: &str) -> Reply {
    let handle = found!(find(name));
    let info = ok_or_reply!(handle.info());
    let spec = ok_or_reply!(state::read_config(&ctx.dirs, &info.id));
    Reply::Json(200, shape(ctx, &info, &spec, shapes::inspect))
}

/// `POST /containers/{id}/start`: attached to the clients that wait for it, if any.
fn start(ctx: &Ctx, name: &str) -> Reply {
    let handle = found!(find(name));
    if handle.info().is_ok_and(|i| i.state.paused) {
        return message(409, "cannot start a paused container, try unpause instead");
    }
    if status(&handle) == Some(Status::Running) {
        return Reply::Empty(304);
    }
    let clients = ctx.attachments.take(handle.id());
    let started = if clients.is_empty() {
        handle.start()
    } else {
        stream::start_attached(&ctx.attachments, &handle, &ctx.dirs, clients)
    };
    match started {
        Ok(()) => Reply::Empty(204),
        // Started by someone else in the meantime.
        Err(e) if e.kind == ErrorKind::Conflict && status(&handle) == Some(Status::Running) => {
            Reply::Empty(304)
        }
        Err(e) => reply_error(&e),
    }
}

fn stop_timeout(req: &Request) -> crate::Result<Option<i64>> {
    req.query.get("t").map(str::parse).transpose().map_err(|_| {
        crate::Error::new(
            ErrorKind::InvalidInput,
            "invalid stop timeout",
            "expected an integer",
            "give t in seconds",
        )
    })
}

/// `POST /containers/{id}/stop?t=…`.
fn stop(req: &Request, name: &str) -> Reply {
    let handle = found!(find(name));
    if status(&handle) != Some(Status::Running) {
        return Reply::Empty(304);
    }
    let timeout = ok_or_reply!(stop_timeout(req));
    ok_or_reply!(handle.stop_configured(timeout));
    Reply::Empty(204)
}

/// `POST /containers/{id}/restart?t=…`.
fn restart(req: &Request, name: &str) -> Reply {
    let handle = found!(find(name));
    let timeout = ok_or_reply!(stop_timeout(req));
    ok_or_reply!(handle.restart_configured(timeout));
    Reply::Empty(204)
}

/// `POST /containers/{id}/kill?signal=…`.
fn kill(req: &Request, name: &str) -> Reply {
    let handle = found!(find(name));
    let signal = match req.query.get("signal").filter(|s| !s.is_empty()) {
        None => libc::SIGKILL,
        Some(s) => match crate::signals::parse(s) {
            Some(n) => n,
            None => return message(400, format!("invalid signal: {s}")),
        },
    };
    if status(&handle) != Some(Status::Running) {
        return message(409, format!("Container {} is not running", handle.id()));
    }
    ok_or_reply!(handle.kill(signal));
    Reply::Empty(204)
}

/// `DELETE /containers/{id}?force&v`.
fn remove(req: &Request, name: &str) -> Reply {
    let handle = found!(find(name));
    let force = req.query.bool_param("force");
    let removed = if req.query.bool_param("v") {
        handle.remove_with_volumes(force)
    } else {
        handle.remove(force)
    };
    ok_or_reply!(removed);
    Reply::Empty(204)
}

/// `POST /containers/{id}/resize?h=…&w=…`.
fn resize(req: &Request, name: &str) -> Reply {
    let handle = found!(find(name));
    let size = |key: &str| req.query.int_param(key).and_then(|v| u16::try_from(v).ok());
    let (Some(rows), Some(cols)) = (size("h"), size("w")) else {
        return message(400, "h and w must be numbers");
    };
    ok_or_reply!(handle.resize(rows, cols));
    Reply::Empty(200)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Condition {
    NotRunning,
    NextExit,
    Removed,
}

/// `POST /containers/{id}/wait?condition=…`: the headers go out at once (the docker CLI
/// starts the container only after they arrive), the result when the condition holds.
fn wait(req: &Request, ctx: &Ctx, name: &str) -> Reply {
    let handle = found!(find(name));
    let condition = match req.query.get("condition").unwrap_or("") {
        "" | "not-running" => Condition::NotRunning,
        "next-exit" => Condition::NextExit,
        "removed" => Condition::Removed,
        other => return message(400, format!("invalid condition: {other}")),
    };
    let registry = ctx.attachments.clone();
    let generation = registry.generation(handle.id());
    let first_start = handle.info().ok().and_then(|i| i.state.started);
    Reply::Stream {
        content_type: "application/json",
        body: Box::new(move |out| {
            let gone = |out: &UnixStream| client_gone(out);
            let Some(code) = wait_until(
                &registry,
                &handle,
                generation,
                first_start,
                condition,
                || gone(out),
            ) else {
                return Ok(());
            };
            let body = json!({"StatusCode": code, "Error": null});
            out.write_all(format!("{body}\n").as_bytes())
        }),
    }
}

/// The exit code once `condition` holds; None if the client left first.
fn wait_until(
    registry: &stream::Attachments,
    handle: &ContainerHandle,
    generation: u64,
    first_start: Option<u64>,
    condition: Condition,
    client_left: impl Fn() -> bool,
) -> Option<i32> {
    let id = handle.id();
    let mut code = None;
    let mut last_exit = None;
    let mut ran = false;
    loop {
        // Read before looking: a change made while this looks wakes the wait below.
        let seen = registry.changes();
        // A start by this service records the code even when --rm removes the
        // container before anyone could read it.
        let ours = registry.exit_after(id, generation);
        let pending = ours == Some(Exit::Running);
        if let Some(Exit::Code(c)) = ours {
            code = Some(c);
            ran = true;
        }
        ran |= pending;
        match handle.info().ok().map(|i| i.state) {
            None if !pending => return Some(code.or(last_exit).unwrap_or(0)),
            Some(s) => {
                ran |= s.started.is_some() && s.started != first_start;
                match s.status {
                    Status::Running => {
                        // Its shim answers when it ends; ask a second at a time, so that a
                        // client that left does not keep this thread (and the service).
                        ran = true;
                        match handle.wait_up_to(Duration::from_secs(1)) {
                            Ok(Some(c)) => {
                                code = Some(c);
                                continue;
                            }
                            Ok(None) => {}
                            Err(_) => std::thread::sleep(Duration::from_millis(50)),
                        }
                        if client_left() {
                            return None;
                        }
                        continue;
                    }
                    Status::Exited => {
                        last_exit = s.exit_code;
                        let done = match condition {
                            Condition::NotRunning => true,
                            Condition::NextExit => ran,
                            Condition::Removed => false,
                        };
                        if done && !pending {
                            return Some(code.or(s.exit_code).unwrap_or(0));
                        }
                    }
                    Status::Created => {
                        if condition == Condition::NotRunning && !pending {
                            return Some(0);
                        }
                    }
                }
            }
            None => {}
        }
        if client_left() {
            return None;
        }
        // A start, end or removal by this service wakes it at once; what others do (the
        // CLI, a restart) shows at the next look.
        registry.wait_for_change(seen, Duration::from_millis(50));
    }
}

/// Write log lines as Docker streams them: raw for a terminal, framed otherwise.
fn write_lines(
    out: &mut impl Write,
    lines: &[LogLine],
    wanted: Wanted,
    tty: bool,
    timestamps: bool,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    for l in lines {
        let (stream_id, keep) = match l.stream {
            Stream::Stdout => (1, wanted.stdout),
            Stream::Stderr => (2, wanted.stderr),
        };
        if !keep {
            continue;
        }
        let text = if timestamps {
            format!("{} {}", l.time, l.log)
        } else {
            l.log.clone()
        };
        if tty {
            buf.extend_from_slice(text.as_bytes());
        } else {
            buf.extend(frame(stream_id, text.as_bytes()));
        }
    }
    if !buf.is_empty() {
        out.write_all(&buf)?;
        out.flush()?;
    }
    Ok(())
}

fn content_type(tty: bool) -> &'static str {
    if tty {
        "application/vnd.docker.raw-stream"
    } else {
        "application/vnd.docker.multiplexed-stream"
    }
}

/// A time parameter: Unix seconds, maybe with a fraction.
fn time_param(req: &Request, key: &str) -> Option<f64> {
    req.query
        .get(key)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
}

/// `GET /containers/{id}/logs?stdout&stderr&follow&timestamps&tail&since&until`.
fn logs(req: &Request, ctx: &Ctx, name: &str) -> Reply {
    let handle = found!(find(name));
    let spec = ok_or_reply!(state::read_config(&ctx.dirs, handle.id()));
    let wanted = Wanted {
        stdin: false,
        stdout: req.query.bool_param("stdout"),
        stderr: req.query.bool_param("stderr"),
    };
    if !wanted.stdout && !wanted.stderr {
        return message(400, "Bad parameters: you must choose at least one stream");
    }
    let follow = req.query.bool_param("follow");
    let timestamps = req.query.bool_param("timestamps");
    let tail = match req.query.get("tail") {
        None | Some("" | "all") => None,
        Some(t) => t.parse::<usize>().ok(),
    };
    let since = time_param(req, "since");
    let until = time_param(req, "until");
    let tty = spec.tty;
    Reply::Stream {
        content_type: content_type(tty),
        body: Box::new(move |out| {
            let Ok(mut follower) = handle.follow_logs() else {
                return Ok(());
            };
            let in_range = |l: &LogLine| {
                let t = epoch_of(&l.time).unwrap_or(0.0);
                since.is_none_or(|s| t >= s) && until.is_none_or(|u| t <= u)
            };
            let selected = |f: &mut LogFollower| -> Vec<LogLine> {
                f.next_lines()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|l| in_range(l))
                    .filter(|l| match l.stream {
                        Stream::Stdout => wanted.stdout,
                        Stream::Stderr => wanted.stderr,
                    })
                    .collect()
            };
            let first = selected(&mut follower);
            let skip = tail.map_or(0, |n| first.len().saturating_sub(n));
            write_lines(out, &first[skip..], wanted, tty, timestamps)?;
            if !follow {
                return Ok(());
            }
            loop {
                let lines = selected(&mut follower);
                if !lines.is_empty() {
                    write_lines(out, &lines, wanted, tty, timestamps)?;
                    continue;
                }
                let past_until = until.is_some_and(|u| state::now() as f64 > u);
                if past_until || status(&handle) != Some(Status::Running) {
                    // Everything it wrote before it ended.
                    let rest = selected(&mut follower);
                    return write_lines(out, &rest, wanted, tty, timestamps);
                }
                if client_gone(out) {
                    return Ok(());
                }
                std::thread::sleep(POLL);
            }
        }),
    }
}

/// Relay a running container's log to an attached client until it ends or leaves.
fn follow(
    sock: &mut UnixStream,
    mut follower: LogFollower,
    handle: &ContainerHandle,
    wanted: Wanted,
    tty: bool,
) {
    loop {
        let lines = follower.next_lines().unwrap_or_default();
        if !lines.is_empty() {
            if write_lines(sock, &lines, wanted, tty, false).is_err() {
                return;
            }
            continue;
        }
        if status(handle) != Some(Status::Running) {
            let rest = follower.next_lines().unwrap_or_default();
            let _ = write_lines(sock, &rest, wanted, tty, false);
            return;
        }
        if peer_closed(sock) {
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// `POST /containers/{id}/attach?stream&stdin&stdout&stderr&logs`: the connection becomes
/// the container's stream. A container that is not running waits for this service's
/// start; a running one (or one started elsewhere) is followed through its log.
fn attach(req: &Request, ctx: &Ctx, name: &str) -> Reply {
    let handle = found!(find(name));
    let spec = ok_or_reply!(state::read_config(&ctx.dirs, handle.id()));
    let wanted = Wanted {
        stdin: req.query.bool_param("stdin"),
        stdout: req.query.bool_param("stdout"),
        stderr: req.query.bool_param("stderr"),
    };
    let live = req.query.bool_param("stream");
    let history = req.query.bool_param("logs");
    let tty = spec.tty;
    let registry = ctx.attachments.clone();
    Reply::Hijack {
        content_type: content_type(tty),
        run: Box::new(move |mut sock, pending, head| {
            let id = handle.id().to_string();
            let mut follower = handle.follow_logs().ok();
            let old = follower
                .as_mut()
                .map(|f| f.next_lines().unwrap_or_default())
                .unwrap_or_default();
            let send_head = |s: &mut UnixStream| -> std::io::Result<()> {
                head.send(s)?;
                if history {
                    write_lines(s, &old, wanted, tty, false)?;
                }
                Ok(())
            };
            if !live {
                let _ = send_head(&mut sock);
                return;
            }
            if status(&handle) != Some(Status::Running) {
                let Ok(copy) = sock.try_clone() else {
                    return;
                };
                let (done_tx, done_rx) = mpsc::channel();
                let Some(token) = registry.add(&id, copy, wanted, pending, done_tx, send_head)
                else {
                    return;
                };
                loop {
                    match done_rx.recv_timeout(POLL) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if !registry.is_waiting(&id, token) {
                        continue; // a start took it: its relay owns the connection now
                    }
                    if peer_closed(&sock) || status(&handle).is_none() {
                        if registry.withdraw(&id, token) {
                            return;
                        }
                        continue;
                    }
                    // Started by another process: follow its log instead.
                    if status(&handle) == Some(Status::Running) && registry.withdraw(&id, token) {
                        break;
                    }
                }
            } else if send_head(&mut sock).is_err() {
                return;
            }
            if let Some(f) = follower {
                follow(&mut sock, f, &handle, wanted, tty);
            }
        }),
    }
}

/// `POST /containers/prune?filters`: every container that is not running.
fn prune(req: &Request) -> Reply {
    let filters = match req.query.filters() {
        Ok(f) => f,
        Err(_) => return message(400, "invalid filters"),
    };
    if let Err(reply) = check_prune_filters(&filters) {
        return reply;
    }
    let until = match filters.get("until").and_then(|v| v.first()) {
        Some(u) => match until_param(u) {
            Some(t) => Some(t),
            None => return message(400, format!("invalid until filter: {u}")),
        },
        None => None,
    };
    let infos = ok_or_reply!(ContainerHandle::list(true));
    let mut deleted = Vec::new();
    for info in infos {
        if info.state.status == Status::Running {
            continue;
        }
        if until.is_some_and(|u| info.state.created >= u) {
            continue;
        }
        let labels = crate::paths::dirs()
            .ok()
            .and_then(|d| state::read_config(&d, &info.id).ok())
            .map(|s| s.labels)
            .unwrap_or_default();
        if !labels_match(&labels, &filters) {
            continue;
        }
        if ContainerHandle::get(&info.id)
            .and_then(|h| h.remove(false))
            .is_ok()
        {
            deleted.push(info.id);
        }
    }
    let deleted = if deleted.is_empty() {
        Value::Null
    } else {
        json!(deleted)
    };
    Reply::Json(
        200,
        json!({"ContainersDeleted": deleted, "SpaceReclaimed": 0}),
    )
}

/// `until`: Unix seconds, or a duration back from now (`24h`, `30m`, `10s`).
fn until_param(v: &str) -> Option<u64> {
    if let Ok(t) = v.parse::<u64>() {
        return Some(t);
    }
    let (num, scale) = [("h", 3600u64), ("m", 60), ("s", 1)]
        .into_iter()
        .find_map(|(suffix, scale)| v.strip_suffix(suffix).map(|n| (n, scale)))?;
    let secs = num.parse::<u64>().ok()?.checked_mul(scale)?;
    Some(state::now().saturating_sub(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_filters_use_anchors() {
        assert!(name_matches("web", "my-web-1"));
        assert!(name_matches("^/web$", "web"));
        assert!(!name_matches("^/web$", "web2"));
        assert!(name_matches("^web", "web2"));
        assert!(name_matches("1$", "web1"));
    }

    fn keys(keys: &[&str]) -> BTreeMap<String, Vec<String>> {
        keys.iter()
            .map(|k| (k.to_string(), vec!["x".to_string()]))
            .collect()
    }

    #[test]
    fn filters_podbag_lacks_are_refused_not_ignored() {
        assert!(check_list_filters(&keys(&["label", "name", "id", "status"])).is_ok());
        // Ignoring these would select every container: `docker rm $(docker ps -aq
        // --filter volume=x)` would remove them all.
        for key in ["volume", "network", "before", "since", "publish", "bogus"] {
            assert!(check_list_filters(&keys(&[key])).is_err(), "{key}");
        }
        assert!(check_prune_filters(&keys(&["label", "label!", "until"])).is_ok());
        assert!(check_prune_filters(&keys(&["name"])).is_err());
    }

    #[test]
    fn until_takes_seconds_or_durations() {
        assert_eq!(until_param("1700000000"), Some(1_700_000_000));
        let ago = until_param("1h").unwrap();
        assert!(state::now() - ago >= 3599);
        assert_eq!(until_param("1d"), None);
        assert_eq!(until_param(""), None);
        // A multibyte last character, and numbers too big to multiply.
        assert_eq!(until_param("1é"), None);
        assert_eq!(until_param("é"), None);
        assert_eq!(until_param("99999999999999999999h"), None);
        assert_eq!(until_param("9999999999999999h"), None);
    }
}
