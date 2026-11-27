//! Own one local kind cluster and its kubeconfig.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::kubernetes::host_command;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    name: String,
    version: Option<String>,
    nodes: u8,
    host: String,
    engine_id: String,
    ready: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCluster {
    context: String,
    version: Option<String>,
    nodes: u8,
    ready: bool,
}

impl From<Record> for LocalCluster {
    fn from(record: Record) -> Self {
        Self {
            context: format!("kind-{}", record.name),
            version: record.version,
            nodes: record.nodes,
            ready: record.ready,
        }
    }
}

fn directory() -> Result<PathBuf, String> {
    crate::client::ui_dir()
        .map(|dir| dir.join("local-kubernetes"))
        .ok_or_else(|| "Could not find the Podbag configuration folder".into())
}

pub(super) fn kubeconfig() -> Option<PathBuf> {
    let dir = directory().ok()?;
    let config = dir.join("kubeconfig");
    (read(&dir).ok()??.ready && config.is_file()).then_some(config)
}

pub fn status() -> Result<Option<LocalCluster>, String> {
    Ok(read(&directory()?)?.map(LocalCluster::from))
}

pub fn create(version: Option<String>, nodes: u8) -> Result<LocalCluster, String> {
    let version = version.filter(|v| !v.is_empty());
    check_settings(version.as_deref(), nodes)?;
    create_in(&directory()?, docker_host()?, version, nodes, &run)
}

pub fn reset() -> Result<LocalCluster, String> {
    reset_in(&directory()?, &run)
}

pub fn remove() -> Result<(), String> {
    remove_in(&directory()?, &run)
}

fn reset_in(
    dir: &Path,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<LocalCluster, String> {
    let _lock = operation(dir)?;
    let mut record = owned(dir)?;
    check_engine(&record, run)?;
    record.ready = false;
    save(dir, &record)?;
    delete(dir, &record, run)?;
    provision(dir, record, run)
}

fn remove_in(
    dir: &Path,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<(), String> {
    let _lock = operation(dir)?;
    let record = owned(dir)?;
    check_engine(&record, run)?;
    delete(dir, &record, run)?;
    remove_file(&dir.join("kubeconfig"))?;
    remove_file(&dir.join("cluster.json"))
}

fn read(dir: &Path) -> Result<Option<Record>, String> {
    let raw = match fs::read(dir.join("cluster.json")) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let record: Record = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
    let valid_name = record.name.strip_prefix("podbag-").is_some_and(|id| {
        id.len() == 12
            && id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    });
    if !valid_name || !local_host(&record.host) {
        return Err("Invalid local cluster ownership record".into());
    }
    Ok(Some(record))
}

fn owned(dir: &Path) -> Result<Record, String> {
    read(dir)?.ok_or_else(|| "Podbag has no local cluster to manage".into())
}

fn operation(dir: &Path) -> Result<File, String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|e| e.to_string())?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("operation.lock"))
        .map_err(|e| e.to_string())?;
    lock.lock()
        .map_err(|e| format!("Local cluster operation is unavailable: {e}"))?;
    Ok(lock)
}

