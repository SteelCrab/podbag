//! Tasks: podbag containers with Docker's swarm labels. Their own files (the spec they
//! were made from, their secrets and configs) are in the store's task directories.

use super::body::{self, Attach};
use super::plan::{self, Slot, TemplateFacts};
use super::shapes::{Host, TaskFacts};
use super::store::{Cluster, Kind, ServiceRecord, Store, TaskSpec};
use crate::paths::Dirs;
use crate::restart::RestartPolicy;
use crate::state::{self, Status};
use crate::{Error, ErrorKind, MountRequest, Result};
use serde_json::Value;
use std::time::Duration;

pub const LABEL_SERVICE_ID: &str = "com.docker.swarm.service.id";
pub const LABEL_SERVICE_NAME: &str = "com.docker.swarm.service.name";
pub const LABEL_TASK_ID: &str = "com.docker.swarm.task.id";

/// A task's container as found on disk.
#[derive(Debug, Clone)]
pub struct Task {
    pub container_id: String,
    pub task_id: String,
    pub service_id: String,
    pub name: String,
    pub slot: Option<Slot>,
    pub state: crate::State,
    pub restart: RestartPolicy,
    pub labels: std::collections::BTreeMap<String, String>,
    /// Its bridge networks, the primary one first.
    pub networks: Vec<String>,
}

impl Task {
    pub fn running(&self) -> bool {
        self.state.status == Status::Running
    }
}

/// Every task container.
pub fn list(dirs: &Dirs, node_id: &str) -> Vec<Task> {
    let mut out = Vec::new();
    for id in state::list_ids(dirs).unwrap_or_default() {
        let Ok(spec) = state::read_config(dirs, &id) else {
            continue;
        };
        let (Some(task_id), Some(service_id)) = (
            spec.labels.get(LABEL_TASK_ID),
            spec.labels.get(LABEL_SERVICE_ID),
        ) else {
            continue;
        };
        let Ok(st) = state::reconcile(dirs, &id) else {
            continue;
        };
        let service_name = spec
            .labels
            .get(LABEL_SERVICE_NAME)
            .cloned()
            .unwrap_or_default();
        out.push(Task {
            container_id: id.clone(),
            task_id: task_id.clone(),
            service_id: service_id.clone(),
            slot: plan::slot_of(&spec.name, &service_name, node_id),
            name: spec.name.clone(),
            state: st,
            restart: spec.restart,
            networks: spec
                .bridge_networks()
                .into_iter()
                .map(String::from)
                .collect(),
            labels: spec.labels,
        });
    }
    out
}

/// A task's state as Docker shows it: (state, desired state, message, error).
pub fn task_state(
    status: Status,
    exit_code: Option<i32>,
    will_restart: bool,
) -> (&'static str, &'static str, String, String) {
    let desired = if will_restart { "running" } else { "shutdown" };
    match status {
        Status::Created => ("starting", "running", "starting".into(), String::new()),
        Status::Running => ("running", "running", "started".into(), String::new()),
        Status::Exited => match exit_code {
            Some(0) => ("complete", desired, "finished".into(), String::new()),
            Some(code) => (
                "failed",
                desired,
                "started".into(),
                format!("task: non-zero exit ({code})"),
            ),
            None => (
                "failed",
                desired,
                "started".into(),
                "task: ended without an exit code".into(),
            ),
        },
    }
}

fn rfc3339(secs: Option<u64>) -> String {
    crate::shim::logbook::rfc3339(secs.unwrap_or(0), 0)
}

