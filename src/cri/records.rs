//! What the CRI keeps on disk, so that `podbag cri serve` can start again and still know
//! every pod and container: `<data>/cri/sandboxes/<id>/` and `<data>/cri/containers/<id>/`,
//! each with the kubelet's config (protobuf, as it came) and podbag's record (JSON).

use std::path::{Path, PathBuf};

use prost::Message;
use serde::{Deserialize, Serialize};

use super::pb;
use crate::{Error, ErrorKind, Result};

/// A pod sandbox's record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sandbox {
    pub id: String,
    /// Nanoseconds since the epoch.
    pub created: i64,
    pub ready: bool,
    /// The namespaces it holds (bind mounts); None: the host's.
    pub net: Option<String>,
    pub ipc: Option<String>,
    pub uts: Option<String>,
    pub pid: Option<String>,
    /// The process that holds the pod's PID namespace.
    pub pid_holder: Option<u32>,
    /// The pod's /dev/shm.
    pub shm: Option<String>,
    /// The CNI configuration file the network came from, and its result.
    pub cni_file: Option<String>,
    pub cni_result: Option<serde_json::Value>,
    pub ips: Vec<String>,
}

/// A container's record. Its state is podbag's container state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Container {
    pub id: String,
    pub sandbox: String,
    /// Nanoseconds since the epoch.
    pub created: i64,
    /// The image as the kubelet named it, and its ID.
    pub image: String,
    pub image_ref: String,
    pub log_path: Option<String>,
    /// Why its start failed, and when (nanoseconds).
    pub start_error: Option<(i64, String)>,
    /// Its resources as last set (UpdateContainerResources changes them).
    #[serde(skip)]
    pub resources: Option<pb::LinuxContainerResources>,
}

const CONFIG: &str = "config.pb";
const RECORD: &str = "record.json";
const RESOURCES: &str = "resources.pb";

/// The CRI's folder.
#[derive(Debug, Clone)]
pub struct Records {
    root: PathBuf,
}

impl Records {
    pub fn new(root: PathBuf) -> Records {
        Records { root }
    }

    pub fn sandbox_dir(&self, id: &str) -> PathBuf {
        self.root.join("sandboxes").join(id)
    }

    pub fn container_dir(&self, id: &str) -> PathBuf {
        self.root.join("containers").join(id)
    }

    pub fn save_sandbox(&self, s: &Sandbox, config: Option<&pb::PodSandboxConfig>) -> Result<()> {
        let dir = self.sandbox_dir(&s.id);
        make_dir(&dir)?;
        if let Some(c) = config {
            write(&dir.join(CONFIG), &c.encode_to_vec())?;
        }
        write(&dir.join(RECORD), &json(s)?)
    }

    pub fn sandbox(&self, id: &str) -> Result<(Sandbox, pb::PodSandboxConfig)> {
        let dir = self.sandbox_dir(id);
        let record = read_json(&dir.join(RECORD), "pod sandbox", id)?;
        let config = read_pb(&dir.join(CONFIG), "pod sandbox", id)?;
        Ok((record, config))
    }

    pub fn remove_sandbox(&self, id: &str) -> Result<()> {
        remove(&self.sandbox_dir(id))
    }

    pub fn save_container(
        &self,
        c: &Container,
        config: Option<&pb::ContainerConfig>,
    ) -> Result<()> {
        let dir = self.container_dir(&c.id);
        make_dir(&dir)?;
        if let Some(cfg) = config {
            write(&dir.join(CONFIG), &cfg.encode_to_vec())?;
        }
        if let Some(r) = &c.resources {
            write(&dir.join(RESOURCES), &r.encode_to_vec())?;
        }
        write(&dir.join(RECORD), &json(c)?)
    }

    pub fn container(&self, id: &str) -> Result<(Container, pb::ContainerConfig)> {
        let dir = self.container_dir(id);
        let mut record: Container = read_json(&dir.join(RECORD), "container", id)?;
        let config: pb::ContainerConfig = read_pb(&dir.join(CONFIG), "container", id)?;
        record.resources = std::fs::read(dir.join(RESOURCES))
            .ok()
            .and_then(|b| pb::LinuxContainerResources::decode(b.as_slice()).ok())
            .or_else(|| config.linux.as_ref().and_then(|l| l.resources.clone()));
        Ok((record, config))
    }

    pub fn remove_container(&self, id: &str) -> Result<()> {
        remove(&self.container_dir(id))
    }

    pub fn sandbox_ids(&self) -> Vec<String> {
        ids(&self.root.join("sandboxes"))
    }

    pub fn container_ids(&self) -> Vec<String> {
        ids(&self.root.join("containers"))
    }

