//! One manager can assign services to Podbag workers reached over authenticated SSH.
//! The remote API remains on the worker's private Unix socket.

use super::ops::{Engine, TaskEntry, host};
use super::plan;
use super::shapes::Host;
use super::store::{self, Cluster, NodeRecord, ServiceRecord, Tokens};
use super::update;
use crate::client::{Client, Endpoint};
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub name: String,
    pub destination: String,
    pub ssh_config: Option<String>,
    pub created: String,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default = "initial_worker_version")]
    pub version: u64,
    #[serde(default)]
    pub drained: bool,
    #[serde(default)]
    pub host: Option<Host>,
}

fn initial_worker_version() -> u64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Membership {
    pub cluster_id: String,
    pub node_id: String,
    #[serde(default)]
    pub own_ingress: bool,
}

impl Membership {
    fn same_node(&self, other: &Self) -> bool {
        self.cluster_id == other.cluster_id && self.node_id == other.node_id
    }

    /// Local task operations need cluster facts but do not expose manager commands.
    fn cluster(&self) -> Cluster {
        let now = store::now();
        Cluster {
            id: self.cluster_id.clone(),
            node_id: self.node_id.clone(),
            created: now.clone(),
            updated: now.clone(),
            version: 1,
            index: 1,
            spec: json!({}),
            node: NodeRecord {
                version: 1,
                created: now.clone(),
                updated: now,
                spec: json!({}),
            },
            tokens: Tokens {
                worker: String::new(),
                manager: String::new(),
            },
            listen_addr: String::new(),
            advertise_addr: String::new(),
            default_addr_pool: Vec::new(),
            subnet_size: 0,
            data_path_port: 0,
            own_ingress: self.own_ingress,
        }
    }
}

fn invalid(what: &str, why: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        what,
        why,
        "check the worker or service settings",
    )
}

fn held(why: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::Conflict,
        "cannot change Swarm membership",
        why,
        "remove the existing assignment first",
    )
}

fn destination_ok(value: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
    };
    match value.split_once('@') {
        Some((user, host)) => part(user) && part(host),
        None => part(value),
    }
}

impl Worker {
    fn endpoint(&self) -> Result<Endpoint> {
        if !destination_ok(&self.destination) {
            return Err(invalid(
                "invalid SSH destination",
                "give an SSH name or user@host",
            ));
        }
        let mut args: Vec<OsString> = vec!["ssh".into()];
        if let Some(path) = &self.ssh_config {
            if !Path::new(path).is_absolute() {
                return Err(invalid(
                    "invalid SSH configuration",
                    "give an absolute file path",
                ));
            }
            args.extend(["-F".into(), path.into()]);
        }
        args.extend(
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=1",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ControlPath=none",
                "--",
                &self.destination,
                "podbag",
                "system",
                "dial-stdio",
            ]
            .map(OsString::from),
        );
        Ok(Endpoint::Command(args))
    }

    fn call<T: DeserializeOwned>(&self, cluster: &Cluster, action: Action) -> Result<T> {
        let failed = |why: String| {
            Error::new(
                ErrorKind::Runtime,
                format!("cannot complete request on worker {}", self.name),
                why,
                "check SSH key access, known hosts and the worker's Podbag installation",
            )
        };
        let client = Client::new(self.endpoint()?);
        let request = serde_json::to_value(Wire {
            member: Membership {
                cluster_id: cluster.id.clone(),
                node_id: self.id.clone(),
                own_ingress: false,
            },
            action,
        })
        .map_err(|e| failed(e.to_string()))?;
        let output = client
            .call("POST", "/swarm/podbag/worker", Some(&request))
            .map_err(|e| failed(e.message))?;
        serde_json::from_value(output).map_err(|e| failed(e.to_string()))
    }

    pub fn ready(&self, cluster: &Cluster) -> bool {
        self.host.is_some() && self.call::<Host>(cluster, Action::Probe).is_ok()
    }

    fn accepts_assignments(&self, cluster: &Cluster) -> bool {
        !self.drained
            && self.host.is_some()
            && self.call::<()>(cluster, Action::ProbeAssignments).is_ok()
    }

    pub fn check(&self, cluster: &Cluster, service: &ServiceRecord) -> Result<Vec<String>> {
        self.call(
            cluster,
            Action::Check {
                service: Box::new(service.clone()),
            },
        )
    }

    pub fn check_pinned(
        &self,
        cluster: &Cluster,
        service: &ServiceRecord,
        rollback: bool,
    ) -> Result<Vec<String>> {
        self.call(
            cluster,
            Action::CheckPinnedRoll {
                service: Box::new(service.clone()),
                rollback,
            },
        )
    }

    pub(super) fn roll(
        &self,
        cluster: &Cluster,
        service: &ServiceRecord,
        rollback: bool,
    ) -> Result<Value> {
        self.call(
            cluster,
            Action::RollPinned {
                service: Box::new(service.clone()),
                rollback,
            },
        )
    }

    pub fn reconcile(&self, cluster: &Cluster, service: &ServiceRecord) -> Result<()> {
        let record = Box::new(service.clone());
        let action = if service.worker_slots.is_empty() {
            Action::Reconcile { service: record }
        } else {
            Action::ReconcileAssigned { service: record }
        };
        self.call(cluster, action)
    }

    fn release_assignment(&self, cluster: &Cluster, service: &ServiceRecord) -> Result<()> {
        self.call(
            cluster,
            Action::ReleaseAssignment {
                id: service.id.clone(),
                version: service.version,
            },
        )
    }

    pub fn tasks(&self, cluster: &Cluster) -> Result<Vec<TaskEntry>> {
        self.call(cluster, Action::Tasks)
    }

    pub fn remove_service(&self, cluster: &Cluster, service: &ServiceRecord) -> Result<()> {
        let id = service.id.clone();
        let action = if service.worker_slots.is_empty() {
            Action::Remove { id }
        } else {
            Action::RemoveAssigned { id }
        };
        self.call(cluster, action)
    }
}