/// The JSON facts of a task container.
pub fn facts(
    dirs: &Dirs,
    store: &Store,
    cluster: &Cluster,
    service: Option<&ServiceRecord>,
    task: &Task,
) -> TaskFacts {
    let stored = store.task_spec(&task.task_id);
    let count = crate::restart::record(dirs, &task.container_id).count;
    let code = task.state.exit_code.unwrap_or(0);
    let will_restart = crate::restart::wanted(
        &task.restart,
        code,
        crate::restart::stopped_by_user(dirs, &task.container_id),
        count,
    );
    let (state, desired, message, err) =
        task_state(task.state.status, task.state.exit_code, will_restart);
    let networks = task
        .networks
        .iter()
        .filter_map(|name| {
            let (network, ip) =
                crate::network::with_address_of(dirs, name, &task.container_id).ok()?;
            let prefix = network.cidr().1;
            Some((
                name.clone(),
                network.docker_id(),
                ip.map(|ip| format!("{ip}/{prefix}")).unwrap_or_default(),
            ))
        })
        .collect();
    let updated = task
        .state
        .finished
        .filter(|_| task.state.status == Status::Exited)
        .or(task.state.started)
        .or(Some(task.state.created));
    TaskFacts {
        id: task.task_id.clone(),
        version: service.map_or(cluster.index, |s| s.version),
        created: stored
            .as_ref()
            .map(|s| s.created.clone())
            .unwrap_or_else(|| rfc3339(Some(task.state.created))),
        updated: rfc3339(updated),
        name: task.name.clone(),
        service_id: task.service_id.clone(),
        slot: task.slot.map_or(0, Slot::number),
        node_id: cluster.node_id.clone(),
        spec: stored
            .map(|s| s.template)
            .or_else(|| service.map(|s| s.spec["TaskTemplate"].clone()))
            .unwrap_or(Value::Null),
        desired_state: desired.into(),
        state: state.into(),
        message,
        err,
        container_id: task.container_id.clone(),
        pid: task
            .state
            .pid
            .filter(|_| task.state.status == Status::Running)
            .unwrap_or(0),
        exit_code: code,
        networks,
    }
}

/// A task that could not be made: its directory says why, and it has no container.
pub fn rejected_facts(
    cluster: &Cluster,
    task_id: &str,
    spec: &TaskSpec,
    version: u64,
) -> TaskFacts {
    TaskFacts {
        id: task_id.to_string(),
        version,
        created: spec.created.clone(),
        updated: spec.created.clone(),
        name: String::new(),
        service_id: spec.service_id.clone(),
        slot: spec.slot,
        node_id: cluster.node_id.clone(),
        spec: spec.template.clone(),
        desired_state: "shutdown".into(),
        state: "rejected".into(),
        message: "preparing".into(),
        err: spec.error.clone().unwrap_or_default(),
        container_id: String::new(),
        pid: 0,
        exit_code: 0,
        networks: Vec::new(),
    }
}

/// Where a secret or config goes in the container: a secret's relative name is under
/// `/run/secrets`, a config's under `/`.
pub fn file_target(kind: Kind, name: &str) -> String {
    match (kind, name.starts_with('/')) {
        (_, true) => name.to_string(),
        (Kind::Secret, false) => format!("/run/secrets/{name}"),
        (Kind::Config, false) => format!("/{name}"),
    }
}

/// The references to secrets or configs in a `ContainerSpec`.
fn references(spec: &Value, kind: Kind) -> &[Value] {
    let key = match kind {
        Kind::Secret => "Secrets",
        Kind::Config => "Configs",
    };
    spec[key].as_array().map_or(&[], Vec::as_slice)
}

/// The object a reference names (by ID, then by name), and the file it asks for.
fn resolve<'a>(
    objects: &'a [super::store::FileObject],
    kind: Kind,
    reference: &Value,
) -> Result<&'a super::store::FileObject> {
    let (id_key, name_key) = match kind {
        Kind::Secret => ("SecretID", "SecretName"),
        Kind::Config => ("ConfigID", "ConfigName"),
    };
    let id = reference[id_key].as_str().unwrap_or_default();
    let name = reference[name_key].as_str().unwrap_or_default();
    objects
        .iter()
        .find(|o| !id.is_empty() && o.id == id)
        .or_else(|| {
            objects
                .iter()
                .find(|o| !name.is_empty() && o.name() == name)
        })
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "{} not found: {}",
                    kind.word(),
                    if name.is_empty() { id } else { name }
                ),
                format!(
                    "the service refers to a {} that does not exist",
                    kind.word()
                ),
                format!("create it first with `docker {} create`", kind.word()),
            )
        })
}

/// Check that every secret and config a `ContainerSpec` refers to exists, and that its
/// target is a path a mount can use.
pub fn check_references(store: &Store, spec: &Value) -> Result<()> {
    for kind in [Kind::Secret, Kind::Config] {
        let objects = store.objects(kind)?;
        for reference in references(spec, kind) {
            resolve(&objects, kind, reference)?;
            if let Some(name) = reference["File"]["Name"].as_str() {
                crate::volume::check_target(&file_target(kind, name))?;
            }
        }
    }
    Ok(())
}