    /// The full ID that `prefix` starts, among `ids`: the kubelet gives full IDs, crictl
    /// may give a prefix.
    pub fn resolve(ids: &[String], prefix: &str, what: &str) -> Result<String> {
        if prefix.is_empty() {
            return Err(not_found(what, prefix));
        }
        let found: Vec<&String> = ids.iter().filter(|i| i.starts_with(prefix)).collect();
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(not_found(what, prefix)),
            _ => match found.iter().find(|i| i.as_str() == prefix) {
                Some(exact) => Ok((*exact).clone()),
                None => Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("{what} ID {prefix} is ambiguous"),
                    "more than one ID starts with it",
                    "give more of the ID",
                )),
            },
        }
    }
}

pub fn not_found(what: &str, id: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("{what} {id} not found"),
        format!("there is no {what} with this ID"),
        "list them with crictl",
    )
}

fn ids(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join(RECORD).exists())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    out.sort();
    out
}

fn io_error(what: String, e: std::io::Error) -> Error {
    Error::io(what, &e, "check that the podbag data directory is writable")
}

fn make_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| io_error(format!("cannot create {}", dir.display()), e))
}

/// Written whole or not at all.
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| io_error(format!("cannot write {}", path.display()), e))
}

fn json<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "cannot encode a CRI record",
            e.to_string(),
            "report this bug",
        )
    })
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path, what: &str, id: &str) -> Result<T> {
    let bytes = std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => not_found(what, id),
        _ => io_error(format!("cannot read {}", path.display()), e),
    })?;
    serde_json::from_slice(&bytes).map_err(|e| damaged(path, &e.to_string()))
}

fn read_pb<T: Message + Default>(path: &Path, what: &str, id: &str) -> Result<T> {
    let bytes = std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => not_found(what, id),
        _ => io_error(format!("cannot read {}", path.display()), e),
    })?;
    T::decode(bytes.as_slice()).map_err(|e| damaged(path, &e.to_string()))
}

fn damaged(path: &Path, why: &str) -> Error {
    Error::new(
        ErrorKind::Internal,
        format!("{} is damaged", path.display()),
        why,
        "remove the pod with crictl rmp -f",
    )
}

fn remove(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(io_error(format!("cannot remove {}", dir.display()), e))
        }
        _ => Ok(()),
    }
}

/// Nanoseconds since the epoch.
pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Records::new(tmp.path().join("cri"));
        assert!(r.sandbox_ids().is_empty());
        let s = Sandbox {
            id: "s1".into(),
            created: 5,
            ready: true,
            net: Some("/run/x/net".into()),
            ips: vec!["10.0.0.2".into()],
            ..Default::default()
        };
        let config = pb::PodSandboxConfig {
            hostname: "web".into(),
            ..Default::default()
        };
        r.save_sandbox(&s, Some(&config)).unwrap();
        let (back, cfg) = r.sandbox("s1").unwrap();
        assert_eq!(back, s);
        assert_eq!(cfg.hostname, "web");
        assert_eq!(r.sandbox("nope").unwrap_err().kind, ErrorKind::NotFound);

        let c = Container {
            id: "c1".into(),
            sandbox: "s1".into(),
            resources: Some(pb::LinuxContainerResources {
                cpu_shares: 7,
                ..Default::default()
            }),
            ..Default::default()
        };
        r.save_container(&c, Some(&pb::ContainerConfig::default()))
            .unwrap();
        let (back, _) = r.container("c1").unwrap();
        assert_eq!(back.resources.unwrap().cpu_shares, 7);
        assert_eq!(r.container_ids(), ["c1"]);
        r.remove_container("c1").unwrap();
        r.remove_container("c1").unwrap();
        assert!(r.container_ids().is_empty());
    }

    #[test]
    fn ids_resolve_by_prefix() {
        let ids = vec!["abc1".to_string(), "abc2".into(), "abd".into(), "ab".into()];
        assert_eq!(Records::resolve(&ids, "abd", "x").unwrap(), "abd");
        assert_eq!(Records::resolve(&ids, "abc1", "x").unwrap(), "abc1");
        assert_eq!(
            Records::resolve(&ids, "ab", "x").unwrap(),
            "ab",
            "an exact ID wins"
        );
        assert_eq!(
            Records::resolve(&ids, "abc", "x").unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(
            Records::resolve(&ids, "z", "x").unwrap_err().kind,
            ErrorKind::NotFound
        );
        assert_eq!(
            Records::resolve(&ids, "", "x").unwrap_err().kind,
            ErrorKind::NotFound
        );
    }
}