fn constraint(service: &ServiceRecord) -> Option<(&str, &str)> {
    let [rule] = service.spec["TaskTemplate"]["Placement"]["Constraints"]
        .as_array()?
        .as_slice()
    else {
        return None;
    };
    let (key, value) = rule.as_str()?.split_once("==")?;
    Some((key.trim(), value.trim()))
}

/// Unconstrained services keep using the manager. Only an explicit worker role
/// constraint opts in to scheduling replicas across enrolled workers.
#[derive(Debug, PartialEq, Eq)]
pub enum Placement<'a> {
    Local,
    Pinned(&'a str),
    Workers,
}

pub fn placement<'a>(service: &'a ServiceRecord, cluster: &Cluster) -> Result<Placement<'a>> {
    let list = service.spec["TaskTemplate"]["Placement"]["Constraints"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if list.is_empty() {
        return Ok(Placement::Local);
    }
    if let Some(rule) = constraint(service) {
        match rule {
            ("node.id", id) if plan::is_object_id(id) => {
                return Ok(if id == cluster.node_id {
                    Placement::Local
                } else {
                    Placement::Pinned(id)
                });
            }
            ("node.role", "worker")
                if matches!(
                    plan::mode_of(&service.spec),
                    Some(plan::Mode::Replicated(_))
                ) =>
            {
                return Ok(Placement::Workers);
            }
            _ => {}
        }
    }
    Err(invalid(
        "unsupported Swarm placement",
        "use node.id==ID with the full ID, or node.role==worker for a replicated service",
    ))
}

pub fn enrolled(engine: &Engine, id: &str) -> Result<Worker> {
    engine
        .store
        .workers()?
        .into_iter()
        .find(|worker| worker.id == id && worker.host.is_some())
        .ok_or_else(|| {
            invalid(
                "unknown Swarm worker",
                format!("worker {id} is not enrolled"),
            )
        })
}

/// All owners get the requested replica count, even if all of their slots were
/// scaled down. Keep the owner of each slot until the service is removed.
pub fn assigned_workers(engine: &Engine, service: &ServiceRecord) -> Result<Vec<Worker>> {
    let owners: BTreeSet<&str> = service.worker_slots.values().map(String::as_str).collect();
    Ok(engine
        .store
        .workers()?
        .into_iter()
        .filter(|worker| owners.contains(worker.id.as_str()))
        .collect())
}

/// Keep each existing owner. For a new slot, prefer the eligible worker that owns the
/// fewest slots of this service, breaking ties by the stable name and ID.
fn fill_slots(
    slots: &mut BTreeMap<u64, String>,
    workers: &[Worker],
    replicas: u64,
) -> BTreeSet<String> {
    let mut load: Vec<_> = workers
        .iter()
        .map(|worker| {
            let count = slots.values().filter(|owner| *owner == &worker.id).count();
            (count, worker.name.as_str(), worker.id.as_str())
        })
        .collect();
    let mut added = BTreeSet::new();
    for slot in 1..=replicas {
        if slots.contains_key(&slot) {
            continue;
        }
        let Some((count, _, id)) = load.iter_mut().min_by_key(|worker| **worker) else {
            break;
        };
        *count += 1;
        slots.insert(slot, (*id).to_owned());
        added.insert((*id).to_owned());
    }
    added
}

