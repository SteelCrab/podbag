//! The swarm's files under `<data>/swarm` (a private directory):
//!
//! - `cluster.json`: the swarm's manager state, present on the manager only.
//! - `workers/<id>.json`: the manager's registered workers and SSH endpoints.
//! - `membership.json`: a worker's assignment to one manager.
//! - `services/<id>.json`: each service's spec, the spec before it, and its update state.
//! - `secrets/<id>.json` and `.data`, `configs/<id>.json` and `.data`: the data files are
//!   0600 and never leave the store except into a task's files.
//! - `networks.json`: the networks made with the `overlay` driver (bridges here).
//! - `tasks/<task id>/`: a task's spec and its secret and config files.
//! - `lock`: held while the files change, so that two changes never mix.

use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The swarm manager and its configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cluster {
    pub id: String,
    pub node_id: String,
    pub created: String,
    pub updated: String,
    /// The swarm object's version.
    pub version: u64,
    /// The last version number handed out to any object.
    pub index: u64,
    /// The swarm spec as Docker shows it (name, raft, dispatcher …).
    pub spec: Value,
    pub node: NodeRecord,
    pub tokens: Tokens,
    /// `host:port` given to `swarm init` (shown, never listened on).
    pub listen_addr: String,
    pub advertise_addr: String,
    pub default_addr_pool: Vec<String>,
    pub subnet_size: u64,
    pub data_path_port: u64,
    /// The `ingress` network was made by `swarm init` (and goes with `swarm leave`).
    #[serde(default)]
    pub own_ingress: bool,
}

/// The node: its spec (labels, role, availability) and versions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub version: u64,
    pub created: String,
    pub updated: String,
    pub spec: Value,
}

/// Docker's join-token shape. Podbag worker enrollment authenticates over SSH.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub worker: String,
    pub manager: String,
}

impl Cluster {
    /// The next version number.
    pub fn next_index(&mut self) -> u64 {
        self.index += 1;
        self.index
    }

    /// Whether the node takes tasks (`docker node update --availability`).
    pub fn active(&self) -> bool {
        !matches!(
            self.node.spec["Availability"].as_str(),
            Some("drain" | "pause")
        )
    }
}

/// A published port as the service shows it: the host port may have been picked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRecord {
    pub protocol: String,
    pub target: u16,
    pub published: u16,
    pub mode: String,
}

/// A service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceRecord {
    pub id: String,
    pub version: u64,
    pub created: String,
    pub updated: String,
    pub spec: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_spec: Option<Value>,
    /// Docker's `UpdateStatus` of the last update or rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_status: Option<Value>,
    #[serde(default)]
    pub ports: Vec<PortRecord>,
    /// The service's virtual address on each of its networks (root mode).
    #[serde(default)]
    pub vips: BTreeMap<String, String>,
    /// Owners of replica slots, including slots reserved after a scale-down.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub worker_slots: BTreeMap<u64, String>,
    /// A distributed service stays stopped while a disconnected worker is cleaned up.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removing: bool,
}

impl ServiceRecord {
    pub fn name(&self) -> &str {
        self.spec["Name"].as_str().unwrap_or_default()
    }
}

/// A secret or a config, without its data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileObject {
    pub id: String,
    pub version: u64,
    pub created: String,
    pub updated: String,
    /// Name, Labels, Driver, Templating: never the data.
    pub spec: Value,
}

impl FileObject {
    pub fn name(&self) -> &str {
        self.spec["Name"].as_str().unwrap_or_default()
    }
}

/// Secrets or configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Secret,
    Config,
}

impl Kind {
    pub fn dir(self) -> &'static str {
        match self {
            Kind::Secret => "secrets",
            Kind::Config => "configs",
        }
    }

    /// The word Docker uses in messages.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Secret => "secret",
            Kind::Config => "config",
        }
    }
}

/// A network made with the `overlay` driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlay {
    #[serde(default)]
    pub attachable: bool,
    #[serde(default)]
    pub ingress: bool,
}

/// The current time as Docker writes it.
pub fn now() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    crate::shim::logbook::rfc3339(t.as_secs(), t.subsec_nanos())
}

/// A new object ID.
pub fn new_id() -> Result<String> {
    let mut bytes = [0u8; super::plan::ID_LEN];
    random(&mut bytes)?;
    Ok(super::plan::object_id(&bytes))
}

/// Random bytes from the kernel.
pub fn random(buf: &mut [u8]) -> Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| {
            Error::io(
                "cannot make a swarm ID",
                &e,
                "check that /dev/urandom is readable",
            )
        })
}

fn fail(path: &Path, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot use {}", path.display()),
        &e,
        "check that the podbag data directory is writable",
    )
}