/// Write a task's secret and config files into its directory and return their mounts
/// (read-only binds; the existing mount code resolves each target inside the
/// container). Root owns them as the reference says; rootless, the user does (root in
/// the container). The data never goes into an error message.
pub fn write_files(
    store: &Store,
    task_dir: &std::path::Path,
    spec: &Value,
) -> Result<Vec<MountRequest>> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let rootless = super::rootless();
    let mut mounts = Vec::new();
    for kind in [Kind::Secret, Kind::Config] {
        let objects = store.objects(kind)?;
        let dir = task_dir.join(kind.dir());
        for (index, reference) in references(spec, kind).iter().enumerate() {
            let file = &reference["File"];
            let Some(name) = file["Name"].as_str() else {
                // A config for the runtime (a credential spec): no file.
                continue;
            };
            let object = resolve(&objects, kind, reference)?;
            let target = file_target(kind, name);
            crate::volume::check_target(&target)?;
            let data = store.object_data(kind, &object.id)?;
            crate::store::perm::private_dir_builder()
                .recursive(true)
                .create(&dir)
                .map_err(|e| {
                    Error::io("cannot make a task's files", &e, "check the data directory")
                })?;
            let path = dir.join(index.to_string());
            let fail = |e: std::io::Error| {
                Error::io(
                    format!("cannot write {} {} for a task", kind.word(), object.name()),
                    &e,
                    "check the data directory",
                )
            };
            let _ = std::fs::remove_file(&path);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(fail)?;
            f.write_all(&data).map_err(fail)?;
            let mode = file["Mode"].as_u64().map_or(0o444, |m| (m & 0o7777) as u32);
            if !rootless {
                let id = |key: &str| file[key].as_str().and_then(|v| v.parse::<u32>().ok());
                std::os::unix::fs::fchown(
                    &f,
                    Some(id("UID").unwrap_or(0)),
                    Some(id("GID").unwrap_or(0)),
                )
                .map_err(fail)?;
            }
            f.set_permissions(std::fs::Permissions::from_mode(mode))
                .map_err(fail)?;
            mounts.push(MountRequest::Bind {
                source: path.display().to_string(),
                target,
                access: crate::Access::ReadOnly,
            });
        }
    }
    Ok(mounts)
}

/// The image of a spec, pulled when it is not stored yet.
pub fn image(spec: &Value) -> Result<crate::Image> {
    let name = spec["TaskTemplate"]["ContainerSpec"]["Image"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "cannot create the service",
                "its ContainerSpec names no image",
                "give an image",
            )
        })?;
    match crate::Image::get(name) {
        Ok(image) => Ok(image),
        Err(e) if e.kind == ErrorKind::NotFound => crate::Image::pull(name),
        Err(e) => Err(e),
    }
}

/// What a new task needs.
pub struct NewTask<'a> {
    pub cluster: &'a Cluster,
    pub service: &'a ServiceRecord,
    pub slot: Slot,
    pub networks: &'a [Attach],
    /// Publish the service's ports from this task (rootless: one task takes them).
    pub publish: bool,
    pub host: &'a Host,
    pub image: &'a crate::Image,
}

/// The container a task would get, with the warnings the mapping gives. Nothing is
/// made: this also checks a spec before the service takes it.
pub fn container(
    new: &NewTask,
    task_id: &str,
    mounts: Vec<MountRequest>,
) -> Result<(crate::Container, Vec<String>)> {
    let service = new.service;
    let name = plan::task_name(service.name(), new.slot, &new.cluster.node_id, task_id);
    let facts = TemplateFacts {
        service_id: &service.id,
        service_name: service.name(),
        node_id: &new.cluster.node_id,
        node_hostname: &new.host.hostname,
        task_id,
        task_name: &name,
        slot: new.slot.number(),
    };
    let template = &service.spec["TaskTemplate"];
    let body = body::create_body(template, &facts, new.networks);
    let (mut c, warnings) = crate::api::create::map_create(&body, Some(&name), new.image)?;
    for m in mounts {
        c = c.mount(m);
    }
    if new.publish {
        for p in &service.ports {
            c = c.publish(crate::network::PortMapping {
                host_ip: std::net::Ipv4Addr::UNSPECIFIED,
                host_port: p.published,
                container_port: p.target,
                proto: if p.protocol == "udp" {
                    crate::network::Proto::Udp
                } else {
                    crate::network::Proto::Tcp
                },
            });
        }
    }
    Ok((c, warnings))
}