pub fn prepare_assignments(
    engine: &Engine,
    cluster: &Cluster,
    service: &mut ServiceRecord,
    create: bool,
) -> Result<Vec<String>> {
    check_remote(service)?;
    if !service.worker_releases.is_empty() {
        return Err(held(
            "wait for the old workers to confirm that their tasks have stopped",
        ));
    }
    let Some(plan::Mode::Replicated(replicas)) = plan::mode_of(&service.spec) else {
        return Err(invalid("cannot place the service", "use replicated mode"));
    };
    if !create && (1..=replicas).all(|slot| service.worker_slots.contains_key(&slot)) {
        return Ok(Vec::new());
    }
    let mut candidates = engine.store.workers()?;
    candidates.retain(|worker| worker.accepts_assignments(cluster));
    if candidates.is_empty() {
        return Err(held(
            "no enrolled worker can accept new replica assignments",
        ));
    }
    let added = fill_slots(&mut service.worker_slots, &candidates, replicas);
    let mut warnings = BTreeSet::new();
    for worker in &candidates {
        if added.contains(&worker.id) {
            warnings.extend(worker.check(cluster, service)?);
        }
    }
    if create && replicas == 0 {
        warnings.extend(candidates[0].check(cluster, service)?);
    }
    Ok(warnings.into_iter().collect())
}

/// A previously assigned replica can move only after its old worker has fenced
/// and removed the assignment. Persist the pending release before sending it.
pub(super) fn reconcile_assignments(
    engine: &Engine,
    cluster: &mut Cluster,
    service: &mut ServiceRecord,
) -> Result<()> {
    let workers = engine.store.workers()?;
    let draining: BTreeSet<_> = workers
        .iter()
        .filter(|node| node.drained && service.worker_slots.values().any(|owner| owner == &node.id))
        .map(|node| node.id.clone())
        .collect();
    if !draining.is_subset(&service.worker_releases) {
        service.worker_releases.extend(draining);
        save_revised_assignments(engine, cluster, service)?;
    }

    let mut waiting = Ok(());
    for id in service.worker_releases.clone() {
        let result = workers
            .iter()
            .find(|worker| worker.id == id)
            .ok_or_else(|| held(format!("cannot confirm task removal on worker {id}")))
            .and_then(|worker| worker.release_assignment(cluster, service));
        if let Err(error) = result {
            waiting = Err(error);
            continue;
        }
        service.worker_slots.retain(|_, owner| owner != &id);
        service.worker_releases.remove(&id);
        engine.store.write_service(service)?;
    }
    waiting?;

    let mut candidate = service.clone();
    let fill = prepare_assignments(engine, cluster, &mut candidate, false);
    if fill.is_ok() && candidate.worker_slots != service.worker_slots {
        save_revised_assignments(engine, cluster, &mut candidate)?;
        *service = candidate;
    }
    // Continue reconciling existing owners even if no worker can accept new slots.
    let mut result = fill.map(|_| ());
    for worker in assigned_workers(engine, service)? {
        if let Err(error) = worker.reconcile(cluster, service) {
            result = Err(error);
        }
    }
    result
}

fn save_revised_assignments(
    engine: &Engine,
    cluster: &mut Cluster,
    service: &mut ServiceRecord,
) -> Result<()> {
    service.version = cluster.next_index();
    service.updated = store::now();
    engine.store.write_cluster(cluster)?;
    engine.store.write_service(service)
}

pub fn check_remote(service: &ServiceRecord) -> Result<()> {
    let spec = &service.spec;
    let template = &spec["TaskTemplate"];
    let container = &template["ContainerSpec"];
    let nonempty = |value: &Value| value.as_array().is_some_and(|items| !items.is_empty());
    for (field, label) in [
        (&spec["EndpointSpec"]["Ports"], "published ports"),
        (&spec["Networks"], "service networks"),
        (&template["Networks"], "service networks"),
        (&container["Mounts"], "mounts"),
        (&container["Secrets"], "secrets"),
        (&container["Configs"], "configs"),
        (
            &template["Placement"]["Preferences"],
            "placement preferences",
        ),
    ] {
        if nonempty(field) {
            return Err(invalid(
                "unsupported worker service setting",
                format!("{label} are not supported on SSH workers"),
            ));
        }
    }
    if template["Placement"]["MaxReplicas"]
        .as_u64()
        .is_some_and(|max| max > 0)
    {
        return Err(invalid(
            "unsupported worker service setting",
            "maximum replicas per node is not supported on SSH workers",
        ));
    }
    Ok(())
}

