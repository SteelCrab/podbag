//! Rolling updates and rollbacks. An update call stores the new spec and starts a
//! thread here that replaces the outdated tasks a batch at a time (`Parallelism`), stop
//! first or start first (`Order`), waits `Delay` between batches, and watches each new
//! task for `Monitor`: too many failures (`MaxFailureRatio`) pause the update, let it
//! go on, or roll it back (`FailureAction`). The docker CLI follows it through the
//! service's `UpdateStatus` and its tasks. The API service does not exit while a thread
//! runs here.

use super::ops::Engine;
use super::plan::{self, FailureAction, Order, Slot, UpdatePolicy};
use super::store::{ServiceRecord, now};
use super::tasks::{self, Task};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Services with an update thread.
static RUNNING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
/// Update threads that have not ended.
static THREADS: AtomicUsize = AtomicUsize::new(0);

const POLL: Duration = Duration::from_millis(100);

/// Whether an update runs in this process: the API service then stays.
pub fn busy() -> bool {
    THREADS.load(Ordering::SeqCst) > 0
}

/// Whether service `id` is in a rolling update (reconcile leaves it alone).
pub fn running(id: &str) -> bool {
    RUNNING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(id)
}

/// The service was removed: its update thread ends at its next step.
pub fn forget(id: &str) {
    RUNNING.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
}

/// Where an update is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Started,
    Paused,
    Completed,
}

/// Docker's `UpdateStatus`.
pub fn status(rollback: bool, stage: Stage, message: &str) -> Value {
    let state = match (rollback, stage) {
        (false, Stage::Started) => "updating",
        (false, Stage::Paused) => "paused",
        (false, Stage::Completed) => "completed",
        (true, Stage::Started) => "rollback_started",
        (true, Stage::Paused) => "rollback_paused",
        (true, Stage::Completed) => "rollback_completed",
    };
    let message = match (message, rollback, stage) {
        ("", false, Stage::Started) => "update in progress",
        ("", true, Stage::Started) => "rollback in progress",
        ("", false, Stage::Completed) => "update completed",
        ("", true, Stage::Completed) => "rollback completed",
        (m, ..) => m,
    };
    let mut v = json!({"State": state, "StartedAt": now(), "Message": message});
    if stage == Stage::Completed {
        v["CompletedAt"] = Value::from(now());
    }
    v
}

/// Start the update thread of service `id`, unless one runs: that one takes the new
/// spec at its next step. Called with the store's lock held.
pub fn spawn(engine: Engine, id: String, rollback: bool) {
    if !RUNNING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id.clone())
    {
        return;
    }
    THREADS.fetch_add(1, Ordering::SeqCst);
    let started = std::thread::Builder::new()
        .name("swarm-update".into())
        .spawn(move || {
            run(&engine, &id, rollback);
            THREADS.fetch_sub(1, Ordering::SeqCst);
        });
    if started.is_err() {
        THREADS.fetch_sub(1, Ordering::SeqCst);
        forget_after_spawn_failure();
    }
}

fn forget_after_spawn_failure() {
    // The service stays as it is; its next update or reconcile goes on.
    RUNNING.lock().unwrap_or_else(|p| p.into_inner()).clear();
}

/// What became of a new task while it is watched.
fn failed(engine: &Engine, all: &[Task], task_id: &str) -> bool {
    match all.iter().find(|t| t.task_id == task_id) {
        Some(t) => {
            t.state.status == crate::state::Status::Exited
                || crate::restart::record(&engine.dirs, &t.container_id).count > 0
        }
        // Never made (rejected) or gone.
        None => true,
    }
}

/// Write the service's update status (the service may be gone).
fn set_status(engine: &Engine, id: &str, value: Value) -> Option<ServiceRecord> {
    let _lock = engine.store.lock().ok()?;
    let mut service = engine.find_service(id).ok()?;
    let started = service
        .update_status
        .as_ref()
        .and_then(|s| s["StartedAt"].as_str().map(String::from));
    let mut value = value;
    if let Some(started) = started {
        value["StartedAt"] = Value::from(started);
    }
    service.update_status = Some(value);
    engine.store.write_service(&service).ok()?;
    Some(service)
}

/// What one step did.
enum Step {
    /// Made a batch of new tasks (their IDs; empty for a task that could not start).
    Batch(Vec<String>, UpdatePolicy),
    /// Nothing is outdated.
    Nothing(UpdatePolicy),
    /// The service or the swarm is gone, or the update was stopped.
    Gone,
}

