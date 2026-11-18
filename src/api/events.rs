//! `GET /events`: container lifecycle, found by watching the containers' state files, and
//! the image, volume and network actions of this service. There is no daemon, so there is
//! no history: a stream starts with what happens after the request.

use super::http::{Request, client_gone};
use super::router::{Reply, message};
use crate::state::Status;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_millis(250);

static SUBSCRIBERS: Mutex<Vec<mpsc::Sender<Value>>> = Mutex::new(Vec::new());

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn event(kind: &str, action: &str, id: &str, attributes: &BTreeMap<String, String>) -> Value {
    let t = now();
    json!({
        "Type": kind,
        "Action": action,
        "Actor": {"ID": id, "Attributes": attributes},
        "scope": "local",
        "time": t.as_secs(),
        "timeNano": t.as_nanos() as u64,
        "status": action,
        "id": id,
        "from": attributes.get("image").cloned().unwrap_or_default(),
    })
}

/// Tell every open event stream about an action of this service (images, volumes,
/// networks). Containers are seen through their state files.
pub fn publish(kind: &str, action: &str, id: &str, attributes: &[(&str, String)]) {
    let attributes: BTreeMap<String, String> = attributes
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    let ev = event(kind, action, id, &attributes);
    let mut subscribers = SUBSCRIBERS.lock().unwrap_or_else(|p| p.into_inner());
    subscribers.retain(|s| s.send(ev.clone()).is_ok());
}

#[derive(Clone, PartialEq)]
struct Seen {
    status: Status,
    started: Option<u64>,
    name: String,
    image: String,
    exit_code: Option<i32>,
    labels: BTreeMap<String, String>,
}

fn check(filters: &BTreeMap<String, Vec<String>>) -> Result<(), Reply> {
    super::router::check_filters(
        filters,
        &[
            "type",
            "event",
            "container",
            "image",
            "label",
            "network",
            "volume",
            "scope",
        ],
        &["daemon", "plugin", "node", "service", "secret", "config"],
    )
}

/// The containers now. `labels` keeps each container's labels (fixed at create), so each
/// config is read once.
fn snapshot(labels: &mut HashMap<String, BTreeMap<String, String>>) -> HashMap<String, Seen> {
    let list = crate::ContainerHandle::list(true).unwrap_or_default();
    let ids: std::collections::HashSet<&str> = list.iter().map(|c| c.id.as_str()).collect();
    labels.retain(|id, _| ids.contains(id.as_str()));
    let dirs = crate::paths::dirs().ok();
    list.into_iter()
        .map(|c| {
            let own = labels
                .entry(c.id.clone())
                .or_insert_with(|| {
                    dirs.as_ref()
                        .and_then(|d| crate::state::read_config(d, &c.id).ok())
                        .map(|s| s.labels)
                        .unwrap_or_default()
                })
                .clone();
            (
                c.id.clone(),
                Seen {
                    status: c.state.status,
                    started: c.state.started,
                    name: c.name,
                    image: c.image,
                    exit_code: c.state.exit_code,
                    labels: own,
                },
            )
        })
        .collect()
}

/// Container events between two snapshots, in the order they happened.
fn changes(before: &HashMap<String, Seen>, after: &HashMap<String, Seen>) -> Vec<Value> {
    let mut out = Vec::new();
    // Like Docker: the container's labels, then its name and image.
    let attrs = |s: &Seen, code: bool| {
        let mut a = s.labels.clone();
        a.insert("name".to_string(), s.name.clone());
        a.insert("image".to_string(), s.image.clone());
        if code {
            a.insert(
                "exitCode".to_string(),
                s.exit_code.unwrap_or(-1).to_string(),
            );
        }
        a
    };
    for (id, now) in after {
        let ran = now.started.is_some();
        match before.get(id) {
            None => {
                out.push(event("container", "create", id, &attrs(now, false)));
                if ran {
                    out.push(event("container", "start", id, &attrs(now, false)));
                }
                if now.status == Status::Exited && ran {
                    out.push(event("container", "die", id, &attrs(now, true)));
                }
            }
            Some(was) => {
                let restarted = now.started != was.started && ran;
                if was.status == Status::Running && (now.status != Status::Running || restarted) {
                    out.push(event("container", "die", id, &attrs(was, true)));
                }
                if restarted {
                    out.push(event("container", "start", id, &attrs(now, false)));
                }
                if now.status == Status::Exited && (was.status != Status::Exited || restarted) {
                    out.push(event("container", "die", id, &attrs(now, true)));
                }
            }
        }
    }
    for (id, was) in before {
        if !after.contains_key(id) {
            if was.status == Status::Running {
                out.push(event("container", "die", id, &attrs(was, true)));
            }
            out.push(event("container", "destroy", id, &attrs(was, false)));
        }
    }
    out
}