fn validate_pinned_roll(
    cluster: &Cluster,
    previous: &ServiceRecord,
    service: &ServiceRecord,
    rollback: bool,
) -> Result<()> {
    let policy = plan::UpdatePolicy::from_spec(
        &service.spec[if rollback {
            "RollbackConfig"
        } else {
            "UpdateConfig"
        }],
    );
    let mode = plan::mode_of(&service.spec);
    if constraint(previous) != Some(("node.id", cluster.node_id.as_str()))
        || constraint(service) != constraint(previous)
        || !service.worker_slots.is_empty()
        || !matches!(mode, Some(plan::Mode::Replicated(_)))
        || mode != plan::mode_of(&previous.spec)
    {
        return Err(invalid(
            "unsupported worker rollout placement",
            "the service must keep the same worker and replica count",
        ));
    }
    if policy.order != plan::Order::StopFirst
        || policy.failure_action != plan::FailureAction::Pause
        || policy.max_failure_ratio != 0.0
    {
        return Err(invalid(
            "unsupported worker rollout policy",
            "use stop-first order and pause at the first task failure",
        ));
    }
    check_remote(service)
}

pub fn add(
    engine: &Engine,
    name: &str,
    destination: &str,
    ssh_config: Option<String>,
) -> Result<String> {
    if !plan::valid_name(name) || !destination_ok(destination) {
        return Err(invalid(
            "invalid worker",
            "give a worker name and an SSH name or user@host",
        ));
    }
    let _lock = engine.store.lock()?;
    let cluster = engine
        .cluster()?
        .ok_or_else(|| held("initialize the manager with docker swarm init"))?;
    let match_node = engine
        .store
        .workers()?
        .into_iter()
        .find(|w| w.name == name || w.destination == destination);
    let mut worker = match match_node {
        Some(w) if w.name == name && w.destination == destination && w.ssh_config == ssh_config => {
            w
        }
        Some(_) => return Err(held("a worker already uses that name or SSH destination")),
        None => Worker {
            id: store::new_id()?,
            name: name.to_owned(),
            destination: destination.to_owned(),
            ssh_config,
            created: store::now(),
            updated: None,
            version: initial_worker_version(),
            drained: false,
            host: None,
        },
    };
    worker.endpoint()?;
    worker.call::<Host>(&cluster, Action::Preflight)?;
    // Keep the assigned identity if the SSH connection ends after the worker accepted it.
    engine.store.write_worker(&worker)?;
    worker.host = Some(worker.call(&cluster, Action::Join)?);
    engine.store.write_worker(&worker)?;
    Ok(worker.id)
}

pub fn find(engine: &Engine, key: &str) -> Result<Option<Worker>> {
    let mut found = engine
        .store
        .workers()?
        .into_iter()
        .filter(|worker| key == worker.name || (key.len() >= 3 && worker.id.starts_with(key)));
    let first = found.next();
    if found.next().is_some() {
        return Err(invalid("ambiguous worker", "give its full ID"));
    }
    Ok(first)
}

pub(super) fn update_availability(
    engine: &Engine,
    id: &str,
    version: Option<u64>,
    spec: &Value,
) -> Result<()> {
    {
        let _lock = engine.store.lock()?;
        let mut cluster = engine
            .cluster()?
            .ok_or_else(|| held("the manager is inactive"))?;
        let mut worker = enrolled(engine, id)?;
        Engine::check_version(version, worker.version)?;
        if spec["Role"].as_str().is_some_and(|role| role != "worker")
            || spec["Name"]
                .as_str()
                .is_some_and(|name| name != worker.name)
            || spec["Labels"]
                .as_object()
                .is_some_and(|labels| !labels.is_empty())
        {
            return Err(invalid(
                "cannot change worker settings",
                "SSH workers support availability updates",
            ));
        }
        let drain = match spec["Availability"].as_str() {
            Some("active") => false,
            Some("drain") => true,
            _ => {
                return Err(invalid(
                    "invalid worker availability",
                    "use active or drain",
                ));
            }
        };
        for service in engine.store.services()? {
            if drain && placement(&service, &cluster)? == Placement::Pinned(id) {
                return Err(held(format!(
                    "remove the pinned service {} before draining this worker",
                    service.name()
                )));
            }
            if !drain && service.worker_releases.contains(id) {
                return Err(held("finish draining this worker before reactivating it"));
            }
        }
        if worker.drained != drain {
            worker.drained = drain;
            worker.version = cluster.next_index();
            worker.updated = Some(store::now());
            engine.store.write_cluster(&cluster)?;
            engine.store.write_worker(&worker)?;
        }
    }
    engine.reconcile()
}