fn save(dir: &Path, record: &Record) -> Result<(), String> {
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    serde_json::to_writer(file.as_file_mut(), record).map_err(|e| e.to_string())?;
    file.persist(dir.join("cluster.json"))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn create_in(
    dir: &Path,
    host: String,
    version: Option<String>,
    nodes: u8,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<LocalCluster, String> {
    let _lock = operation(dir)?;
    if read(dir)?.is_some() {
        return Err("Podbag already manages a local cluster".into());
    }
    let engine_id = engine_id(&host, run)?;
    let name_file = tempfile::Builder::new()
        .prefix("podbag-")
        .rand_bytes(12)
        .tempfile_in(dir)
        .map_err(|e| e.to_string())?;
    let name = name_file
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_ascii_lowercase();
    let record = Record {
        name,
        version,
        nodes,
        host,
        engine_id,
        ready: false,
    };

    let clusters = run(kind(&record.host)?.args(["get", "clusters"]))?;
    if clusters.lines().any(|line| line.trim() == record.name) {
        return Err("A cluster already has the generated name. Try again.".into());
    }
    // Claim the unique name before starting kind. A failed or interrupted creation
    // can then be reset or removed safely with the same private kubeconfig.
    save(dir, &record)?;
    provision(dir, record, run)
}

fn provision(
    dir: &Path,
    mut record: Record,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<LocalCluster, String> {
    use std::io::Write;
    let mut config = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    config
        .write_all(node_config(record.nodes).as_bytes())
        .map_err(|e| e.to_string())?;
    let mut command = kind(&record.host)?;
    command.args(["create", "cluster", "--name", &record.name, "--wait", "5m"]);
    command.arg("--config").arg(config.path());
    command.arg("--kubeconfig").arg(dir.join("kubeconfig"));
    if let Some(version) = &record.version {
        command.args(["--image", &format!("kindest/node:{version}")]);
    }
    run(&mut command)?;
    if !dir.join("kubeconfig").is_file() {
        return Err("kind returned without creating a kubeconfig".into());
    }
    record.ready = true;
    save(dir, &record)?;
    Ok(record.into())
}

fn check_engine(
    record: &Record,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<(), String> {
    if engine_id(&record.host, run)? != record.engine_id {
        return Err("The local cluster belongs to a different Docker engine".into());
    }
    Ok(())
}

fn delete(
    dir: &Path,
    record: &Record,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<(), String> {
    run(kind(&record.host)?
        .args(["delete", "cluster", "--name", &record.name, "--kubeconfig"])
        .arg(dir.join("kubeconfig")))?;
    Ok(())
}

fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn check_settings(version: Option<&str>, nodes: u8) -> Result<(), String> {
    if nodes == 0 {
        return Err("Choose at least one node".into());
    }
    if let Some(version) = version {
        let valid = version.strip_prefix("v1.").is_some_and(|parts| {
            let Some((minor, patch)) = parts.split_once('.') else {
                return false;
            };
            [minor, patch].iter().all(|part| {
                !part.is_empty() && part.len() <= 3 && part.bytes().all(|c| c.is_ascii_digit())
            })
        });
        if !valid {
            return Err("Use a Kubernetes version such as v1.35.0".into());
        }
    }
    Ok(())
}

fn node_config(nodes: u8) -> String {
    let mut config = "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnetworking:\n  apiServerAddress: 127.0.0.1\nnodes:\n- role: control-plane\n".to_owned();
    for _ in 1..nodes {
        config.push_str("- role: worker\n");
    }
    config
}

fn docker_host() -> Result<String, String> {
    if std::env::var_os("DOCKER_CONTEXT").is_none_or(|s| s.is_empty())
        && let Ok(host) = std::env::var("DOCKER_HOST")
        && !host.is_empty()
    {
        return Ok(host);
    }
    Ok(run(host_command("docker")?.args([
        "context",
        "inspect",
        "--format",
        "{{ (index .Endpoints \"docker\").Host }}",
    ]))?
    .trim()
    .into())
}

fn local_host(host: &str) -> bool {
    host.strip_prefix("unix://")
        .is_some_and(|path| Path::new(path).is_absolute())
        || host
            .strip_prefix("npipe:////./pipe/")
            .is_some_and(|name| !name.is_empty())
}

fn engine_id(
    host: &str,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<String, String> {
    if !local_host(host) {
        return Err("Select a local Docker engine before creating a cluster".into());
    }
    let output =
        run(docker_command("docker", host)?.args(["info", "--format", "{{.OSType}} {{.ID}}"]))?;
    let id = output
        .trim()
        .strip_prefix("linux ")
        .filter(|id| !id.is_empty());
    match id {
        Some("podbag") => Err(
            "kind needs a separate local Docker engine. Select it with docker context use.".into(),
        ),
        Some(id) => Ok(id.into()),
        None => Err("kind needs a local Docker engine running Linux containers".into()),
    }
}

fn docker_command(program: &str, host: &str) -> Result<Command, String> {
    let mut command = host_command(program)?;
    command.env("DOCKER_HOST", host);
    for var in [
        "DOCKER_CONTEXT",
        "DOCKER_TLS",
        "DOCKER_TLS_VERIFY",
        "DOCKER_CERT_PATH",
        "DOCKER_API_VERSION",
    ] {
        command.env_remove(var);
    }
    Ok(command)
}

fn kind(host: &str) -> Result<Command, String> {
    let mut command = docker_command("kind", host)?;
    command.env("KIND_EXPERIMENTAL_PROVIDER", "docker");
    command.env_remove("KIND_EXPERIMENTAL_DOCKER_NETWORK");
    Ok(command)
}

fn run(command: &mut Command) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("Install {program} on the desktop host and try again")
        } else {
            format!("{program}: {e}")
        }
    })?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{program} ({}): {}", output.status, error.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const TEST_HOST: &str = if cfg!(windows) {
        "npipe:////./pipe/podbag-kind-test"
    } else {
        "unix:///test/docker.sock"
    };

    #[test]
    fn creates_resets_and_removes_only_the_owned_cluster() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("managed");
        let external = root.path().join("external-config");
        fs::write(&external, "external kubeconfig").unwrap();
        let calls = RefCell::new(Vec::new());
        let engine = Cell::new("engine-one");
        let run = |command: &mut Command| {
            let program = command.get_program().to_str().unwrap();
            let env = command
                .get_envs()
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(env[std::ffi::OsStr::new("DOCKER_HOST")].unwrap(), TEST_HOST);
            assert_eq!(env[std::ffi::OsStr::new("DOCKER_CONTEXT")], None);
            let args = command
                .get_args()
                .map(|s| s.to_str().unwrap())
                .collect::<Vec<_>>();
            calls.borrow_mut().push(format!("{program} {}", args[0]));
            if program == "docker" {
                assert_eq!(args[0], "info");
                return Ok(format!("linux {}", engine.get()));
            }
            assert_eq!(
                env[std::ffi::OsStr::new("KIND_EXPERIMENTAL_PROVIDER")].unwrap(),
                "docker"
            );
            if args[0] == "get" {
                assert_eq!(args, ["get", "clusters"]);
                return Ok("unowned\n".into());
            }
            let record = owned(&dir).unwrap();
            assert_eq!(args[1..4], ["cluster", "--name", &record.name]);
            let kubeconfig = args[args.iter().position(|&a| a == "--kubeconfig").unwrap() + 1];
            assert_eq!(Path::new(kubeconfig), dir.join("kubeconfig"));
            match args[0] {
                "create" => {
                    assert!(
                        args.windows(2)
                            .any(|pair| pair == ["--image", "kindest/node:v1.35.8"])
                    );
                    let config = args[args.iter().position(|&a| a == "--config").unwrap() + 1];
                    let config = fs::read_to_string(config).unwrap();
                    assert!(config.contains("apiServerAddress: 127.0.0.1"));
                    assert_eq!(config.matches("role: control-plane").count(), 1);
                    assert_eq!(config.matches("role: worker").count(), 2);
                    fs::write(kubeconfig, "local kubeconfig").unwrap();
                }
                "delete" => fs::remove_file(kubeconfig).unwrap(),
                other => panic!("unexpected kind operation {other}"),
            }
            Ok(String::new())
        };
        assert!(remove_in(&dir, &run).is_err());
        assert!(calls.borrow().is_empty());
        let created = create_in(&dir, TEST_HOST.into(), Some("v1.35.8".into()), 3, &run).unwrap();
        assert!(created.ready);
        engine.set("engine-two");
        let err = reset_in(&dir, &run).unwrap_err();
        assert!(err.contains("different Docker engine"), "{err}");
        assert!(dir.join("kubeconfig").exists());
        engine.set("engine-one");
        let reset = reset_in(&dir, &run).unwrap();
        assert_eq!(reset.context, created.context);
        assert!(reset.ready);
        remove_in(&dir, &run).unwrap();
        assert!(read(&dir).unwrap().is_none());
        assert!(!dir.join("kubeconfig").exists());
        assert_eq!(fs::read_to_string(external).unwrap(), "external kubeconfig");
        assert_eq!(
            calls.into_inner(),
            [
                "docker info",
                "kind get",
                "kind create",
                "docker info",
                "docker info",
                "kind delete",
                "kind create",
                "docker info",
                "kind delete"
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(dir.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn interrupted_creation_keeps_a_recoverable_ownership_record() {
        let dir = tempfile::tempdir().unwrap();
        let run = |command: &mut Command| match command.get_args().next().unwrap().to_str().unwrap()
        {
            "info" => Ok("linux engine-one".into()),
            "get" => Ok("".into()),
            "create" => Err("kind exited".into()),
            other => panic!("unexpected operation {other}"),
        };
        assert_eq!(
            create_in(dir.path(), TEST_HOST.into(), None, 1, &run)
                .err()
                .as_deref(),
            Some("kind exited")
        );
        let record = owned(dir.path()).unwrap();
        assert!(!record.ready);
        assert_eq!(record.engine_id, "engine-one");
    }

    #[test]
    fn rejects_remote_endpoints_invalid_settings_and_podbag_as_kind_backend() {
        assert!(!local_host("ssh://example.com"));
        assert!(!local_host("tcp://127.0.0.1:2375"));
        assert!(!local_host("unix://relative.sock"));
        for version in ["v1", "1.1.1", "v2.1.1", "v1.35.8:other", "v1.35.8\n"] {
            assert!(check_settings(Some(version), 1).is_err(), "{version}");
        }
        assert!(check_settings(None, 0).is_err());
        assert!(check_settings(Some("v1.35.8"), 5).is_ok());
        assert!(engine_id(TEST_HOST, &|_| Ok("linux podbag".into())).is_err());
    }

    #[test]
    fn kind_creates_and_resets_a_real_cluster() {
        let Ok(version) = std::env::var("PODBAG_TEST_KIND_VERSION") else {
            return;
        };
        check_settings(Some(&version), 2).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let result = (|| -> Result<(), String> {
            let cluster = create_in(dir.path(), docker_host()?, Some(version), 2, &run)?;
            let config = dir.path().join("kubeconfig");
            let nodes = || -> Result<usize, String> {
                let output = run(host_command("kubectl")?
                    .arg("--kubeconfig")
                    .arg(&config)
                    .args(["get", "nodes", "-o", "name"]))?;
                Ok(output.lines().count())
            };
            if nodes()? != 2 {
                return Err("kind did not create both nodes".into());
            }
            if reset_in(dir.path(), &run)?.context != cluster.context || nodes()? != 2 {
                return Err("kind did not recreate the same cluster and nodes".into());
            }
            Ok(())
        })();
        let cleanup = read(dir.path())
            .unwrap()
            .map(|_| remove_in(dir.path(), &run));
        result.unwrap();
        if let Some(cleanup) = cleanup {
            cleanup.unwrap();
        }
        assert!(read(dir.path()).unwrap().is_none());
    }
}
