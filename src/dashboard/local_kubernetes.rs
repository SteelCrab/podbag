//! Own one local kind cluster and its kubeconfig.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::kubernetes::host_command;

#[derive(Clone, Copy, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Provider {
    #[default]
    Docker,
    Podbag,
}

#[derive(Debug, PartialEq, Eq)]
struct Engine {
    provider: Provider,
    id: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    name: String,
    version: Option<String>,
    nodes: u8,
    host: String,
    engine_id: String,
    #[serde(default)]
    provider: Provider,
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
    let native = std::env::var("PODBAG_KUBERNETES_NATIVE").as_deref() == Ok("1");
    create_in(&directory()?, docker_host()?, version, nodes, native, &run)
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
    allow_native: bool,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<LocalCluster, String> {
    let _lock = operation(dir)?;
    if read(dir)?.is_some() {
        return Err("Podbag already manages a local cluster".into());
    }
    let engine = engine(&host, allow_native, run)?;
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
        engine_id: engine.id,
        provider: engine.provider,
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
        .write_all(node_config(record.nodes, record.provider).as_bytes())
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
    let current = engine(&record.host, record.provider == Provider::Podbag, run)?;
    if current.id != record.engine_id || current.provider != record.provider {
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

fn node_config(nodes: u8, provider: Provider) -> String {
    let mut config = "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnetworking:\n  apiServerAddress: 127.0.0.1\nnodes:\n".to_owned();
    for index in 0..nodes {
        config.push_str(if index == 0 {
            "- role: control-plane\n"
        } else {
            "- role: worker\n"
        });
        if provider == Provider::Podbag {
            config.push_str("  extraMounts:\n  - hostPath: /dev/kmsg\n    containerPath: /dev/kmsg\n    readOnly: true\n");
        }
    }
    if provider == Provider::Podbag {
        config.push_str("kubeadmConfigPatches:\n- |\n  kind: KubeletConfiguration\n  protectKernelDefaults: true\n");
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

fn engine(
    host: &str,
    allow_native: bool,
    run: &impl Fn(&mut Command) -> Result<String, String>,
) -> Result<Engine, String> {
    if !local_host(host) {
        return Err("Select a local Docker engine before creating a cluster".into());
    }
    let output = run(docker_command("docker", host)?.args([
        "info", "--format", r#"{"os":{{json .OSType}},"id":{{json .ID}},"name":{{json .Name}},"root":{{json .DockerRootDir}},"cgroup":{{json .CgroupVersion}},"security":{{json .SecurityOptions}}}"#,
    ]))?;
    let info: serde_json::Value = serde_json::from_str(&output)
        .map_err(|e| format!("Could not read the local engine information: {e}"))?;
    let id = info["id"].as_str().filter(|s| !s.is_empty());
    if info["os"] != "linux" || id.is_none() {
        return Err("kind needs a local engine running Linux containers".into());
    }
    let id = id.unwrap_or_default();
    if id != "podbag" {
        return Ok(Engine {
            provider: Provider::Docker,
            id: id.into(),
        });
    }
    if !cfg!(target_os = "linux") {
        return Err("kind needs a separate local Docker engine on this platform".into());
    }
    if !allow_native {
        return Err("Select a separate Docker engine or set PODBAG_KUBERNETES_NATIVE=1 for rootful Linux Podbag".into());
    }
    let security = info["security"].as_array();
    if info["cgroup"] != "2"
        || security.is_none_or(|options| options.iter().any(|v| v == "name=rootless"))
    {
        return Err("Local Kubernetes needs a rootful Podbag engine with cgroup v2".into());
    }
    let root = info["root"]
        .as_str()
        .filter(|s| Path::new(s).is_absolute())
        .ok_or("Podbag did not report its local data directory")?;
    let name = info["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Podbag did not report its host name")?;
    Ok(Engine {
        provider: Provider::Podbag,
        id: serde_json::json!(["podbag", name, root]).to_string(),
    })
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
                return Ok(serde_json::json!({"os":"linux", "id":engine.get()}).to_string());
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
        let created = create_in(
            &dir,
            TEST_HOST.into(),
            Some("v1.35.8".into()),
            3,
            false,
            &run,
        )
        .unwrap();
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
            "info" => Ok(serde_json::json!({"os":"linux", "id":"engine-one"}).to_string()),
            "get" => Ok("".into()),
            "create" => Err("kind exited".into()),
            other => panic!("unexpected operation {other}"),
        };
        assert_eq!(
            create_in(dir.path(), TEST_HOST.into(), None, 1, false, &run)
                .err()
                .as_deref(),
            Some("kind exited")
        );
        let record = owned(dir.path()).unwrap();
        assert!(!record.ready);
        assert_eq!(record.engine_id, "engine-one");
    }

    #[test]
    fn rejects_remote_endpoints_and_invalid_settings() {
        assert!(!local_host("ssh://example.com"));
        assert!(!local_host("tcp://127.0.0.1:2375"));
        assert!(!local_host("unix://relative.sock"));
        for version in ["v1", "1.1.1", "v2.1.1", "v1.35.8:other", "v1.35.8\n"] {
            assert!(check_settings(Some(version), 1).is_err(), "{version}");
        }
        assert!(check_settings(None, 0).is_err());
        assert!(check_settings(Some("v1.35.8"), 5).is_ok());
    }

    #[test]
    fn native_kind_requires_explicit_rootful_linux_and_mounts_kmsg_read_only() {
        let info = serde_json::json!({
            "os":"linux", "id":"podbag", "name":"test-guest", "root":"/srv/podbag",
            "cgroup":"2", "security":["name=cgroupns"],
        });
        let query = |_: &mut Command| Ok(info.to_string());
        assert!(engine(TEST_HOST, false, &query).is_err());
        let mut rootless = info.clone();
        rootless["security"] = serde_json::json!(["name=rootless"]);
        assert!(engine(TEST_HOST, true, &|_| Ok(rootless.to_string())).is_err());
        let mut old_cgroup = info.clone();
        old_cgroup["cgroup"] = "1".into();
        assert!(engine(TEST_HOST, true, &|_| Ok(old_cgroup.to_string())).is_err());
        if cfg!(target_os = "linux") {
            let native = engine(TEST_HOST, true, &query).unwrap();
            assert_eq!(native.provider, Provider::Podbag);
            assert_eq!(native.id, r#"["podbag","test-guest","/srv/podbag"]"#);
        }
        let native = node_config(2, Provider::Podbag);
        assert_eq!(native.matches("hostPath: /dev/kmsg").count(), 2);
        assert_eq!(native.matches("readOnly: true").count(), 2);
        assert!(native.contains("protectKernelDefaults: true"));
        assert!(!node_config(2, Provider::Docker).contains("/dev/kmsg"));
    }

    #[test]
    fn kind_creates_and_resets_a_real_cluster() {
        let Ok(version) = std::env::var("PODBAG_TEST_KIND_VERSION") else {
            return;
        };
        check_settings(Some(&version), 2).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let result = (|| -> Result<(), String> {
            let cluster = create_in(dir.path(), docker_host()?, Some(version), 2, false, &run)?;
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

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a prepared rootful Podbag engine, kind, and a private PODBAG_UI_DIR"]
    fn linux_desktop_manages_a_real_native_kind_cluster() {
        assert!(std::env::var_os("PODBAG_UI_DIR").is_some_and(|v| !v.is_empty()));
        assert_eq!(
            std::env::var("PODBAG_KUBERNETES_NATIVE").as_deref(),
            Ok("1")
        );
        assert_eq!(
            engine(&docker_host().unwrap(), true, &run)
                .unwrap()
                .provider,
            Provider::Podbag
        );
        assert!(status().unwrap().is_none());
        let version =
            std::env::var("PODBAG_TEST_KIND_VERSION").unwrap_or_else(|_| "v1.35.8".into());
        let count = std::env::var("PODBAG_TEST_KIND_NODES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let result = (|| -> Result<(), String> {
            let created = create(Some(version), count)?;
            let config = kubeconfig().ok_or("the cluster did not create a private kubeconfig")?;
            let kubectl = |args: &[&str]| -> Result<String, String> {
                run(host_command("kubectl")?
                    .arg("--kubeconfig")
                    .arg(&config)
                    .args(args))
            };
            let verify = || -> Result<(), String> {
                kubectl(&[
                    "wait",
                    "--for=condition=Ready",
                    "node",
                    "--all",
                    "--timeout=60s",
                ])?;
                if kubectl(&["get", "nodes", "-o", "name"])?.lines().count() != usize::from(count) {
                    return Err("the provisioned node count does not match the request".into());
                }
                if kubectl(&["get", "--raw=/readyz"])?.trim() != "ok" {
                    return Err("the Kubernetes API is not ready".into());
                }
                println!("{}", kubectl(&["get", "nodes", "-o", "wide"])?.trim());
                Ok(())
            };
            verify()?;
            println!("Desktop created a cluster on the native Podbag socket.");
            if reset()?.context != created.context {
                return Err("reset changed the owned cluster context".into());
            }
            verify()?;
            println!("Desktop reset the same cluster and its nodes.");
            Ok(())
        })();
        let cleanup = status().unwrap().map(|_| remove());
        result.unwrap();
        if let Some(cleanup) = cleanup {
            cleanup.unwrap();
        }
        assert!(status().unwrap().is_none());
        assert!(!directory().unwrap().join("kubeconfig").exists());
        println!("Desktop removed its cluster and private kubeconfig.");
    }
}