fn wanted(ev: &Value, filters: &BTreeMap<String, Vec<String>>) -> bool {
    let field = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let attrs = &ev["Actor"]["Attributes"];
    let label = |want: &str| match want.split_once('=') {
        Some((k, v)) => attrs[k].as_str() == Some(v),
        None => attrs.get(want).is_some(),
    };
    filters.iter().all(|(key, values)| {
        // Labels must all match; other values of one key are alternatives.
        if key == "label" {
            return values.iter().all(|want| label(want));
        }
        values.is_empty()
            || values.iter().any(|want| match key.as_str() {
                "type" => field(ev, "Type") == *want,
                "event" => field(ev, "Action") == *want,
                "container" => {
                    field(ev, "Type") == "container"
                        && (field(ev, "id").starts_with(want.as_str())
                            || attrs["name"].as_str() == Some(want.as_str()))
                }
                "image" => attrs["image"].as_str() == Some(want.as_str()),
                "network" => {
                    field(ev, "Type") == "network"
                        && (field(ev, "id").starts_with(want.as_str())
                            || attrs["name"].as_str() == Some(want.as_str()))
                }
                "volume" => field(ev, "Type") == "volume" && field(ev, "id") == *want,
                "scope" => want == "local",
                _ => false,
            })
    })
}

pub fn stream(req: &Request) -> Reply {
    let filters = match req.query.filters() {
        Ok(f) => f,
        Err(_) => return message(400, "invalid filters"),
    };
    if let Err(reply) = check(&filters) {
        return reply;
    }
    let until = req.query.int_param("until").filter(|u| *u > 0);
    let (tx, rx) = mpsc::channel();
    SUBSCRIBERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(tx);
    Reply::Stream {
        content_type: "application/json",
        body: Box::new(move |out| {
            let mut labels = HashMap::new();
            let mut known = snapshot(&mut labels);
            loop {
                let mut batch: Vec<Value> = rx.try_iter().collect();
                let current = snapshot(&mut labels);
                batch.extend(changes(&known, &current));
                known = current;
                for ev in batch.iter().filter(|e| wanted(e, &filters)) {
                    let mut line = serde_json::to_vec(ev).unwrap_or_default();
                    line.push(b'\n');
                    out.write_all(&line)?;
                }
                out.flush()?;
                if until.is_some_and(|u| now().as_secs() >= u as u64) || client_gone(out) {
                    return Ok(());
                }
                std::thread::sleep(POLL);
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(status: Status, started: Option<u64>) -> Seen {
        Seen {
            status,
            started,
            name: "n".into(),
            image: "i".into(),
            exit_code: Some(0),
            labels: BTreeMap::new(),
        }
    }

    fn filter(key: &str, value: &str) -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([(key.to_string(), vec![value.to_string()])])
    }

    #[test]
    fn container_events_carry_labels_and_filter_by_them() {
        let mut s = seen(Status::Created, None);
        s.labels = BTreeMap::from([("app".to_string(), "x".to_string())]);
        let evs = changes(&HashMap::new(), &HashMap::from([("c1".to_string(), s)]));
        assert_eq!(evs[0]["Actor"]["Attributes"]["app"], "x");
        assert_eq!(evs[0]["Actor"]["Attributes"]["name"], "n");
        assert!(wanted(&evs[0], &filter("label", "app=x")));
        assert!(wanted(&evs[0], &filter("label", "app")));
        assert!(!wanted(&evs[0], &filter("label", "app=y")));
        assert!(!wanted(&evs[0], &filter("label", "other")));
    }

    #[test]
    fn unknown_event_filters_are_refused() {
        assert!(check(&filter("type", "container")).is_ok());
        assert!(check(&filter("label", "a")).is_ok());
        assert!(check(&filter("bogus", "a")).is_err());
        assert!(check(&filter("daemon", "a")).is_err());
    }

    fn actions(before: &[(&str, Seen)], after: &[(&str, Seen)]) -> Vec<String> {
        let map = |l: &[(&str, Seen)]| -> HashMap<String, Seen> {
            l.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
        };
        changes(&map(before), &map(after))
            .iter()
            .map(|e| e["Action"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn lifecycles_between_polls_keep_their_order() {
        assert_eq!(
            actions(&[], &[("a", seen(Status::Running, Some(1)))]),
            ["create", "start"]
        );
        assert_eq!(
            actions(&[], &[("a", seen(Status::Exited, Some(1)))]),
            ["create", "start", "die"]
        );
        assert_eq!(
            actions(&[("a", seen(Status::Running, Some(1)))], &[]),
            ["die", "destroy"]
        );
        assert_eq!(
            actions(
                &[("a", seen(Status::Exited, Some(1)))],
                &[("a", seen(Status::Running, Some(5)))]
            ),
            ["start"]
        );
        assert_eq!(
            actions(
                &[("a", seen(Status::Running, Some(1)))],
                &[("a", seen(Status::Running, Some(5)))]
            ),
            ["die", "start"]
        );
        assert!(
            actions(
                &[("a", seen(Status::Created, None))],
                &[("a", seen(Status::Created, None))]
            )
            .is_empty()
        );
    }
}