/// The swarm's directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// Held while the swarm's files change.
pub struct Lock(#[allow(dead_code)] std::fs::File);

impl Store {
    pub fn new(dirs: &Dirs) -> Store {
        Store {
            dir: dirs.data.join("swarm"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn make_dir(path: &Path) -> Result<()> {
        crate::store::perm::private_dir_builder()
            .recursive(true)
            .create(path)
            .map_err(|e| fail(path, e))
    }

    /// Wait for the swarm's lock.
    pub fn lock(&self) -> Result<Lock> {
        Store::make_dir(&self.dir)?;
        let path = self.dir.join("lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| fail(&path, e))?;
        file.lock().map_err(|e| fail(&path, e))?;
        Ok(Lock(file))
    }

    fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                Error::new(
                    ErrorKind::Internal,
                    format!("cannot read {}", path.display()),
                    e.to_string(),
                    "remove the file, or leave the swarm with `docker swarm leave --force`",
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(fail(path, e)),
        }
    }

    /// Write a file whole: a reader sees the old content or the new, never a mix.
    fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = path.parent().unwrap_or(Path::new("/"));
        Store::make_dir(dir)?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let tmp = dir.join(format!(".{name}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| fail(&tmp, e))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| fail(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| fail(path, e))
    }

    fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value).unwrap_or_default();
        Store::write_bytes(path, &bytes)
    }

    fn remove(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(fail(path, e)),
            _ => Ok(()),
        }
    }

    /// Every record in a directory, by ID.
    fn read_all<T: DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(fail(dir, e)),
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
            .collect();
        names.sort();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            if let Some(v) = Store::read(&dir.join(name))? {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// A path for object `id` (checked: IDs become file names).
    fn object_path(&self, dir: &str, id: &str, ext: &str) -> Result<PathBuf> {
        if !super::plan::is_object_id(id) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("invalid swarm ID {id:?}"),
                "a swarm ID is 25 lowercase letters and digits",
                "use the ID that podbag gave",
            ));
        }
        Ok(self.dir.join(dir).join(format!("{id}.{ext}")))
    }

    pub fn cluster(&self) -> Result<Option<Cluster>> {
        Store::read(&self.dir.join("cluster.json"))
    }

    pub fn write_cluster(&self, c: &Cluster) -> Result<()> {
        Store::write(&self.dir.join("cluster.json"), c)
    }

    pub fn workers(&self) -> Result<Vec<super::remote::Worker>> {
        Store::read_all(&self.dir.join("workers"))
    }

    pub fn write_worker(&self, node: &super::remote::Worker) -> Result<()> {
        Store::write(&self.object_path("workers", &node.id, "json")?, node)
    }

    pub fn remove_worker(&self, id: &str) -> Result<()> {
        Store::remove(&self.object_path("workers", id, "json")?)
    }

    pub fn membership(&self) -> Result<Option<super::remote::Membership>> {
        Store::read(&self.dir.join("membership.json"))
    }

    pub fn write_membership(&self, node: &super::remote::Membership) -> Result<()> {
        Store::write(&self.dir.join("membership.json"), node)
    }

    pub fn remove_membership(&self) -> Result<()> {
        Store::remove(&self.dir.join("membership.json"))?;
        let retired = self.dir.join("retired-services");
        match std::fs::remove_dir_all(&retired) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(fail(&retired, error)),
        }
    }

    pub fn services(&self) -> Result<Vec<ServiceRecord>> {
        Store::read_all(&self.dir.join("services"))
    }

    pub fn write_service(&self, s: &ServiceRecord) -> Result<()> {
        Store::write(&self.object_path("services", &s.id, "json")?, s)
    }

    pub fn remove_service(&self, id: &str) -> Result<()> {
        Store::remove(&self.object_path("services", id, "json")?)
    }

    /// Remember the removal so a delayed reconciliation cannot recreate its tasks.
    pub fn retire_service(&self, id: &str) -> Result<()> {
        Store::write(&self.object_path("retired-services", id, "json")?, &true)
    }

    pub fn service_retired(&self, id: &str) -> Result<bool> {
        Ok(Store::read(&self.object_path("retired-services", id, "json")?)?.unwrap_or(false))
    }

    pub fn objects(&self, kind: Kind) -> Result<Vec<FileObject>> {
        Store::read_all(&self.dir.join(kind.dir()))
    }

    /// Write an object, and its data when given (0600, whole).
    pub fn write_object(&self, kind: Kind, o: &FileObject, data: Option<&[u8]>) -> Result<()> {
        if let Some(data) = data {
            Store::write_bytes(&self.object_path(kind.dir(), &o.id, "data")?, data)?;
        }
        Store::write(&self.object_path(kind.dir(), &o.id, "json")?, o)
    }

    pub fn object_data(&self, kind: Kind, id: &str) -> Result<Vec<u8>> {
        let path = self.object_path(kind.dir(), id, "data")?;
        std::fs::read(&path).map_err(|e| fail(&path, e))
    }

    pub fn remove_object(&self, kind: Kind, id: &str) -> Result<()> {
        Store::remove(&self.object_path(kind.dir(), id, "json")?)?;
        Store::remove(&self.object_path(kind.dir(), id, "data")?)
    }

    pub fn networks(&self) -> Result<BTreeMap<String, Overlay>> {
        Ok(Store::read(&self.dir.join("networks.json"))?.unwrap_or_default())
    }

    pub fn write_networks(&self, n: &BTreeMap<String, Overlay>) -> Result<()> {
        Store::write(&self.dir.join("networks.json"), n)
    }

    /// A task's own directory (its spec, secrets and configs).
    pub fn task_dir(&self, task_id: &str) -> Result<PathBuf> {
        Ok(self.object_path("tasks", task_id, "d")?.with_extension(""))
    }

    /// Make a task's directory (private).
    pub fn make_task_dir(&self, task_id: &str) -> Result<PathBuf> {
        let dir = self.task_dir(task_id)?;
        Store::make_dir(&dir)?;
        Ok(dir)
    }

    /// The IDs of the tasks that have a directory.
    pub fn task_dirs(&self) -> Vec<String> {
        std::fs::read_dir(self.dir.join("tasks"))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| super::plan::is_object_id(n))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn remove_task_dir(&self, task_id: &str) {
        if let Ok(dir) = self.task_dir(task_id) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A task's stored spec: (TaskTemplate, EndpointSpec) when it was made.
    pub fn task_spec(&self, task_id: &str) -> Option<TaskSpec> {
        let dir = self.task_dir(task_id).ok()?;
        Store::read(&dir.join("spec.json")).ok().flatten()
    }

    pub fn write_task_spec(&self, task_id: &str, spec: &TaskSpec) -> Result<()> {
        let dir = self.make_task_dir(task_id)?;
        Store::write(&dir.join("spec.json"), spec)
    }

    /// Everything of the swarm, gone.
    pub fn wipe(&self) -> Result<()> {
        match std::fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(fail(&self.dir, e)),
            _ => Ok(()),
        }
    }
}

