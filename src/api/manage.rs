//! `/containers/{id}/…`: `stats`, `top`, `pause`, `unpause`, `rename` and `update`.

use super::http::{Request, client_gone};
use super::router::{Reply, message, reply_error};
use crate::containers::stats::Sample;
use crate::containers::update::{Host, Update};
use crate::state::Status;
use crate::{ContainerHandle, ContainerInfo, ErrorKind};
use serde_json::{Value, json};
use std::io::Write;
use std::time::Duration;

/// Time between two samples, as Docker's collector takes them.
const INTERVAL: Duration = Duration::from_secs(1);

/// Routes under `/containers/{id}/`; None for others.
pub fn route(req: &Request, rest: &[&str]) -> Option<Reply> {
    Some(match (req.method.as_str(), rest) {
        ("GET", [id, "stats"]) => stats(req, id),
        ("GET", [id, "top"]) => top(req, id),
        ("POST", [id, "pause"]) => pause(id, true),
        ("POST", [id, "unpause"]) => pause(id, false),
        ("POST", [id, "rename"]) => rename(req, id),
        ("POST", [id, "update"]) => update(req, id),
        _ => return None,
    })
}

/// The container and its details, or Docker's 404.
fn find(name: &str) -> Result<(ContainerHandle, ContainerInfo), Reply> {
    let missing = || message(404, format!("No such container: {name}"));
    let handle = ContainerHandle::get(name).map_err(|e| match e.kind {
        ErrorKind::NotFound => missing(),
        _ => reply_error(&e),
    })?;
    let info = handle.info().map_err(|e| match e.kind {
        ErrorKind::NotFound => missing(),
        _ => reply_error(&e),
    })?;
    Ok((handle, info))
}

macro_rules! found {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(reply) => return reply,
        }
    };
}

/// A container event of this service, with the attributes Docker gives. `what`: the
/// change as the event poller would see it, if it can.
fn event(info: &ContainerInfo, action: &str, extra: &[(&str, String)], what: Option<&str>) {
    let labels = crate::paths::dirs()
        .ok()
        .and_then(|d| crate::state::read_config(&d, &info.id).ok())
        .map(|s| s.labels)
        .unwrap_or_default();
    let mut attributes: Vec<(&str, String)> = labels
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    attributes.push(("name", info.name.clone()));
    attributes.push(("image", info.image.clone()));
    for (key, value) in extra {
        attributes.retain(|(k, _)| k != key);
        attributes.push((key, value.clone()));
    }
    match what {
        Some(what) => super::events::publish_container(action, &info.id, &attributes, what),
        None => super::events::publish("container", action, &info.id, &attributes),
    }
}

fn not_running(id: &str) -> Reply {
    message(409, format!("Container {id} is not running"))
}

/// `GET /containers/{id}/stats?stream&one-shot`: one sample, or one a second.
fn stats(req: &Request, name: &str) -> Reply {
    let (handle, info) = found!(find(name));
    let stream = !matches!(req.query.get("stream"), Some("0" | "false" | "False"));
    let one_shot = req.query.bool_param("one-shot");
    let (id, name) = (info.id.clone(), info.name.clone());
    if !stream {
        let first = match handle.stats() {
            Ok(s) => s,
            Err(e) => return reply_error(&e),
        };
        // Like Docker: without one-shot, wait for a second sample, so the CPU use of the
        // time between them can be computed.
        let (previous, now) = if one_shot || first.read_ns == 0 {
            (None, first)
        } else {
            std::thread::sleep(INTERVAL);
            match handle.stats() {
                Ok(second) => (Some(first), second),
                Err(e) => return reply_error(&e),
            }
        };
        return Reply::Json(200, now.docker_json(previous.as_ref(), &id, &name));
    }
    Reply::Stream {
        content_type: "application/json",
        body: Box::new(move |out| {
            let mut previous: Option<Sample> = None;
            loop {
                let Ok(now) = handle.stats() else {
                    // Removed: the stream ends.
                    return Ok(());
                };
                let body = now.docker_json(previous.as_ref(), &id, &name);
                out.write_all(format!("{body}\n").as_bytes())?;
                out.flush()?;
                previous = (now.read_ns != 0).then_some(now);
                std::thread::sleep(INTERVAL);
                if client_gone(out) {
                    return Ok(());
                }
            }
        }),
    }
}