pub fn remove(engine: &Engine, key: &str) -> Result<()> {
    let _lock = engine.store.lock()?;
    let cluster = engine
        .cluster()?
        .ok_or_else(|| held("the manager is inactive"))?;
    let worker = find(engine, key)?.ok_or_else(|| {
        Error::new(
            ErrorKind::NotFound,
            format!("worker {key} not found"),
            "there is no worker with that name or ID",
            "list registered workers with docker node ls",
        )
    })?;
    for service in engine.store.services()? {
        if placement(&service, &cluster)? == Placement::Pinned(&worker.id)
            || service
                .worker_slots
                .values()
                .any(|owner| *owner == worker.id)
        {
            return Err(held(format!("service {} uses this worker", service.name())));
        }
    }
    worker.call::<()>(&cluster, Action::Leave)?;
    engine.store.remove_worker(&worker.id)
}

#[derive(Serialize, Deserialize)]
pub struct Wire {
    member: Membership,
    action: Action,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Preflight,
    Join,
    Probe,
    ProbeAssignments,
    Check {
        service: Box<ServiceRecord>,
    },
    CheckPinnedRoll {
        service: Box<ServiceRecord>,
        rollback: bool,
    },
    RollPinned {
        service: Box<ServiceRecord>,
        rollback: bool,
    },
    Reconcile {
        service: Box<ServiceRecord>,
    },
    // A separate action lets an older worker reject this request instead of
    // ignoring the slot map and starting all replicas.
    ReconcileAssigned {
        service: Box<ServiceRecord>,
    },
    ReleaseAssignment {
        id: String,
        version: u64,
    },
    Tasks,
    Remove {
        id: String,
    },
    RemoveAssigned {
        id: String,
    },
    Leave,
}