/// Make and start a task. A task that cannot start keeps its directory with the reason
/// (shown as a rejected task) and leaves no container behind.
pub fn create(store: &Store, new: &NewTask) -> Result<String> {
    let task_id = super::store::new_id()?;
    let service = new.service;
    let mut spec = TaskSpec {
        service_id: service.id.clone(),
        created: super::store::now(),
        template: service.spec["TaskTemplate"].clone(),
        endpoint: service.spec["EndpointSpec"].clone(),
        slot: new.slot.number(),
        error: None,
    };
    store.write_task_spec(&task_id, &spec)?;
    let started = (|| {
        let dir = store.make_task_dir(&task_id)?;
        let mounts = write_files(store, &dir, &service.spec["TaskTemplate"]["ContainerSpec"])?;
        let (c, _) = container(new, &task_id, mounts)?;
        c.start()
    })();
    match started {
        Ok(_) => Ok(task_id),
        Err(e) => {
            let name = plan::task_name(service.name(), new.slot, &new.cluster.node_id, &task_id);
            if let Ok(handle) = crate::ContainerHandle::get(&name) {
                let _ = handle.remove_with_volumes(true);
            }
            let _ = std::fs::remove_dir_all(store.task_dir(&task_id)?.join(Kind::Secret.dir()));
            let _ = std::fs::remove_dir_all(store.task_dir(&task_id)?.join(Kind::Config.dir()));
            spec.error = Some(crate::api::router::error_text(&e));
            let _ = store.write_task_spec(&task_id, &spec);
            Err(e)
        }
    }
}

/// How long a task gets to stop before it is killed (`--stop-grace-period`; 10 s).
pub fn grace(spec: &Value) -> Duration {
    spec["TaskTemplate"]["ContainerSpec"]["StopGracePeriod"]
        .as_u64()
        .map_or(Duration::from_secs(10), Duration::from_nanos)
}

/// Stop and remove tasks, all at once, with their files.
pub fn remove(store: &Store, tasks: &[&Task], grace: Duration) {
    std::thread::scope(|scope| {
        for task in tasks {
            scope.spawn(move || {
                let handle = crate::ContainerHandle::from_id(task.container_id.clone());
                if task.running() {
                    let _ = handle.stop(grace);
                }
                let _ = handle.remove_with_volumes(true);
                store.remove_task_dir(&task.task_id);
            });
        }
    });
}

/// Remove and confirm the service's containers before allowing another worker
/// to start the same slots. A failed state read is not evidence of removal.
pub(super) fn release_service(
    store: &Store,
    dirs: &Dirs,
    service: &str,
    grace: Duration,
) -> Result<()> {
    for id in state::list_ids(dirs)? {
        let spec = match state::read_config(dirs, &id) {
            Ok(spec) => spec,
            Err(error) if error.kind == ErrorKind::NotFound => {
                let directory = dirs.container_dir(&id);
                if matches!(directory.try_exists(), Ok(false)) {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if spec
            .labels
            .get(LABEL_SERVICE_ID)
            .is_none_or(|value| value != service)
        {
            continue;
        }
        let handle = crate::ContainerHandle::from_id(id);
        let _ = handle.stop(grace);
        handle.remove_with_volumes(true)?;
        if let Some(task_id) = spec.labels.get(LABEL_TASK_ID) {
            store.remove_task_dir(task_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_follow_the_container() {
        assert_eq!(task_state(Status::Running, None, true).0, "running");
        assert_eq!(task_state(Status::Running, None, false).1, "running");
        assert_eq!(task_state(Status::Created, None, true).0, "starting");
        let (state, desired, _, err) = task_state(Status::Exited, Some(0), false);
        assert_eq!((state, desired, err.as_str()), ("complete", "shutdown", ""));
        let (state, desired, _, err) = task_state(Status::Exited, Some(3), true);
        assert_eq!(
            (state, desired, err.as_str()),
            ("failed", "running", "task: non-zero exit (3)")
        );
    }

    #[test]
    fn file_targets() {
        assert_eq!(file_target(Kind::Secret, "pw"), "/run/secrets/pw");
        assert_eq!(file_target(Kind::Secret, "/etc/pw"), "/etc/pw");
        assert_eq!(file_target(Kind::Config, "site.conf"), "/site.conf");
        assert_eq!(
            file_target(Kind::Config, "/etc/nginx/a.conf"),
            "/etc/nginx/a.conf"
        );
    }
}