/// `GET /containers/{id}/top?ps_args=`.
fn top(req: &Request, name: &str) -> Reply {
    let (handle, info) = found!(find(name));
    if info.state.status != Status::Running {
        return not_running(&info.id);
    }
    let args: Vec<String> = req
        .query
        .get("ps_args")
        .unwrap_or("")
        .split_whitespace()
        .map(String::from)
        .collect();
    match handle.top(&args) {
        Ok(top) => {
            event(&info, "top", &[], None);
            Reply::Json(200, json!(top))
        }
        Err(e) if e.kind == ErrorKind::InvalidInput => message(400, e.why),
        Err(e) if e.kind == ErrorKind::Conflict => not_running(&info.id),
        Err(e) => reply_error(&e),
    }
}

/// `POST /containers/{id}/pause` and `…/unpause`.
fn pause(name: &str, freeze: bool) -> Reply {
    let (handle, info) = found!(find(name));
    let id = &info.id;
    if info.state.status != Status::Running {
        return not_running(id);
    }
    if freeze && info.state.paused {
        return message(409, format!("Container {id} is already paused"));
    }
    if !freeze && !info.state.paused {
        return message(409, format!("Container {id} is not paused"));
    }
    let done = if freeze {
        handle.pause()
    } else {
        handle.unpause()
    };
    match done {
        Ok(()) => {
            let action = if freeze { "pause" } else { "unpause" };
            event(&info, action, &[], Some(action));
            Reply::Empty(204)
        }
        Err(e) => reply_error(&e),
    }
}

/// `POST /containers/{id}/rename?name=`.
fn rename(req: &Request, name: &str) -> Reply {
    let (handle, info) = found!(find(name));
    let new_name = req.query.get("name").unwrap_or("");
    match handle.rename(new_name) {
        Ok(()) => {
            let new_name = new_name.trim_start_matches('/').to_string();
            let extra = [
                ("name", new_name.clone()),
                ("oldName", format!("/{}", info.name)),
            ];
            event(&info, "rename", &extra, Some(&format!("rename:{new_name}")));
            Reply::Empty(204)
        }
        // Docker's words: the name in use, an empty or unchanged name, an invalid one.
        Err(e) if e.kind == ErrorKind::Conflict => message(409, format!("{}. {}.", e.what, e.why)),
        Err(e) if e.kind == ErrorKind::InvalidInput && e.what.starts_with("Invalid") => {
            message(400, format!("{}, {}", e.what, e.why))
        }
        Err(e) if e.kind == ErrorKind::InvalidInput => message(400, e.why),
        Err(e) => reply_error(&e),
    }
}

/// Fields of Docker's update body that podbag does not apply.
const IGNORED: [&str; 19] = [
    "KernelMemory",
    "KernelMemoryTCP",
    "CpuRealtimePeriod",
    "CpuRealtimeRuntime",
    "CpuCount",
    "CpuPercent",
    "BlkioWeightDevice",
    "BlkioDeviceReadBps",
    "BlkioDeviceWriteBps",
    "BlkioDeviceReadIOps",
    "BlkioDeviceWriteIOps",
    "MemorySwappiness",
    "OomKillDisable",
    "Devices",
    "DeviceCgroupRules",
    "DeviceRequests",
    "Ulimits",
    "IOMaximumIOps",
    "IOMaximumBandwidth",
];

