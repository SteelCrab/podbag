//! What the swarm does: init and leave, services, secrets and configs, and reconcile
//! (the live tasks brought to what the services want). Every change holds the store's
//! lock.

use super::body::Attach;
use super::plan::{self, Live, Slot};
use super::shapes::{Host, TaskFacts};
use super::store::{
    Cluster, FileObject, Kind, NodeRecord, Overlay, ServiceRecord, Store, Tokens, now,
};
use super::tasks::{self, NewTask, Task};
use super::{net, update};
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// The largest secret or config (Docker's limit for secrets).
pub const MAX_DATA: usize = 500 * 1024;

/// A rejected slot is tried again after this long.
const RETRY: Duration = Duration::from_secs(5);

fn invalid(what: impl Into<String>, why: impl Into<String>, next: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, what, why, next)
}

fn not_found(kind: &str, key: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("{kind} {key} not found"),
        format!("no {kind} has this name or ID"),
        format!("list them with `docker {kind} ls`"),
    )
}

fn no_swarm() -> Error {
    Error::new(
        ErrorKind::Conflict,
        "This node is not a swarm manager",
        "the swarm is not active",
        "use `docker swarm init` first",
    )
}

/// A task with what filters need: its facts, its container's labels, whether it was
/// made from the current spec, and its service's name.
pub type TaskEntry = (
    TaskFacts,
    std::collections::BTreeMap<String, String>,
    bool,
    String,
);

