//! One manager can assign services to Podbag workers reached over authenticated SSH.
//! The remote API remains on the worker's private Unix socket.

use super::ops::{Engine, TaskEntry, host};
use super::plan;
use super::shapes::Host;
use super::store::{self, Cluster, NodeRecord, ServiceRecord, Tokens};
use crate::client::{Client, Endpoint};
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
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
    pub host: Option<Host>,
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

    pub fn check(&self, cluster: &Cluster, service: &ServiceRecord) -> Result<Vec<String>> {
        self.call(
            cluster,
            Action::Check {
                service: Box::new(service.clone()),
            },
        )
    }

    pub fn reconcile(&self, cluster: &Cluster, service: &ServiceRecord) -> Result<()> {
        self.call(
            cluster,
            Action::Reconcile {
                service: Box::new(service.clone()),
            },
        )
    }

    pub fn tasks(&self, cluster: &Cluster) -> Result<Vec<TaskEntry>> {
        self.call(cluster, Action::Tasks)
    }

    pub fn remove_service(&self, cluster: &Cluster, id: &str) -> Result<()> {
        self.call(cluster, Action::Remove { id: id.to_owned() })
    }
}

/// Explicit placement keeps existing services on the manager. The worker ID is stable.
pub fn assignment(service: &ServiceRecord, cluster: &Cluster) -> Result<Option<String>> {
    let placement = &service.spec["TaskTemplate"]["Placement"];
    let list = placement["Constraints"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if list.is_empty() {
        return Ok(None);
    }
    let [rule] = list else {
        return Err(invalid(
            "unsupported Swarm placement",
            "use one node.id==ID constraint",
        ));
    };
    let id = rule
        .as_str()
        .and_then(|s| s.split_once("=="))
        .filter(|(key, _)| key.trim() == "node.id")
        .map(|(_, id)| id.trim())
        .filter(|id| plan::is_object_id(id))
        .ok_or_else(|| {
            invalid(
                "unsupported Swarm placement",
                "use node.id==ID with the full ID",
            )
        })?;
    Ok((id != cluster.node_id).then(|| id.to_string()))
}

pub fn worker_for(
    engine: &Engine,
    cluster: &Cluster,
    service: &ServiceRecord,
) -> Result<Option<Worker>> {
    let Some(id) = assignment(service, cluster)? else {
        return Ok(None);
    };
    engine
        .store
        .workers()?
        .into_iter()
        .find(|worker| worker.id == id && worker.host.is_some())
        .map(Some)
        .ok_or_else(|| {
            invalid(
                "unknown Swarm worker",
                format!("worker {id} is not enrolled"),
            )
        })
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
        if assignment(&service, &cluster)?.as_deref() == Some(worker.id.as_str()) {
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
    Check { service: Box<ServiceRecord> },
    Reconcile { service: Box<ServiceRecord> },
    Tasks,
    Remove { id: String },
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
    match action {
        Action::Preflight | Action::Join => unreachable!("enrollment was handled"),
        Action::Probe => Ok(json!(host())),
        Action::Check { service } => {
            check_remote(&service)?;
            let warnings = engine.check_local(&cluster, &service)?;
            Ok(json!(warnings))
        }
        Action::Reconcile { mut service } => {
            check_remote(&service)?;
            let previous = engine
                .store
                .services()?
                .into_iter()
                .find(|s| s.id == service.id);
            if let Some(previous) = &previous {
                if previous.version > service.version
                    || (previous.version == service.version && previous.spec != service.spec)
                {
                    return Err(held("the worker already has a newer service specification"));
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
        Action::Tasks => Ok(json!(engine.local_task_list(&cluster)?)),
        Action::Remove { id } => {
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