/// The outdated tasks of a service, by slot.
fn outdated<'a>(
    engine: &Engine,
    service: &ServiceRecord,
    all: &'a [Task],
) -> BTreeMap<Slot, Vec<&'a Task>> {
    let mut out: BTreeMap<Slot, Vec<&Task>> = BTreeMap::new();
    for t in all.iter().filter(|t| t.service_id == service.id) {
        if let Some(slot) = t.slot
            && !engine.up_to_date(service, &t.task_id)
        {
            out.entry(slot).or_default().push(t);
        }
    }
    out
}

/// One step, under the lock: replace the next batch of outdated tasks.
fn step(engine: &Engine, id: &str, rollback: bool) -> Step {
    let Ok(_lock) = engine.store.lock() else {
        return Step::Gone;
    };
    if !running(id) {
        return Step::Gone;
    }
    let (Ok(Some(cluster)), Ok(mut service)) = (engine.store.cluster(), engine.find_service(id))
    else {
        return Step::Gone;
    };
    let key = if rollback {
        "RollbackConfig"
    } else {
        "UpdateConfig"
    };
    let policy = UpdatePolicy::from_spec(&service.spec[key]);
    // Missing and extra tasks first, as reconcile does.
    let all = tasks::list(&engine.dirs, &cluster.node_id);
    let _ = engine.reconcile_service(&cluster, &mut service, &all);
    let all = tasks::list(&engine.dirs, &cluster.node_id);
    let outdated = outdated(engine, &service, &all);
    let slots: Vec<Slot> = outdated.keys().copied().collect();
    let Some(batch) = plan::batches(&slots, policy.parallelism).into_iter().next() else {
        return Step::Nothing(policy);
    };
    let (Ok(networks), Ok(image)) = (
        super::net::attachments(&service),
        tasks::image(&service.spec),
    ) else {
        return Step::Batch(vec![String::new()], policy);
    };
    let grace = tasks::grace(&service.spec);
    let mut made = Vec::with_capacity(batch.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = batch
            .iter()
            .map(|slot| {
                let old = outdated.get(slot).cloned().unwrap_or_default();
                let (cluster, service, networks, image) = (&cluster, &service, &networks, &image);
                scope.spawn(move || {
                    if policy.order == Order::StopFirst {
                        tasks::remove(&engine.store, &old, grace);
                    }
                    let new = engine.start_task(cluster, service, *slot, networks, image);
                    if policy.order == Order::StartFirst {
                        tasks::remove(&engine.store, &old, grace);
                    }
                    new
                })
            })
            .collect();
        for h in handles {
            made.push(match h.join() {
                Ok(Ok(task)) => task,
                _ => String::new(),
            });
        }
    });
    let live = tasks::list(&engine.dirs, &cluster.node_id);
    let mine: Vec<&Task> = live.iter().filter(|t| t.service_id == service.id).collect();
    if super::net::apply(&engine.dirs, &mut service, &mine).unwrap_or(false) {
        let _ = engine.store.write_service(&service);
    }
    Step::Batch(made, policy)
}

/// End the update when nothing is outdated, under the lock (an update stored meanwhile
/// is then seen here, or finds this thread gone and starts its own).
fn finish(engine: &Engine, id: &str, rollback: bool) -> bool {
    let Ok(_lock) = engine.store.lock() else {
        forget(id);
        return true;
    };
    let (Ok(Some(cluster)), Ok(mut service)) = (engine.store.cluster(), engine.find_service(id))
    else {
        forget(id);
        return true;
    };
    let all = tasks::list(&engine.dirs, &cluster.node_id);
    if !outdated(engine, &service, &all).is_empty() {
        return false;
    }
    let started = service
        .update_status
        .as_ref()
        .and_then(|s| s["StartedAt"].as_str().map(String::from));
    let mut done = status(rollback, Stage::Completed, "");
    if let Some(started) = started {
        done["StartedAt"] = Value::from(started);
    }
    service.update_status = Some(done);
    let _ = engine.store.write_service(&service);
    forget(id);
    true
}

/// Swap the spec back to the previous one, for an automatic rollback.
fn begin_rollback(engine: &Engine, id: &str, task: &str) -> bool {
    let Ok(_lock) = engine.store.lock() else {
        return false;
    };
    let (Ok(Some(mut cluster)), Ok(mut service)) =
        (engine.store.cluster(), engine.find_service(id))
    else {
        return false;
    };
    let Some(previous) = service.previous_spec.take() else {
        return false;
    };
    service.previous_spec = Some(std::mem::replace(&mut service.spec, previous));
    service.version = cluster.next_index();
    service.updated = now();
    service.update_status = Some(status(
        true,
        Stage::Started,
        &format!("update rolled back due to failure or early termination of task {task}"),
    ));
    engine.store.write_cluster(&cluster).is_ok() && engine.store.write_service(&service).is_ok()
}