/// This host, for the node.
pub fn host() -> Host {
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let memory = read("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024);
    Host {
        hostname: read("/proc/sys/kernel/hostname"),
        architecture: std::env::consts::ARCH.to_string(),
        ncpu: std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
        memory,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The swarm's operations on one data directory.
#[derive(Clone)]
pub struct Engine {
    pub dirs: Dirs,
    pub store: Store,
}

/// A token to show: `SWMTKN-1-` and random letters and digits.
fn token() -> Result<String> {
    let mut a = [0u8; plan::ID_LEN];
    let mut b = [0u8; plan::ID_LEN];
    super::store::random(&mut a)?;
    super::store::random(&mut b)?;
    Ok(format!(
        "SWMTKN-1-{}{}-{}",
        plan::object_id(&a),
        plan::object_id(&b),
        plan::object_id(&a.map(|x| x ^ 0x5a))
    ))
}

/// This host's address toward the outside (no packet is sent), or loopback.
fn outside_address() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// `addr` with a port: its own, or `port`.
fn with_port(addr: &str, port: &str) -> String {
    match addr.rsplit_once(':') {
        Some((_, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => addr.to_string(),
        _ => format!("{addr}:{port}"),
    }
}

/// The spec a swarm gets: what was given over Docker's defaults.
fn swarm_spec(given: &Value, autolock: bool) -> Value {
    let mut spec = json!({
        "Name": "default",
        "Labels": {},
        "Orchestration": {"TaskHistoryRetentionLimit": 5},
        "Raft": {"SnapshotInterval": 10000, "KeepOldSnapshots": 0, "LogEntriesForSlowFollowers": 500, "ElectionTick": 10, "HeartbeatTick": 1},
        "Dispatcher": {"HeartbeatPeriod": 5_000_000_000u64},
        "CAConfig": {"NodeCertExpiry": 7_776_000_000_000_000u64},
        "TaskDefaults": {},
        "EncryptionConfig": {"AutoLockManagers": autolock},
    });
    merge(&mut spec, given);
    spec
}

/// Put `given` over `base`: objects key by key; a null or a zero number keeps the base
/// or an empty string keeps the base (Docker reads them as "the default").
fn merge(base: &mut Value, given: &Value) {
    match (base, given) {
        (Value::Object(b), Value::Object(g)) => {
            for (k, v) in g {
                match b.get_mut(k) {
                    Some(old) if old.is_object() && v.is_object() => merge(old, v),
                    Some(_) if v.is_null() || v.as_f64() == Some(0.0) || v.as_str() == Some("") => {
                    }
                    _ if v.is_null() => {}
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, g) if !g.is_null() => *b = g.clone(),
        _ => {}
    }
}

impl Engine {
    pub fn new(dirs: &Dirs) -> Engine {
        Engine {
            dirs: dirs.clone(),
            store: Store::new(dirs),
        }
    }

    pub fn cluster(&self) -> Result<Option<Cluster>> {
        self.store.cluster()
    }

    fn require(&self) -> Result<Cluster> {
        self.store.cluster()?.ok_or_else(no_swarm)
    }

    // ---- the swarm ----

    /// `POST /swarm/init`: the node ID.
    pub fn init(&self, body: &Value) -> Result<String> {
        let _lock = self.store.lock()?;
        if self.store.cluster()?.is_some() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "This node is already part of a swarm",
                "the swarm is active",
                "use `docker swarm leave` to leave it first",
            ));
        }
        if body["AutoLockManagers"].as_bool().unwrap_or(false) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "cannot lock the swarm",
                "podbag keeps the swarm's state in plain files",
                "leave out --autolock",
            ));
        }
        let listen = body["ListenAddr"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("0.0.0.0:2377");
        let port = listen.rsplit_once(':').map_or("2377", |(_, p)| p);
        let advertise = match body["AdvertiseAddr"].as_str().filter(|s| !s.is_empty()) {
            Some(a) if a.parse::<std::net::IpAddr>().is_ok() || a.contains(':') => {
                with_port(a, port)
            }
            // An interface name, or nothing: the address toward the outside.
            _ => with_port(&outside_address(), port),
        };
        let availability = body["Availability"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("active");
        let at = now();
        let mut cluster = Cluster {
            id: super::store::new_id()?,
            node_id: super::store::new_id()?,
            created: at.clone(),
            updated: at.clone(),
            version: 0,
            index: 0,
            spec: swarm_spec(&body["Spec"], false),
            node: NodeRecord {
                version: 0,
                created: at.clone(),
                updated: at,
                spec: json!({"Labels": {}, "Role": "manager", "Availability": availability}),
            },
            tokens: Tokens {
                worker: token()?,
                manager: token()?,
            },
            listen_addr: listen.to_string(),
            advertise_addr: advertise,
            default_addr_pool: body["DefaultAddrPool"]
                .as_array()
                .filter(|a| !a.is_empty())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec!["10.0.0.0/8".to_string()]),
            subnet_size: body["SubnetSize"].as_u64().filter(|s| *s > 0).unwrap_or(24),
            data_path_port: body["DataPathPort"]
                .as_u64()
                .filter(|s| *s > 0)
                .unwrap_or(4789),
            own_ingress: false,
        };
        cluster.version = cluster.next_index();
        cluster.node.version = cluster.next_index();
        cluster.own_ingress = net::ensure_ingress(&self.store)?;
        self.store.write_cluster(&cluster)?;
        Ok(cluster.node_id)
    }

    /// `POST /swarm/leave?force=1`: every service, task, secret, config and overlay
    /// network goes.
    pub fn leave(&self) -> Result<()> {
        let _lock = self.store.lock()?;
        let cluster = self.require()?;
        let all = tasks::list(&self.dirs, &cluster.node_id);
        for service in self.store.services()? {
            let mine: Vec<&Task> = all.iter().filter(|t| t.service_id == service.id).collect();
            tasks::remove(&self.store, &mine, tasks::grace(&service.spec));
            net::remove(&self.dirs, &service);
            self.store.remove_service(&service.id)?;
        }
        // Tasks of services that are gone already.
        let left = tasks::list(&self.dirs, &cluster.node_id);
        let left: Vec<&Task> = left.iter().collect();
        tasks::remove(&self.store, &left, Duration::from_secs(1));
        net::remove_overlays(&self.store, !cluster.own_ingress);
        self.store.wipe()
    }

    /// Check a version a client sends against the object's.
    fn check_version(given: Option<u64>, current: u64) -> Result<()> {
        match given {
            Some(v) if v != current => Err(invalid(
                "update out of sequence",
                format!("the object is at version {current}, the update was made for {v}"),
                "read the object again and repeat the update",
            )),
            _ => Ok(()),
        }
    }

    /// `POST /swarm/update`.
    pub fn update_swarm(
        &self,
        version: Option<u64>,
        spec: &Value,
        rotate_worker: bool,
        rotate_manager: bool,
    ) -> Result<()> {
        let _lock = self.store.lock()?;
        let mut cluster = self.require()?;
        Engine::check_version(version, cluster.version)?;
        if spec["EncryptionConfig"]["AutoLockManagers"]
            .as_bool()
            .unwrap_or(false)
        {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "cannot lock the swarm",
                "podbag keeps the swarm's state in plain files",
                "leave out --autolock",
            ));
        }
        cluster.spec = swarm_spec(spec, false);
        if rotate_worker {
            cluster.tokens.worker = token()?;
        }
        if rotate_manager {
            cluster.tokens.manager = token()?;
        }
        cluster.version = cluster.next_index();
        cluster.updated = now();
        self.store.write_cluster(&cluster)
    }

    /// `POST /nodes/{id}/update`: labels and availability; the one node stays a manager.
    pub fn update_node(&self, version: Option<u64>, spec: &Value) -> Result<()> {
        let mut cluster = {
            let _lock = self.store.lock()?;
            let mut cluster = self.require()?;
            Engine::check_version(version, cluster.node.version)?;
            if spec["Role"].as_str().is_some_and(|r| r != "manager") {
                return Err(invalid(
                    "cannot demote the node",
                    "it is the only manager of the swarm",
                    "keep it a manager",
                ));
            }
            let availability = spec["Availability"].as_str().unwrap_or("active");
            if !matches!(availability, "active" | "pause" | "drain") {
                return Err(invalid(
                    format!("invalid availability {availability}"),
                    "the availabilities are active, pause and drain",
                    "give one of them",
                ));
            }
            cluster.node.spec = json!({
                "Name": spec["Name"],
                "Labels": spec["Labels"].as_object().cloned().unwrap_or_default(),
                "Role": "manager",
                "Availability": availability,
            });
            if cluster.node.spec["Name"].is_null() {
                cluster.node.spec.as_object_mut().map(|m| m.remove("Name"));
            }
            cluster.node.version = cluster.next_index();
            cluster.node.updated = now();
            self.store.write_cluster(&cluster)?;
            cluster
        };
        self.reconcile_with(&mut cluster)
    }

    // ---- services ----

    /// A service by ID, name or ID prefix.
    pub fn find_service(&self, key: &str) -> Result<ServiceRecord> {
        let all = self.store.services()?;
        if let Some(s) = all.iter().find(|s| s.id == key || s.name() == key) {
            return Ok(s.clone());
        }
        let matching: Vec<&ServiceRecord> = all
            .iter()
            .filter(|s| !key.is_empty() && s.id.starts_with(key))
            .collect();
        match matching.as_slice() {
            [s] => Ok((*s).clone()),
            [] => Err(not_found("service", key)),
            _ => Err(invalid(
                format!("service {key} is ambiguous"),
                "more than one service ID starts with it",
                "give more of the ID",
            )),
        }
    }

    /// Check a spec before a service takes it: the mode, the image (pulled now), the
    /// networks, secrets and configs, and the container a task would get. Returns the
    /// mapping's warnings.
    fn check(&self, cluster: &Cluster, service: &ServiceRecord) -> Result<Vec<String>> {
        let spec = &service.spec;
        if !plan::valid_name(service.name()) {
            return Err(invalid(
                format!("invalid service name {:?}", service.name()),
                "a name has letters and digits, with - or _ between them, at most 63",
                "choose a name like web or app_db",
            ));
        }
        if plan::mode_of(spec).is_none() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "cannot run the service",
                "jobs (replicated-job and global-job modes) are not supported by podbag",
                "use the replicated or global mode",
            ));
        }
        let template = &spec["TaskTemplate"];
        if !template["ContainerSpec"].is_object() {
            return Err(invalid(
                "cannot create the service",
                "its TaskTemplate has no ContainerSpec (plugins and network attachments are not supported)",
                "give a container spec",
            ));
        }
        tasks::check_references(&self.store, &template["ContainerSpec"])?;
        let networks = net::attachments(service)?;
        let image = tasks::image(spec)?;
        let host = host();
        let new = NewTask {
            cluster,
            service,
            slot: Slot::Replica(1),
            networks: &networks,
            publish: false,
            host: &host,
            image: &image,
        };
        let (_, warnings) = tasks::container(&new, &"0".repeat(plan::ID_LEN), Vec::new())?;
        Ok(warnings)
    }

    /// The ports the other services publish.
    fn taken_ports(&self, except: &str) -> Result<Vec<(String, u16)>> {
        Ok(self
            .store
            .services()?
            .iter()
            .filter(|s| s.id != except)
            .flat_map(|s| s.ports.iter().map(|p| (p.protocol.clone(), p.published)))
            .collect())
    }

    fn ports_for(
        &self,
        spec: &Value,
        except: &str,
        previous: &[super::store::PortRecord],
    ) -> Result<Vec<super::store::PortRecord>> {
        let wanted = spec["EndpointSpec"]["Ports"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        plan::assign_ports(&wanted, &self.taken_ports(except)?, previous).map_err(|why| {
            Error::new(
                ErrorKind::Conflict,
                "cannot publish the service's ports",
                why,
                "choose another port",
            )
        })
    }

    /// `POST /services/create`: (ID, warnings). The tasks start before it returns.
    pub fn create_service(&self, given: &Value) -> Result<(String, Vec<String>)> {
        let _lock = self.store.lock()?;
        let mut cluster = self.require()?;
        let spec = plan::with_defaults(given);
        let name = spec["Name"].as_str().unwrap_or_default().to_string();
        if self.store.services()?.iter().any(|s| s.name() == name) {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("service {name} already exists"),
                "the name conflicts with an existing object",
                "choose another name, or update that service",
            ));
        }
        let id = super::store::new_id()?;
        let ports = self.ports_for(&spec, &id, &[])?;
        let at = now();
        let mut service = ServiceRecord {
            id: id.clone(),
            version: 0,
            created: at.clone(),
            updated: at,
            spec,
            previous_spec: None,
            update_status: None,
            ports,
            vips: Default::default(),
        };
        let warnings = self.check(&cluster, &service)?;
        service.version = cluster.next_index();
        self.store.write_cluster(&cluster)?;
        self.store.write_service(&service)?;
        let all = tasks::list(&self.dirs, &cluster.node_id);
        self.reconcile_service(&cluster, &mut service, &all)?;
        Ok((id, warnings))
    }

    /// `POST /services/{id}/update`: a new spec, or the previous one (`rollback`). A
    /// change of the tasks' spec replaces the tasks in a rolling update that goes on after
    /// the call returns; a change of the count only starts or removes tasks.
    pub fn update_service(
        &self,
        key: &str,
        version: Option<u64>,
        given: &Value,
        rollback: bool,
    ) -> Result<Vec<String>> {
        let _lock = self.store.lock()?;
        let mut cluster = self.require()?;
        let mut service = self.find_service(key)?;
        Engine::check_version(version, service.version)?;
        let old = service.spec.clone();
        let new = if rollback {
            service.previous_spec.clone().ok_or_else(|| {
                invalid(
                    format!("cannot roll back service {}", service.name()),
                    "it has no previous spec",
                    "update it first",
                )
            })?
        } else {
            plan::with_defaults(given)
        };
        if new["Name"] != old["Name"] {
            return Err(invalid(
                "cannot update the service",
                "renaming services is not supported",
                "create a new service instead",
            ));
        }
        let (old_mode, new_mode) = (plan::mode_of(&old), plan::mode_of(&new));
        if matches!(old_mode, Some(plan::Mode::Global))
            != matches!(new_mode, Some(plan::Mode::Global))
        {
            return Err(invalid(
                "cannot update the service",
                "service mode change is not allowed",
                "remove the service and create it again in the new mode",
            ));
        }
        let ports = self.ports_for(&new, &service.id, &service.ports)?;
        let mut candidate = service.clone();
        candidate.spec = new.clone();
        candidate.ports = ports.clone();
        let warnings = self.check(&cluster, &candidate)?;
        let tasks_change = new["TaskTemplate"] != old["TaskTemplate"]
            || new["EndpointSpec"] != old["EndpointSpec"];
        service.previous_spec = Some(old);
        service.spec = new;
        service.ports = ports;
        service.version = cluster.next_index();
        service.updated = now();
        if tasks_change {
            service.update_status = Some(update::status(rollback, update::Stage::Started, ""));
        }
        self.store.write_cluster(&cluster)?;
        self.store.write_service(&service)?;
        if tasks_change {
            update::spawn(self.clone(), service.id.clone(), rollback);
        } else {
            let all = tasks::list(&self.dirs, &cluster.node_id);
            self.reconcile_service(&cluster, &mut service, &all)?;
        }
        Ok(warnings)
    }

    /// `DELETE /services/{id}`.
    pub fn remove_service(&self, key: &str) -> Result<()> {
        let _lock = self.store.lock()?;
        let cluster = self.require()?;
        let service = self.find_service(key)?;
        update::forget(&service.id);
        self.store.remove_service(&service.id)?;
        let all = tasks::list(&self.dirs, &cluster.node_id);
        let mine: Vec<&Task> = all.iter().filter(|t| t.service_id == service.id).collect();
        tasks::remove(&self.store, &mine, tasks::grace(&service.spec));
        net::remove(&self.dirs, &service);
        self.clean_task_dirs(&all);
        Ok(())
    }

    /// Whether a task was made from its service's current spec.
    pub fn up_to_date(&self, service: &ServiceRecord, task_id: &str) -> bool {
        self.store.task_spec(task_id).is_some_and(|t| {
            t.template == service.spec["TaskTemplate"] && t.endpoint == service.spec["EndpointSpec"]
        })
    }

    /// The live tasks of a service as reconcile sees them.
    pub fn live(&self, service: &ServiceRecord, tasks: &[&Task]) -> Vec<Live> {
        tasks
            .iter()
            .filter_map(|t| {
                Some(Live {
                    task_id: t.task_id.clone(),
                    slot: t.slot?,
                    up_to_date: self.up_to_date(service, &t.task_id),
                    created: t.state.created,
                })
            })
            .collect()
    }

    /// Bring every service's tasks to what it wants (not those in a rolling update).
    pub fn reconcile(&self) -> Result<()> {
        let _lock = self.store.lock()?;
        let Some(mut cluster) = self.store.cluster()? else {
            return Ok(());
        };
        self.reconcile_with(&mut cluster)
    }

    fn reconcile_with(&self, cluster: &mut Cluster) -> Result<()> {
        let all = tasks::list(&self.dirs, &cluster.node_id);
        for mut service in self.store.services()? {
            if update::running(&service.id) {
                continue;
            }
            // One service's trouble (an image that cannot be pulled) must not stop the
            // others: it shows in its tasks.
            let _ = self.reconcile_service(cluster, &mut service, &all);
        }
        self.clean_task_dirs(&all);
        Ok(())
    }

    /// The slot that publishes the ports rootless: the first one.
    fn port_slot(service: &ServiceRecord) -> Slot {
        match plan::mode_of(&service.spec) {
            Some(plan::Mode::Global) => Slot::Node,
            _ => Slot::Replica(1),
        }
    }

    /// Start one task for `slot`. A slot whose last try failed a moment ago waits.
    pub fn start_task(
        &self,
        cluster: &Cluster,
        service: &ServiceRecord,
        slot: Slot,
        networks: &[Attach],
        image: &crate::Image,
    ) -> Result<String> {
        let host = host();
        let new = NewTask {
            cluster,
            service,
            slot,
            networks,
            publish: super::rootless()
                && !service.ports.is_empty()
                && slot == Engine::port_slot(service),
            host: &host,
            image,
        };
        tasks::create(&self.store, &new)
    }

    /// Rejected tries of a slot: (task ID, when), the newest last.
    fn rejected(&self, service_id: &str, slot: u64) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> =
            self.store
                .task_dirs()
                .into_iter()
                .filter_map(|id| {
                    let spec = self.store.task_spec(&id)?;
                    (spec.error.is_some() && spec.service_id == service_id && spec.slot == slot)
                        .then(|| {
                            (
                                id,
                                crate::api::docker::epoch_of(&spec.created).unwrap_or(0.0),
                            )
                        })
                })
                .collect();
        out.sort_by(|a, b| a.1.total_cmp(&b.1));
        out
    }

    pub fn reconcile_service(
        &self,
        cluster: &Cluster,
        service: &mut ServiceRecord,
        all: &[Task],
    ) -> Result<()> {
        let mode = plan::mode_of(&service.spec).unwrap_or(plan::Mode::Replicated(0));
        let mine: Vec<&Task> = all.iter().filter(|t| t.service_id == service.id).collect();
        let plan = plan::reconcile(mode, cluster.active(), &self.live(service, &mine));
        let gone: Vec<&Task> = mine
            .iter()
            .copied()
            .filter(|t| plan.remove.contains(&t.task_id))
            .collect();
        tasks::remove(&self.store, &gone, tasks::grace(&service.spec));
        let mut result = Ok(());
        if !plan.start.is_empty() {
            let networks = net::attachments(service)?;
            let image = tasks::image(&service.spec)?;
            let now = crate::state::now() as f64;
            for slot in plan.start {
                let mut tries = self.rejected(&service.id, slot.number());
                if tries
                    .last()
                    .is_some_and(|(_, at)| now - at < RETRY.as_secs_f64())
                {
                    continue;
                }
                // Keep the newest rejection to show; the older ones go.
                tries.pop();
                for (id, _) in tries {
                    self.store.remove_task_dir(&id);
                }
                if let Err(e) = self.start_task(cluster, service, slot, &networks, &image) {
                    result = Err(e);
                }
            }
        }
        let live = tasks::list(&self.dirs, &cluster.node_id);
        let mine: Vec<&Task> = live.iter().filter(|t| t.service_id == service.id).collect();
        if net::apply(&self.dirs, service, &mine)? {
            self.store.write_service(service)?;
        }
        result
    }

    /// Task directories whose task is gone (not the rejected ones of live services).
    fn clean_task_dirs(&self, all: &[Task]) {
        let services: Vec<String> = self
            .store
            .services()
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.id)
            .collect();
        // Tasks made since `all` was read count too.
        let live: Vec<String> = tasks::list(&self.dirs, "")
            .into_iter()
            .map(|t| t.task_id)
            .chain(all.iter().map(|t| t.task_id.clone()))
            .collect();
        for id in self.store.task_dirs() {
            if live.contains(&id) {
                continue;
            }
            let keep = self
                .store
                .task_spec(&id)
                .is_some_and(|s| s.error.is_some() && services.contains(&s.service_id));
            if !keep {
                self.store.remove_task_dir(&id);
            }
        }
    }

    /// Every task (live ones and rejected tries), with what filters need: (facts, its
    /// container's labels, made from the current spec, its service's name).
    pub fn task_list(&self) -> Result<Vec<TaskEntry>> {
        let cluster = self.require()?;
        let services = self.store.services()?;
        let mut out = Vec::new();
        let all = tasks::list(&self.dirs, &cluster.node_id);
        for t in &all {
            let service = services.iter().find(|s| s.id == t.service_id);
            let facts = tasks::facts(&self.dirs, &self.store, &cluster, service, t);
            let up = service.is_some_and(|s| self.up_to_date(s, &t.task_id));
            let name = service.map(|s| s.name().to_string()).unwrap_or_default();
            out.push((facts, t.labels.clone(), up, name));
        }
        for id in self.store.task_dirs() {
            if all.iter().any(|t| t.task_id == id) {
                continue;
            }
            let Some(spec) = self.store.task_spec(&id) else {
                continue;
            };
            let Some(service) = services.iter().find(|s| s.id == spec.service_id) else {
                continue;
            };
            if spec.error.is_none() {
                continue;
            }
            let up = spec.template == service.spec["TaskTemplate"];
            let facts = tasks::rejected_facts(&cluster, &id, &spec, service.version);
            out.push((facts, Default::default(), up, service.name().to_string()));
        }
        Ok(out)
    }

    // ---- secrets and configs ----

    pub fn find_object(&self, kind: Kind, key: &str) -> Result<FileObject> {
        let all = self.store.objects(kind)?;
        if let Some(o) = all.iter().find(|o| o.id == key || o.name() == key) {
            return Ok(o.clone());
        }
        let matching: Vec<&FileObject> = all
            .iter()
            .filter(|o| !key.is_empty() && o.id.starts_with(key))
            .collect();
        match matching.as_slice() {
            [o] => Ok((*o).clone()),
            [] => Err(not_found(kind.word(), key)),
            _ => Err(invalid(
                format!("{} {key} is ambiguous", kind.word()),
                "more than one ID starts with it",
                "give more of the ID",
            )),
        }
    }

    /// The data of a create or update body (base64 in `Data`), checked.
    fn data_of(kind: Kind, spec: &Value) -> Result<Option<Vec<u8>>> {
        let Some(text) = spec["Data"].as_str() else {
            return Ok(None);
        };
        let data = super::shapes::base64_decode(text).ok_or_else(|| {
            invalid(
                format!("invalid {} data", kind.word()),
                "the data is not base64",
                "send the data base64-encoded",
            )
        })?;
        if data.len() > MAX_DATA {
            return Err(invalid(
                format!("{} data too long", kind.word()),
                format!("it has {} bytes, the most is {MAX_DATA}", data.len()),
                "use a smaller file",
            ));
        }
        Ok(Some(data))
    }

    /// The spec as stored: without the data.
    fn stored_spec(spec: &Value) -> Value {
        let mut spec = spec.clone();
        if let Some(map) = spec.as_object_mut() {
            map.remove("Data");
            if map.get("Labels").is_none_or(Value::is_null) {
                map.insert("Labels".into(), json!({}));
            }
        }
        spec
    }

    /// `POST /secrets/create` or `/configs/create`: the new ID.
    pub fn create_object(&self, kind: Kind, spec: &Value) -> Result<String> {
        let _lock = self.store.lock()?;
        let mut cluster = self.require()?;
        let name = spec["Name"].as_str().unwrap_or_default();
        let ok_name = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && name.as_bytes()[0].is_ascii_alphanumeric();
        if !ok_name {
            return Err(invalid(
                format!("invalid {} name {name:?}", kind.word()),
                "a name has letters, digits, -, _ and ., starts with a letter or digit, at most 64",
                "choose another name",
            ));
        }
        if spec["Driver"]["Name"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
            || spec["Templating"]["Name"]
                .as_str()
                .is_some_and(|d| !d.is_empty())
        {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("cannot create the {}", kind.word()),
                "drivers and templating are not supported by podbag",
                "give the data itself",
            ));
        }
        if self.store.objects(kind)?.iter().any(|o| o.name() == name) {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("{} {name} already exists", kind.word()),
                "the name conflicts with an existing object",
                "choose another name",
            ));
        }
        let data = Engine::data_of(kind, spec)?.unwrap_or_default();
        let at = now();
        let object = FileObject {
            id: super::store::new_id()?,
            version: cluster.next_index(),
            created: at.clone(),
            updated: at,
            spec: Engine::stored_spec(spec),
        };
        self.store.write_object(kind, &object, Some(&data))?;
        self.store.write_cluster(&cluster)?;
        Ok(object.id)
    }

    /// `POST /secrets/{id}/update`: only the labels change (the same data may come
    /// again, as `docker stack deploy` sends it).
    pub fn update_object(
        &self,
        kind: Kind,
        key: &str,
        version: Option<u64>,
        spec: &Value,
    ) -> Result<()> {
        let _lock = self.store.lock()?;
        let mut cluster = self.require()?;
        let mut object = self.find_object(kind, key)?;
        Engine::check_version(version, object.version)?;
        let same_data = match Engine::data_of(kind, spec)? {
            Some(data) => data == self.store.object_data(kind, &object.id)?,
            None => true,
        };
        if spec["Name"].as_str() != Some(object.name()) || !same_data {
            return Err(invalid(
                format!("cannot update {} {}", kind.word(), object.name()),
                "only updates to Labels are allowed",
                format!("create a new {} under another name", kind.word()),
            ));
        }
        object.spec["Labels"] = spec["Labels"]
            .as_object()
            .cloned()
            .map_or(json!({}), Value::Object);
        object.version = cluster.next_index();
        object.updated = now();
        self.store.write_object(kind, &object, None)?;
        self.store.write_cluster(&cluster)
    }

    /// `DELETE /secrets/{id}`: refused while a service uses it.
    pub fn remove_object(&self, kind: Kind, key: &str) -> Result<()> {
        let _lock = self.store.lock()?;
        self.require()?;
        let object = self.find_object(kind, key)?;
        let (id_key, name_key, list) = match kind {
            Kind::Secret => ("SecretID", "SecretName", "Secrets"),
            Kind::Config => ("ConfigID", "ConfigName", "Configs"),
        };
        let users: Vec<String> = self
            .store
            .services()?
            .iter()
            .filter(|s| {
                s.spec["TaskTemplate"]["ContainerSpec"][list]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|r| r[id_key] == object.id.as_str() || r[name_key] == object.name())
            })
            .map(|s| s.name().to_string())
            .collect();
        if !users.is_empty() {
            return Err(invalid(
                format!(
                    "{} '{}' is in use by the following service: {}",
                    kind.word(),
                    object.name(),
                    users.join(", ")
                ),
                "services still use it",
                "remove it from those services first",
            ));
        }
        self.store.remove_object(kind, &object.id)
    }

    // ---- networks ----

    /// `POST /networks/create` with the overlay driver: a bridge network, marked.
    pub fn create_overlay(
        &self,
        name: &str,
        subnet: Option<&str>,
        labels: crate::network::Labels,
        attachable: bool,
    ) -> Result<crate::network::Network> {
        let _lock = self.store.lock()?;
        self.require()?;
        let network = crate::network::Network::create_with(name, subnet, labels)?;
        net::mark(
            &self.store,
            name,
            Overlay {
                attachable,
                ingress: false,
            },
        )?;
        Ok(network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_spec_keeps_the_defaults() {
        let spec = swarm_spec(
            &json!({"Name": "", "Orchestration": {}, "Raft": {"SnapshotInterval": 0, "ElectionTick": 20},
                    "Dispatcher": {"HeartbeatPeriod": 0}, "Labels": {"a": "b"}, "TaskDefaults": null}),
            false,
        );
        assert_eq!(spec["Orchestration"]["TaskHistoryRetentionLimit"], 5);
        assert_eq!(spec["Raft"]["SnapshotInterval"], 10000);
        assert_eq!(spec["Raft"]["ElectionTick"], 20);
        assert_eq!(spec["Dispatcher"]["HeartbeatPeriod"], 5_000_000_000u64);
        assert_eq!(spec["Labels"]["a"], "b");
        assert_eq!(spec["TaskDefaults"], json!({}));
        assert_eq!(spec["Name"], "default", "an empty name keeps the default");
    }

    #[test]
    fn ports_are_added_to_addresses() {
        assert_eq!(with_port("192.0.2.1", "2377"), "192.0.2.1:2377");
        assert_eq!(with_port("192.0.2.1:4000", "2377"), "192.0.2.1:4000");
    }
}