/// Docker's update body as an [`Update`], with warnings for fields podbag ignores.
pub fn update_from_docker(body: &Value) -> Result<(Update, Vec<String>), String> {
    let int = |key: &str| body[key].as_i64().unwrap_or(0);
    let text = |key: &str| body[key].as_str().unwrap_or("").to_string();
    let policy = &body["RestartPolicy"];
    let restart = match policy["Name"].as_str().unwrap_or("") {
        "" => None,
        name => Some(
            crate::restart::RestartPolicy::from_docker(
                name,
                policy["MaximumRetryCount"].as_i64().unwrap_or(0),
            )
            .map_err(|e| e.what)?,
        ),
    };
    let update = Update {
        memory: int("Memory"),
        memory_swap: int("MemorySwap"),
        memory_reservation: int("MemoryReservation"),
        nano_cpus: int("NanoCpus"),
        cpu_quota: int("CpuQuota"),
        cpu_period: int("CpuPeriod"),
        cpu_shares: int("CpuShares"),
        cpuset_cpus: text("CpusetCpus"),
        cpuset_mems: text("CpusetMems"),
        pids_limit: body["PidsLimit"].as_i64(),
        blkio_weight: int("BlkioWeight"),
        restart,
    };
    let set = |v: &Value| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    };
    let warnings = IGNORED
        .iter()
        .filter(|key| set(&body[**key]))
        .map(|key| format!("{key} is ignored by podbag"))
        .collect();
    Ok((update, warnings))
}

/// `POST /containers/{id}/update`.
fn update(req: &Request, name: &str) -> Reply {
    let (handle, info) = found!(find(name));
    let body = match req.json() {
        Ok(b) => b,
        Err(e) => return message(400, format!("{e:?}")),
    };
    let (update, mut warnings) = match update_from_docker(&body) {
        Ok(u) => u,
        Err(why) => return message(400, why),
    };
    // The checks of the values alone answer in Docker's words, without a prefix.
    if let Err(why) = update.check(&Host::this()) {
        return message(400, why);
    }
    match handle.update(&update) {
        Ok(more) => {
            warnings.extend(more);
            event(&info, "update", &[], None);
            Reply::Json(200, json!({ "Warnings": warnings }))
        }
        Err(e) if e.kind == ErrorKind::InvalidInput && e.what.starts_with("Cannot update") => {
            message(400, format!("{}: {}", e.what, e.why))
        }
        Err(e) => reply_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_update_body_as_docker_sends_it() {
        let body = json!({
            "BlkioWeight": 0, "CpuShares": 512, "Memory": 134217728, "MemorySwap": -1,
            "NanoCpus": 0, "CpuQuota": 0, "CpuPeriod": 0, "CpusetCpus": "0", "CpusetMems": "",
            "PidsLimit": null, "KernelMemory": 0, "Devices": [], "Ulimits": null,
            "MemorySwappiness": null,
            "RestartPolicy": {"Name": "on-failure", "MaximumRetryCount": 3}
        });
        let (u, warnings) = update_from_docker(&body).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(u.memory, 128 << 20);
        assert_eq!(u.memory_swap, -1);
        assert_eq!(u.cpu_shares, 512);
        assert_eq!(u.cpuset_cpus, "0");
        assert_eq!(u.pids_limit, None);
        assert_eq!(
            u.restart,
            Some(crate::restart::RestartPolicy::OnFailure { max: 3 })
        );
        // No policy: unchanged. Fields podbag lacks: warnings.
        let (u, warnings) = update_from_docker(&json!({
            "RestartPolicy": {"Name": ""}, "PidsLimit": 0, "KernelMemory": 1024,
            "Devices": [{"PathOnHost": "/dev/x"}]
        }))
        .unwrap();
        assert_eq!(u.restart, None);
        assert_eq!(u.pids_limit, Some(0));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(update_from_docker(&json!({"RestartPolicy": {"Name": "sometimes"}})).is_err());
    }
}