/// What a task was made from, kept in its directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub service_id: String,
    pub created: String,
    /// The service's `TaskTemplate`.
    pub template: Value,
    /// The service's `EndpointSpec`.
    #[serde(default)]
    pub endpoint: Value,
    /// Its slot number (0 for a global task).
    #[serde(default)]
    pub slot: u64,
    /// Why the task could not start, if it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store(tmp: &Path) -> Store {
        Store::new(&Dirs {
            data: tmp.join("data"),
            runtime: tmp.join("run"),
            config: tmp.join("config"),
        })
    }

    fn id(c: char) -> String {
        c.to_string().repeat(super::super::plan::ID_LEN)
    }

    #[test]
    fn records_round_trip_and_data_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let _lock = s.lock().unwrap();
        assert_eq!(s.cluster().unwrap(), None);
        assert!(s.services().unwrap().is_empty());
        let service = ServiceRecord {
            id: id('a'),
            version: 3,
            created: now(),
            updated: now(),
            spec: json!({"Name": "web"}),
            previous_spec: None,
            update_status: None,
            ports: vec![],
            vips: BTreeMap::new(),
            worker_slots: BTreeMap::from([(1, id('b')), (3, id('c'))]),
            removing: false,
        };
        s.write_service(&service).unwrap();
        assert_eq!(s.services().unwrap(), std::slice::from_ref(&service));
        assert_eq!(s.services().unwrap()[0].name(), "web");
        // Made at run time: no secret-like text in the source.
        let mut data = [0u8; 24];
        random(&mut data).unwrap();
        let secret = FileObject {
            id: id('b'),
            version: 1,
            created: now(),
            updated: now(),
            spec: json!({"Name": "pw"}),
        };
        s.write_object(Kind::Secret, &secret, Some(&data)).unwrap();
        assert_eq!(s.object_data(Kind::Secret, &secret.id).unwrap(), data);
        let path = s.dir().join("secrets").join(format!("{}.data", secret.id));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(s.dir()).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o077, 0, "the swarm directory is private");
        let json =
            std::fs::read_to_string(s.dir().join("secrets").join(format!("{}.json", secret.id)))
                .unwrap();
        assert!(!json.contains("Data"), "{json}");
        s.remove_object(Kind::Secret, &secret.id).unwrap();
        assert!(s.objects(Kind::Secret).unwrap().is_empty());
        assert!(!path.exists());
        // IDs are checked before they become paths.
        assert!(s.remove_service("../x").is_err());
        assert!(s.task_dir("..").is_err());
        s.wipe().unwrap();
        assert!(!s.dir().exists());
    }

    #[test]
    fn task_dirs_hold_specs() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let spec = TaskSpec {
            service_id: id('s'),
            created: now(),
            template: json!({"ContainerSpec": {"Image": "alpine"}}),
            endpoint: json!({"Mode": "vip"}),
            slot: 1,
            error: None,
        };
        s.write_task_spec(&id('t'), &spec).unwrap();
        assert_eq!(s.task_spec(&id('t')), Some(spec));
        assert_eq!(s.task_dirs(), [id('t')]);
        s.remove_task_dir(&id('t'));
        assert!(s.task_dirs().is_empty());
        assert_eq!(s.task_spec(&id('t')), None);
    }
}