pub fn handle(engine: &Engine, request: Wire) -> Result<Value> {
    let Wire { mut member, action } = request;
    if !plan::is_object_id(&member.cluster_id) || !plan::is_object_id(&member.node_id) {
        return Err(invalid(
            "invalid worker identity",
            "give the cluster and node IDs",
        ));
    }
    // In particular, a manager might be registering an SSH alias to itself. Refuse
    // before taking the manager's own lock, already held by the registering call.
    if engine.cluster()?.is_some() {
        return Err(held("this Podbag engine already manages a Swarm"));
    }
    let _lock = engine.store.lock()?;
    if engine.cluster()?.is_some() {
        return Err(held("this Podbag engine already manages a Swarm"));
    }
    let enrolled = engine.store.membership()?;
    if enrolled
        .as_ref()
        .is_some_and(|previous| !previous.same_node(&member))
    {
        return Err(held(
            "this worker already belongs to another cluster or node assignment",
        ));
    }
    if matches!(action, Action::Preflight) {
        return Ok(json!(host()));
    }
    if matches!(action, Action::Join) {
        if enrolled.is_none() {
            member.own_ingress = super::net::ensure_ingress(&engine.store)?;
            engine.store.write_membership(&member)?;
        }
        return Ok(json!(host()));
    }
    let Some(enrolled) = enrolled else {
        if matches!(action, Action::Leave) {
            return Ok(Value::Null);
        }
        return Err(held("this Podbag engine is not an enrolled worker"));
    };
    let cluster = enrolled.cluster();
    let assigned_request = matches!(action, Action::ReconcileAssigned { .. });
    if let Action::Remove { id } | Action::RemoveAssigned { id } = &action {
        engine.store.retire_service(id)?;
    }
    match action {
        Action::Preflight | Action::Join => unreachable!("enrollment was handled"),
        Action::Probe => Ok(json!(host())),
        Action::ProbeAssignments => Ok(Value::Null),
        Action::Check { service } => {
            check_remote(&service)?;
            let warnings = engine.check_local(&cluster, &service)?;
            Ok(json!(warnings))
        }
        Action::CheckPinnedRoll { service, rollback } => {
            let previous = engine.find_service(&service.id)?;
            if previous.version != service.version
                || engine.store.service_retired(&service.id)?
                || update::active(&previous).is_some()
            {
                return Err(held("this worker service is being removed or updated"));
            }
            validate_pinned_roll(&cluster, &previous, &service, rollback)?;
            Ok(json!(engine.check_local(&cluster, &service)?))
        }
        Action::RollPinned { service, rollback } => {
            roll_pinned(engine, cluster, *service, rollback)
        }
        Action::Reconcile { mut service } | Action::ReconcileAssigned { mut service } => {
            let valid_placement = match placement(&service, &cluster)? {
                Placement::Workers => {
                    assigned_request
                        && service
                            .worker_slots
                            .values()
                            .any(|node| *node == cluster.node_id)
                }
                Placement::Local => !assigned_request && service.worker_slots.is_empty(),
                Placement::Pinned(_) => false,
            };
            if !valid_placement || !service.worker_releases.is_empty() {
                return Err(held(
                    "the service has no matching replica assignments for this worker",
                ));
            }
            if engine.store.service_retired(&service.id)? {
                return Err(held("the worker already removed this service"));
            }
            if engine
                .store
                .assignment_revoked(&service.id, service.version)?
            {
                return Err(held("the worker already released this assignment revision"));
            }
            check_remote(&service)?;
            let previous = engine
                .store
                .services()?
                .into_iter()
                .find(|s| s.id == service.id);
            if let Some(previous) = &previous {
                if previous.version > service.version
                    || (previous.version == service.version
                        && (previous.spec != service.spec
                            || previous.worker_slots != service.worker_slots))
                {
                    return Err(held("the worker already has a newer service specification"));
                }
                if update::active(previous).is_some() || update::paused(previous) {
                    if previous.version == service.version {
                        return Ok(Value::Null);
                    }
                    return Err(held("finish or roll back the previous worker update first"));
                }
                service.vips = previous.vips.clone();
            }
            if previous.as_ref() != Some(&service) {
                engine.store.write_service(&service)?;
            }
            let all = super::tasks::list(&engine.dirs, &cluster.node_id);
            engine.reconcile_local(&cluster, &mut service, &all)?;
            engine.clean_task_dirs(&all);
            Ok(Value::Null)
        }
        Action::ReleaseAssignment { id, version } => {
            release_assignment(engine, &cluster, &id, version)?;
            Ok(Value::Null)
        }
        Action::Tasks => Ok(json!(engine.local_task_list(&cluster)?)),
        Action::Remove { id } | Action::RemoveAssigned { id } => {
            update::forget(&id);
            let Some(service) = engine.store.services()?.into_iter().find(|s| s.id == id) else {
                return Ok(Value::Null);
            };
            let all = super::tasks::list(&engine.dirs, &cluster.node_id);
            let owned: Vec<_> = all.iter().filter(|task| task.service_id == id).collect();
            super::tasks::remove(&engine.store, &owned, super::tasks::grace(&service.spec));
            if super::tasks::list(&engine.dirs, &cluster.node_id)
                .iter()
                .any(|task| task.service_id == id)
            {
                return Err(held("the service still has tasks on this worker"));
            }
            super::net::remove(&engine.dirs, &service);
            engine.store.remove_service(&id)?;
            engine.clean_task_dirs(&[]);
            Ok(Value::Null)
        }
        Action::Leave => {
            if !engine.store.services()?.is_empty() {
                return Err(held("the worker still has assigned services"));
            }
            if enrolled.own_ingress
                && let Err(e) = crate::network::Network::remove(super::net::INGRESS)
                && e.kind != ErrorKind::NotFound
            {
                return Err(e);
            }
            let mut networks = engine.store.networks()?;
            networks.remove(super::net::INGRESS);
            engine.store.write_networks(&networks)?;
            engine.store.remove_membership()?;
            Ok(Value::Null)
        }
    }
}

fn release_assignment(engine: &Engine, cluster: &Cluster, id: &str, version: u64) -> Result<()> {
    let service = engine
        .store
        .services()?
        .into_iter()
        .find(|record| record.id == id);
    if let Some(service) = &service {
        if placement(service, cluster)? != Placement::Workers || service.version >= version {
            return Err(held(
                "the release does not supersede the current worker assignment",
            ));
        }
    }

    // Fence first, then remove the tasks. A lost SSH reply is safe to retry.
    engine.store.revoke_assignment(id, version)?;
    let grace = service
        .as_ref()
        .map_or(std::time::Duration::from_secs(10), |s| {
            super::tasks::grace(&s.spec)
        });
    super::tasks::release_service(&engine.store, &engine.dirs, id, grace)?;
    if let Some(service) = &service {
        super::net::remove(&engine.dirs, service);
    }
    engine.store.remove_service(id)
}

