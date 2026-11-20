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
/// Whether the poller runs: one thread looks at the containers for every open stream.
static POLLER: Mutex<bool> = Mutex::new(false);
/// Pollers that run now (one at most).
static POLLERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Open a stream: its events arrive here (`Value::Null` now and then: the poller looks
/// whether the stream is still open).
fn subscribe() -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    SUBSCRIBERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(tx);
    let mut running = POLLER.lock().unwrap_or_else(|p| p.into_inner());
    if !*running {
        *running = true;
        POLLERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::spawn(poll_containers);
    }
    rx
}

/// Send `ev` to every open stream; streams that closed go.
fn broadcast(ev: &Value) {
    let mut subscribers = SUBSCRIBERS.lock().unwrap_or_else(|p| p.into_inner());
    subscribers.retain(|s| s.send(ev.clone()).is_ok());
}

/// The poller: container events from the changes of their state files, for all streams.
/// It ends when no stream is left; the next stream starts it again.
fn poll_containers() {
    let mut cache = HashMap::new();
    let mut known = snapshot(&mut cache);
    loop {
        std::thread::sleep(POLL);
        {
            let mut subscribers = SUBSCRIBERS.lock().unwrap_or_else(|p| p.into_inner());
            subscribers.retain(|s| s.send(Value::Null).is_ok());
            if subscribers.is_empty() {
                *POLLER.lock().unwrap_or_else(|p| p.into_inner()) = false;
                POLLERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        }
        let current = snapshot(&mut cache);
        for ev in changes(&known, &current) {
            broadcast(&ev);
        }
        known = current;
    }
}

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

/// Container changes this service announced itself (a pause and an unpause can come
/// faster than the poller looks): (ID, what) for the poller to skip once.
static ANNOUNCED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Whether the poller should skip a change: this service announced it already.
fn announced(id: &str, what: &str) -> bool {
    let mut list = ANNOUNCED.lock().unwrap_or_else(|p| p.into_inner());
    match list.iter().position(|(i, w)| i == id && w == what) {
        Some(at) => {
            list.remove(at);
            true
        }
        None => false,
    }
}

/// Tell every open stream about a container change this service made (`pause`,
/// `unpause`, `rename`), at once; `what` names the change for the poller, which sees it
/// in the state files later (or never, when it is undone first): `pause`, `unpause`, or
/// `rename:<new name>`.
pub fn publish_container(action: &str, id: &str, attributes: &[(&str, String)], what: &str) {
    remember(id, what);
    publish("container", action, id, attributes);
}

/// Note a change this service announced, for the poller to skip. The newest of one kind
/// wins: a pause that the poller never saw is forgotten at the unpause.
fn remember(id: &str, what: &str) {
    let mut list = ANNOUNCED.lock().unwrap_or_else(|p| p.into_inner());
    let kind = |w: &str| {
        match w.split(':').next().unwrap_or(w) {
            "unpause" => "pause",
            k => k,
        }
        .to_string()
    };
    list.retain(|(i, w)| !(i == id && kind(w) == kind(what)));
    list.push((id.to_string(), what.to_string()));
}

/// Tell every open event stream about an action of this service (images, volumes,
/// networks). Containers are seen through their state files.
pub fn publish(kind: &str, action: &str, id: &str, attributes: &[(&str, String)]) {
    let attributes: BTreeMap<String, String> = attributes
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    broadcast(&event(kind, action, id, &attributes));
}

#[derive(Clone, PartialEq)]
struct Seen {
    status: Status,
    started: Option<u64>,
    name: String,
    image: String,
    exit_code: Option<i32>,
    labels: BTreeMap<String, String>,
    /// Its health while it runs, when it has a check.
    health: Option<crate::health::Status>,
    /// Frozen by `pause`.
    paused: bool,
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

/// The containers now. `labels` keeps each container's labels and whether it has a health
/// check (both fixed at create), so each config is read once.
fn snapshot(
    labels: &mut HashMap<String, (BTreeMap<String, String>, bool)>,
) -> HashMap<String, Seen> {
    let list = crate::ContainerHandle::list(true).unwrap_or_default();
    let ids: std::collections::HashSet<&str> = list.iter().map(|c| c.id.as_str()).collect();
    labels.retain(|id, _| ids.contains(id.as_str()));
    let dirs = crate::paths::dirs().ok();
    list.into_iter()
        .map(|c| {
            let (own, checked) = labels
                .entry(c.id.clone())
                .or_insert_with(|| {
                    dirs.as_ref()
                        .and_then(|d| crate::state::read_config(d, &c.id).ok())
                        .map(|s| (s.labels, s.health.is_some()))
                        .unwrap_or_default()
                })
                .clone();
            let health = (checked && c.state.status == Status::Running).then(|| {
                dirs.as_ref()
                    .and_then(|d| crate::health::read(d, &c.id))
                    .unwrap_or_default()
                    .status
            });
            (
                c.id.clone(),
                Seen {
                    status: c.state.status,
                    started: c.state.started,
                    name: c.name,
                    image: c.image,
                    exit_code: c.state.exit_code,
                    labels: own,
                    health,
                    paused: c.state.paused,
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
        // A check's result that changed the health (a start's "starting" is no event).
        let health = now
            .health
            .filter(|h| *h != crate::health::Status::Starting)
            .filter(|h| before.get(id).and_then(|was| was.health) != Some(*h));
        if let Some(h) = health {
            out.push(event(
                "container",
                &format!("health_status: {}", h.name()),
                id,
                &attrs(now, false),
            ));
        }
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
                if was.name != now.name {
                    let mut a = attrs(now, false);
                    a.insert("oldName".to_string(), format!("/{}", was.name));
                    if !announced(id, &format!("rename:{}", now.name)) {
                        out.push(event("container", "rename", id, &a));
                    }
                }
                let restarted = now.started != was.started && ran;
                let was_running = was.status == Status::Running;
                // Paused or continued within one run (an end continues it too).
                let paused_now = now.paused && now.status == Status::Running;
                if !restarted && was.paused != paused_now && (paused_now || was_running) {
                    let action = if paused_now { "pause" } else { "unpause" };
                    if !announced(id, action) {
                        out.push(event("container", action, id, &attrs(now, false)));
                    }
                }
                // The run it was in ended.
                if was_running && (now.status != Status::Running || restarted) {
                    out.push(event("container", "die", id, &attrs(was, true)));
                }
                if restarted {
                    out.push(event("container", "start", id, &attrs(now, false)));
                }
                // A run that began after the last look has ended too (the end of the run
                // it was in has its event above).
                if now.status == Status::Exited
                    && (restarted || (!was_running && was.status != Status::Exited))
                {
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
                // As Docker: these three match the actions that begin with them.
                "event"
                    if matches!(
                        want.as_str(),
                        "health_status" | "exec_create" | "exec_start"
                    ) =>
                {
                    field(ev, "Action").starts_with(want.as_str())
                }
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
    let rx = subscribe();
    Reply::Stream {
        content_type: "application/json",
        body: Box::new(move |out| {
            loop {
                let first = match rx.recv_timeout(POLL) {
                    Ok(ev) => Some(ev),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                };
                let batch = first.into_iter().chain(rx.try_iter());
                for ev in batch.filter(|e| !e.is_null() && wanted(e, &filters)) {
                    let mut line = serde_json::to_vec(&ev).unwrap_or_default();
                    line.push(b'\n');
                    out.write_all(&line)?;
                }
                out.flush()?;
                if until.is_some_and(|u| now().as_secs() >= u as u64) || client_gone(out) {
                    return Ok(());
                }
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
            health: None,
            paused: false,
        }
    }

    fn filter(key: &str, value: &str) -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([(key.to_string(), vec![value.to_string()])])
    }

    #[test]
    fn a_run_that_ends_gives_one_die() {
        let id = "c1".to_string();
        let running = HashMap::from([(id.clone(), seen(Status::Running, Some(5)))]);
        let exited = HashMap::from([(id.clone(), seen(Status::Exited, Some(5)))]);
        let actions: Vec<String> = changes(&running, &exited)
            .iter()
            .map(|e| e["Action"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(actions, ["die"]);
        // A new run that began and ended between the two looks: the old run's end, the
        // start, and the new run's end.
        let again = HashMap::from([(id, seen(Status::Exited, Some(9)))]);
        let actions: Vec<String> = changes(&running, &again)
            .iter()
            .map(|e| e["Action"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(actions, ["die", "start", "die"]);
    }

    #[test]
    fn one_poller_serves_every_stream() {
        let running = || POLLERS.load(std::sync::atomic::Ordering::SeqCst);
        let first = subscribe();
        let second = subscribe();
        assert_eq!(running(), 1);
        // Both hear what the service does.
        publish("volume", "create", "v1", &[]);
        for rx in [&first, &second] {
            let got = rx.iter().find(|ev| !ev.is_null()).expect("an event");
            assert_eq!(got["Action"], "create");
        }
        // The last stream gone, the poller ends.
        drop((first, second));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while running() > 0 {
            assert!(std::time::Instant::now() < deadline, "the poller stayed");
            std::thread::sleep(Duration::from_millis(20));
        }
        // And a new stream starts it again.
        let third = subscribe();
        assert_eq!(running(), 1);
        drop(third);
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
    fn pauses_and_renames_are_events() {
        let running = seen(Status::Running, Some(1));
        let mut paused = running.clone();
        paused.paused = true;
        assert_eq!(
            actions(&[("a", running.clone())], &[("a", paused.clone())]),
            ["pause"]
        );
        assert_eq!(
            actions(&[("a", paused.clone())], &[("a", running.clone())]),
            ["unpause"]
        );
        // A paused container that is killed: it continues, then ends.
        assert_eq!(
            actions(&[("a", paused)], &[("a", seen(Status::Exited, Some(1)))]),
            ["unpause", "die"]
        );
        let mut renamed = running.clone();
        renamed.name = "m".into();
        let evs = changes(
            &HashMap::from([("a".to_string(), running)]),
            &HashMap::from([("a".to_string(), renamed)]),
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["Action"], "rename");
        assert_eq!(evs[0]["Actor"]["Attributes"]["name"], "m");
        assert_eq!(evs[0]["Actor"]["Attributes"]["oldName"], "/n");
    }

    #[test]
    fn what_the_service_announced_is_not_told_twice() {
        let running = seen(Status::Running, Some(1));
        let mut paused = running.clone();
        paused.paused = true;
        let one = |id: &str, a: &Seen, b: &Seen| {
            changes(
                &HashMap::from([(id.to_string(), a.clone())]),
                &HashMap::from([(id.to_string(), b.clone())]),
            )
            .len()
        };
        remember("announced-1", "pause");
        assert_eq!(one("announced-1", &running, &paused), 0);
        // Once only: a later pause the service did not make is told.
        assert_eq!(one("announced-1", &running, &paused), 1);
        // A pause and an unpause the poller never saw: the unpause is the one left.
        remember("announced-2", "pause");
        remember("announced-2", "unpause");
        assert_eq!(one("announced-2", &running, &paused), 1);
        assert_eq!(one("announced-2", &paused, &running), 0);
        let mut renamed = running.clone();
        renamed.name = "m".into();
        remember("announced-3", "rename:m");
        assert_eq!(one("announced-3", &running, &renamed), 0);
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