/// The new tasks under watch, and the failures so far.
#[derive(Default)]
struct Watch {
    /// (task ID, made at, watched for).
    tasks: Vec<(String, Instant, Duration)>,
    updated: usize,
    failures: BTreeSet<String>,
}

enum Watched {
    /// The batch runs and the delay is over (or, after the last batch, the watch is).
    Done,
    /// Too many tasks failed: the task that failed first.
    Failed(String),
    /// The update was stopped (the service removed).
    Stopped,
}

/// Wait for the new tasks of a batch to run and for the delay (`more`), or for the last
/// watch to end; meanwhile, count tasks that end.
fn watch(
    engine: &Engine,
    id: &str,
    w: &mut Watch,
    batch: &[String],
    policy: UpdatePolicy,
    more: bool,
) -> Watched {
    let since = Instant::now();
    loop {
        if !running(id) {
            return Watched::Stopped;
        }
        let node = engine
            .store
            .cluster()
            .ok()
            .flatten()
            .map(|c| c.node_id)
            .unwrap_or_default();
        let all = tasks::list(&engine.dirs, &node);
        for (task, made, window) in &w.tasks {
            if made.elapsed() < *window && failed(engine, &all, task) {
                w.failures.insert(task.clone());
            }
        }
        if policy.failure_action != FailureAction::Continue
            && plan::too_many_failures(w.failures.len(), w.updated, policy.max_failure_ratio)
        {
            return Watched::Failed(w.failures.iter().next().cloned().unwrap_or_default());
        }
        let done = if more {
            let up = batch
                .iter()
                .all(|t| all.iter().any(|x| &x.task_id == t && x.running()));
            (up || since.elapsed() >= policy.monitor) && since.elapsed() >= policy.delay
        } else {
            w.tasks
                .iter()
                .all(|(_, made, window)| made.elapsed() >= *window)
        };
        if done {
            return Watched::Done;
        }
        std::thread::sleep(POLL);
    }
}

fn run(engine: &Engine, id: &str, rollback: bool) {
    let mut rollback = rollback;
    let mut w = Watch::default();
    loop {
        let (made, policy, more) = match step(engine, id, rollback) {
            Step::Gone => {
                forget(id);
                return;
            }
            Step::Batch(made, policy) => (made, policy, true),
            Step::Nothing(policy) => (Vec::new(), policy, false),
        };
        w.updated += made.len();
        let now = Instant::now();
        let mut batch = Vec::with_capacity(made.len());
        for task in made {
            if task.is_empty() {
                // A task that could not start: a failure at once.
                w.failures
                    .insert(format!("(not started {})", w.failures.len() + 1));
            } else {
                w.tasks.push((task.clone(), now, policy.monitor));
                batch.push(task);
            }
        }
        match watch(engine, id, &mut w, &batch, policy, more) {
            Watched::Stopped => return,
            Watched::Done => {}
            Watched::Failed(task) => {
                if policy.failure_action == FailureAction::Rollback
                    && !rollback
                    && begin_rollback(engine, id, &task)
                {
                    rollback = true;
                    w = Watch::default();
                    continue;
                }
                pause(engine, id, rollback, &task);
                return;
            }
        }
        if !more && finish(engine, id, rollback) {
            return;
        }
    }
}

fn pause(engine: &Engine, id: &str, rollback: bool, task: &str) {
    let what = if rollback { "rollback" } else { "update" };
    let _ = set_status(
        engine,
        id,
        status(
            rollback,
            Stage::Paused,
            &format!("{what} paused due to failure or early termination of task {task}"),
        ),
    );
    forget(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_have_docker_states() {
        let s = status(false, Stage::Started, "");
        assert_eq!(s["State"], "updating");
        assert_eq!(s["Message"], "update in progress");
        assert!(s.get("CompletedAt").is_none());
        let s = status(true, Stage::Completed, "");
        assert_eq!(s["State"], "rollback_completed");
        assert_eq!(s["Message"], "rollback completed");
        assert!(s["CompletedAt"].is_string());
        assert_eq!(status(false, Stage::Paused, "x")["State"], "paused");
        assert_eq!(status(true, Stage::Paused, "x")["State"], "rollback_paused");
        assert_eq!(status(true, Stage::Started, "why")["Message"], "why");
    }

    #[test]
    fn the_service_process_waits_for_updates() {
        assert!(!busy());
        assert!(!running("x"));
    }
}