/// The worker owns batch execution. Repeating the same revision reads or resumes its
/// existing rollout, while old revisions and deleted services can never restart tasks.
fn roll_pinned(
    engine: &Engine,
    cluster: Cluster,
    mut service: ServiceRecord,
    rollback: bool,
) -> Result<Value> {
    if engine.store.service_retired(&service.id)? {
        return Err(held("the worker already removed this service"));
    }
    let previous = engine.find_service(&service.id)?;
    validate_pinned_roll(&cluster, &previous, &service, rollback)?;
    if previous.version > service.version
        || (previous.version == service.version
            && (previous.spec != service.spec || previous.previous_spec != service.previous_spec))
    {
        return Err(held("the worker already has a newer service specification"));
    }
    if previous.version == service.version {
        service = previous;
    } else {
        if update::active(&previous).is_some()
            || service.previous_spec.as_ref() != Some(&previous.spec)
            || update::active(&service) != Some(rollback)
        {
            return Err(held(
                "the worker's current service revision does not match the rollout",
            ));
        }
        service.vips = previous.vips;
        engine.store.write_service(&service)?;
    }
    let status = service
        .update_status
        .clone()
        .ok_or_else(|| held("this service revision has no rolling update"))?;
    if let Some(active) = update::active(&service) {
        if active != rollback {
            return Err(held(
                "this rollout revision has a different update direction",
            ));
        }
        update::spawn_worker(engine.clone(), cluster, &service, rollback);
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn membership() -> Membership {
        Membership {
            cluster_id: "a".repeat(plan::ID_LEN),
            node_id: "b".repeat(plan::ID_LEN),
            own_ingress: false,
        }
    }

    #[test]
    fn ssh_requires_a_known_host_and_cannot_reuse_another_session() {
        let worker = Worker {
            id: membership().node_id,
            name: "worker".into(),
            destination: "user@test-host".into(),
            ssh_config: None,
            created: String::new(),
            updated: None,
            version: initial_worker_version(),
            drained: false,
            host: None,
        };
        let Endpoint::Command(args) = worker.endpoint().unwrap() else {
            panic!("expected an SSH command");
        };
        for option in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "ControlPath=none",
            "ForwardAgent=no",
        ] {
            assert!(args.iter().any(|arg| arg == option), "{args:?}");
        }
        assert!(args.ends_with(
            &["--", "user@test-host", "podbag", "system", "dial-stdio"].map(OsString::from)
        ));
        for invalid in ["-o", "ssh://user@host", "a@b@c", "a;whoami", "a b", "a\n"] {
            assert!(!destination_ok(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn new_slots_balance_and_an_unreachable_owner_keeps_its_slots() {
        let worker = |name: &str| Worker {
            id: name.into(),
            name: name.into(),
            destination: "test-host".into(),
            ssh_config: None,
            created: String::new(),
            updated: None,
            version: initial_worker_version(),
            drained: false,
            host: None,
        };
        let alpha = worker("alpha");
        let beta = worker("beta");
        let mut slots = BTreeMap::new();
        assert_eq!(
            fill_slots(&mut slots, &[beta.clone(), alpha], 4),
            BTreeSet::from(["alpha".into(), "beta".into()])
        );
        assert_eq!(
            slots,
            BTreeMap::from([
                (1, "alpha".into()),
                (2, "beta".into()),
                (3, "alpha".into()),
                (4, "beta".into()),
            ])
        );
        assert_eq!(
            fill_slots(&mut slots, &[beta.clone()], 6),
            BTreeSet::from(["beta".into()])
        );
        assert!(fill_slots(&mut slots, &[beta], 1).is_empty());
        assert_eq!(slots[&1], "alpha");
        assert_eq!(slots[&3], "alpha");
        assert_eq!(slots[&5], "beta");
        assert_eq!(slots[&6], "beta");
    }

    #[test]
    fn a_retired_assignment_rejects_delayed_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let engine = Engine::new(&crate::paths::Dirs {
            data: temp.path().join("data"),
            runtime: temp.path().join("run"),
            config: temp.path().join("config"),
        });
        let member = membership();
        engine.store.write_membership(&member).unwrap();
        let send = |action| {
            handle(
                &engine,
                Wire {
                    member: member.clone(),
                    action,
                },
            )
        };
        let id = "s".repeat(plan::ID_LEN);
        send(Action::RemoveAssigned { id: id.clone() }).unwrap();
        let service = ServiceRecord {
            id,
            version: 1,
            created: String::new(),
            updated: String::new(),
            spec: json!({
                "Name": "pool",
                "TaskTemplate": {"Placement": {"Constraints": ["node.role==worker"]}},
                "Mode": {"Replicated": {"Replicas": 1}},
            }),
            previous_spec: None,
            update_status: None,
            ports: Vec::new(),
            vips: BTreeMap::new(),
            worker_slots: BTreeMap::from([(1, member.node_id.clone())]),
            worker_releases: Default::default(),
            removing: false,
        };
        let error = send(Action::ReconcileAssigned {
            service: Box::new(service),
        })
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conflict);
        assert!(error.why.contains("already removed"));
        assert!(engine.store.services().unwrap().is_empty());
    }

    #[test]
    fn a_released_assignment_rejects_old_revisions_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = crate::paths::Dirs {
            data: temp.path().join("data"),
            runtime: temp.path().join("run"),
            config: temp.path().join("config"),
        };
        let engine = Engine::new(&dirs);
        let member = membership();
        engine.store.write_membership(&member).unwrap();
        let id = "s".repeat(plan::ID_LEN);
        let send = |engine: &Engine, action| {
            handle(
                engine,
                Wire {
                    member: member.clone(),
                    action,
                },
            )
        };
        let reconcile = |version| Action::ReconcileAssigned {
            service: Box::new(ServiceRecord {
                id: id.clone(),
                version,
                created: String::new(),
                updated: String::new(),
                spec: json!({
                    "Name": "pool",
                    "TaskTemplate": {"Placement": {"Constraints": ["node.role==worker"]}},
                    "Mode": {"Replicated": {"Replicas": 0}},
                }),
                previous_spec: None,
                update_status: None,
                ports: Vec::new(),
                vips: BTreeMap::new(),
                worker_slots: BTreeMap::from([(1, member.node_id.clone())]),
                worker_releases: BTreeSet::new(),
                removing: false,
            }),
        };
        let release = |version| Action::ReleaseAssignment {
            id: id.clone(),
            version,
        };

        send(&engine, reconcile(4)).unwrap();
        send(&engine, release(5)).unwrap();
        let engine = Engine::new(&dirs);
        send(&engine, release(5)).unwrap();
        assert!(engine.store.services().unwrap().is_empty());
        for old in [4, 5] {
            let error = send(&engine, reconcile(old)).unwrap_err();
            assert!(error.why.contains("already released"));
        }
        send(&engine, reconcile(6)).unwrap();
        assert!(send(&engine, release(5)).is_err());
        assert_eq!(engine.store.services().unwrap()[0].version, 6);
    }

    #[test]
    fn a_pinned_worker_keeps_newer_state_and_never_restarts_a_removed_rollout() {
        let temp = tempfile::tempdir().unwrap();
        let engine = Engine::new(&crate::paths::Dirs {
            data: temp.path().join("data"),
            runtime: temp.path().join("run"),
            config: temp.path().join("config"),
        });
        let member = membership();
        engine.store.write_membership(&member).unwrap();
        let cluster = member.cluster();
        let service = ServiceRecord {
            id: "s".repeat(plan::ID_LEN),
            version: 2,
            created: String::new(),
            updated: String::new(),
            spec: json!({
                "Name": "pinned",
                "TaskTemplate": {"Placement": {"Constraints": [format!("node.id=={}", member.node_id)]}},
                "Mode": {"Replicated": {"Replicas": 1}},
            }),
            previous_spec: None,
            update_status: Some(update::status(false, update::Stage::Completed, "")),
            ports: Vec::new(),
            vips: BTreeMap::new(),
            worker_slots: BTreeMap::new(),
            worker_releases: Default::default(),
            removing: false,
        };
        engine.store.write_service(&service).unwrap();
        let get = |service| roll_pinned(&engine, cluster.clone(), service, false);
        assert_eq!(get(service.clone()).unwrap()["State"], "completed");
        let mut stale = service.clone();
        stale.version -= 1;
        assert_eq!(get(stale).unwrap_err().kind, ErrorKind::Conflict);
        let mut conflict = service.clone();
        conflict.spec["TaskTemplate"]["ForceUpdate"] = json!(1);
        assert_eq!(get(conflict).unwrap_err().kind, ErrorKind::Conflict);
        assert_eq!(engine.find_service(&service.id).unwrap(), service);
        engine.store.retire_service(&service.id).unwrap();
        engine.store.remove_service(&service.id).unwrap();
        assert_eq!(get(service).unwrap_err().kind, ErrorKind::Conflict);
        assert!(engine.store.services().unwrap().is_empty());
    }

    #[test]
    fn a_worker_refuses_a_different_cluster_and_node_assignment() {
        let temp = tempfile::tempdir().unwrap();
        let engine = Engine::new(&crate::paths::Dirs {
            data: temp.path().join("data"),
            runtime: temp.path().join("run"),
            config: temp.path().join("config"),
        });
        let identity = membership();
        engine.store.write_membership(&identity).unwrap();
        let send = |member| {
            handle(
                &engine,
                Wire {
                    member,
                    action: Action::Probe,
                },
            )
        };
        assert!(send(identity.clone()).is_ok());
        let mut other = identity.clone();
        other.cluster_id = "c".repeat(plan::ID_LEN);
        assert_eq!(send(other).unwrap_err().kind, ErrorKind::Conflict);
        let mut other = identity;
        other.node_id = "c".repeat(plan::ID_LEN);
        assert_eq!(send(other).unwrap_err().kind, ErrorKind::Conflict);
    }
}
