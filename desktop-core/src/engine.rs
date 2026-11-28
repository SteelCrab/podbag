//! What the window asks the engine, on a [`Client`]. Tested against a fake engine.

use podbag::client::{
    Choice, Client, ContainerSummary, Endpoint, ImageSummary, Kind, LogLines, NetworkSummary,
    Pruned, Target, VolumeSummary,
};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// The engine, as the window and the tray show it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    /// `linux`, `macos` or `windows`.
    pub platform: &'static str,
    /// The engine this window uses: `podbag`, `docker` or `socket`.
    pub engine: &'static str,
    /// Its name for the user: `podbag`, `Docker` or the socket's path.
    pub engine_name: String,
    pub endpoint: String,
    /// Open shell works: podbag, or the docker CLI is installed.
    pub shell: bool,
    /// The API answered.
    pub reachable: bool,
    /// macOS and Windows: `running`, `stopped` or `not set up`.
    pub vm: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
}

pub fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// The VM's state without waking it: `podbag vm status` (macOS reads files; Windows asks
/// WSL). None on Linux.
pub fn vm_status(podbag: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let _ = podbag;
        podbag::macos::cli::vm_status().ok().map(str::to_string)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new(podbag)
            .args(["vm", "status"])
            .creation_flags(0x0800_0000)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = podbag;
        None
    }
}

/// Find out whether the engine answers. On Linux podbag's service is started; on macOS
/// a stopped VM is left alone unless `start_vm` (a standby that holds the socket starts
/// it at the first request anyway).
pub fn engine(target: &Target, client: &Client, podbag: &Path, start_vm: bool) -> EngineStatus {
    let mut status = EngineStatus {
        platform: platform(),
        engine: engine_kind(&target.choice),
        engine_name: target.choice.label(),
        endpoint: client.endpoint.describe(),
        shell: target
            .cli(podbag, podbag::client::choice::find_docker_cli().as_deref())
            .is_some(),
        ..EngineStatus::default()
    };
    let own = target.choice == Choice::Podbag
        && matches!(client.endpoint, Endpoint::Socket { own: true, .. });
    if own {
        status.vm = vm_status(podbag);
    }
    let socket_missing =
        matches!(&client.endpoint, Endpoint::Socket { path, .. } if !path.exists());
    if cfg!(target_os = "macos") && own && socket_missing && !start_vm {
        status.error = Some("vm-stopped".into());
        return status;
    }
    if let Err(e) = target.ensure_running(podbag, start_vm) {
        status.error = Some(e);
        return status;
    }
    match client.version() {
        Ok(v) => {
            status.reachable = true;
            let name = v["Platform"]["Name"].as_str().unwrap_or("Engine");
            status.version = Some(format!(
                "{name} {} (API {})",
                v["Version"].as_str().unwrap_or("?"),
                v["ApiVersion"].as_str().unwrap_or("?")
            ));
        }
        Err(e) => status.error = Some(e.message),
    }
    if own {
        status.vm = vm_status(podbag).or(status.vm);
    }
    status
}

/// `podbag`, `docker` or `socket`.
pub fn engine_kind(choice: &Choice) -> &'static str {
    match choice {
        Choice::Podbag => "podbag",
        Choice::Docker => "docker",
        Choice::Socket(_) => "socket",
        Choice::Kiri => "kiri",
        Choice::Podman => "podman",
        Choice::Context(_) => "context",
    }
}

/// Switch this window (and the TUI, which reads the same file) to `podbag` or `docker`:
/// saved in `file` (`ui.json`), then found.
pub fn choose(name: &str, file: &Path, podbag: &Path) -> Result<Target, String> {
    let choice = Choice::parse(name).ok_or_else(|| format!("unknown engine {name}"))?;
    let target = Target::find(Some(&choice), podbag)?;
    podbag::client::choice::save_in(file, &choice)?;
    Ok(target)
}

/// The kind named `name` (`containers`, `images`, `volumes`, `networks`).
pub fn parse_kind(name: &str) -> Result<Kind, String> {
    Kind::ALL
        .into_iter()
        .find(|k| k.segment() == name)
        .ok_or_else(|| format!("unknown kind {name}"))
}

/// A list, as the engine gives it. Serialized, it is the bare array with Docker's field
/// names.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum List {
    Containers(Vec<ContainerSummary>),
    Images(Vec<ImageSummary>),
    Volumes(Vec<VolumeSummary>),
    Networks(Vec<NetworkSummary>),
}

impl List {
    /// How many containers run (0 for other lists).
    pub fn running(&self) -> usize {
        match self {
            List::Containers(list) => list.iter().filter(|c| c.state == "running").count(),
            _ => 0,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            List::Containers(l) => l.len(),
            List::Images(l) => l.len(),
            List::Volumes(l) => l.len(),
            List::Networks(l) => l.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn list(client: &Client, kind: Kind) -> Result<List, String> {
    let list = match kind {
        Kind::Containers => List::Containers(client.containers().map_err(|e| e.message)?),
        Kind::Images => List::Images(client.images().map_err(|e| e.message)?),
        Kind::Volumes => List::Volumes(client.volumes().map_err(|e| e.message)?),
        Kind::Networks => List::Networks(client.networks().map_err(|e| e.message)?),
    };
    Ok(list)
}

pub fn inspect(client: &Client, kind: Kind, id: &str) -> Result<Value, String> {
    client.inspect(kind, id).map_err(|e| e.message)
}

/// `start`, `stop`, `restart`, `pause` or `unpause`: nothing else is sent.
pub fn act(client: &Client, id: &str, action: &str) -> Result<(), String> {
    if !matches!(action, "start" | "stop" | "restart" | "pause" | "unpause") {
        return Err(format!("unknown action {action}"));
    }
    client.container_action(id, action).map_err(|e| e.message)
}

pub fn remove(client: &Client, kind: Kind, id: &str, force: bool) -> Result<(), String> {
    client.remove(kind, id, force).map_err(|e| e.message)
}

pub fn prune(client: &Client, kind: Kind) -> Result<Pruned, String> {
    client.prune(kind).map_err(|e| e.message)
}

pub fn rename(client: &Client, id: &str, name: &str) -> Result<(), String> {
    client.rename(id, name).map_err(|e| e.message)
}

/// `GET /containers/{id}/stats` or `top`, where the engine has them.
pub fn container_json(client: &Client, id: &str, what: &str) -> Result<Value, String> {
    if !matches!(what, "stats" | "top") {
        return Err(format!("unknown request {what}"));
    }
    client.container_json(id, what).map_err(|e| e.message)
}

/// Send log lines in batches: what came within 100 ms, at most 500 lines. `emit` returns
/// false to stop.
pub fn follow_logs(lines: LogLines, mut emit: impl FnMut(Vec<String>) -> bool + Send + 'static) {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in lines {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut batch = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                batch.push(line);
                if batch.len() < 500 {
                    continue;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if !batch.is_empty() {
                    emit(batch);
                }
                return;
            }
        }
        if !batch.is_empty() && !emit(std::mem::take(&mut batch)) {
            return;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, small_engine};

    #[test]
    fn engine_status_from_version() {
        let engine = FakeEngine::start(|_, path| {
            if path == "/version" {
                podbag::client::fake::json(
                    200,
                    serde_json::json!({"Platform": {"Name": "podbag"}, "Version": "0.1.0",
                                       "ApiVersion": "1.47"}),
                )
            } else {
                small_engine("GET", path)
            }
        });
        let target = Target {
            choice: Choice::Podbag,
            endpoint: engine.client.endpoint.clone(),
        };
        let s = super::engine(
            &target,
            &engine.client,
            Path::new("/nonexistent/podbag"),
            false,
        );
        assert!(s.reachable, "{s:?}");
        assert_eq!(s.engine, "podbag");
        assert_eq!(s.version.as_deref(), Some("podbag 0.1.0 (API 1.47)"));
        assert_eq!(s.vm, None);
        assert!(s.endpoint.starts_with("unix://"));
    }

    #[test]
    fn no_engine_is_an_error_not_a_panic() {
        let target = Target {
            choice: Choice::Socket("/nonexistent/docker.sock".into()),
            endpoint: Endpoint::Socket {
                path: "/nonexistent/docker.sock".into(),
                own: false,
            },
        };
        let client = Client::new(target.endpoint.clone());
        let s = super::engine(&target, &client, Path::new("/nonexistent/podbag"), false);
        assert!(!s.reachable);
        assert!(s.error.unwrap().contains("nothing answers"));
    }

    #[test]
    fn lists_keep_docker_field_names() {
        let engine = FakeEngine::start(small_engine);
        let json = |kind: &str| {
            serde_json::to_value(list(&engine.client, parse_kind(kind).unwrap()).unwrap()).unwrap()
        };
        let c = json("containers");
        assert_eq!(c[0]["Names"][0], "/web");
        assert_eq!(c[0]["Ports"][0]["PublicPort"], 8080);
        assert_eq!(list(&engine.client, Kind::Containers).unwrap().running(), 1);
        assert_eq!(json("images")[0]["RepoTags"][0], "nginx:alpine");
        assert_eq!(json("volumes")[0]["Name"], "pgdata");
        assert_eq!(
            json("networks")[1]["IPAM"]["Config"][0]["Subnet"],
            "10.77.1.0/24"
        );
        assert_eq!(parse_kind("pods").unwrap_err(), "unknown kind pods");
    }

    #[test]
    fn actions_send_only_what_is_allowed() {
        let engine = FakeEngine::start(small_engine);
        let c = &engine.client;
        act(c, "web", "restart").unwrap();
        assert!(act(c, "web", "kill?signal=9").is_err());
        let e = act(c, "web", "pause").unwrap_err();
        assert!(e.contains("not supported"), "{e}");
        remove(c, Kind::Volumes, "pgdata", false).unwrap();
        assert_eq!(prune(c, Kind::Containers).unwrap().count, 1);
        assert!(container_json(c, "web", "logs").is_err());
        assert!(
            container_json(c, "web", "stats")
                .unwrap_err()
                .contains("not supported")
        );
        assert_eq!(
            engine.requests(),
            [
                "POST /containers/web/restart",
                "POST /containers/web/pause",
                "DELETE /volumes/pgdata",
                "POST /containers/prune",
                "GET /containers/web/stats?stream=0",
            ]
        );
    }

    #[test]
    fn log_lines_come_in_batches() {
        let engine = FakeEngine::start(small_engine);
        let lines = engine.client.logs("web", 100, false).unwrap();
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = got.clone();
        follow_logs(lines, move |batch| {
            sink.lock().unwrap().push(batch);
            true
        });
        let got = got.lock().unwrap();
        assert_eq!(got.concat(), ["listening on :80", "GET / 200"]);
    }

    fn docker_at(path: impl Into<std::path::PathBuf>) -> Target {
        Target {
            choice: Choice::Docker,
            endpoint: Endpoint::Socket {
                path: path.into(),
                own: false,
            },
        }
    }

    #[test]
    fn docker_that_does_not_answer_is_never_started() {
        let target = docker_at("/nonexistent/docker.sock");
        let client = Client::new(target.endpoint.clone());
        let s = super::engine(&target, &client, Path::new("/bin/false"), true);
        assert!(!s.reachable);
        assert_eq!(s.engine, "docker");
        assert_eq!(
            s.error.as_deref(),
            Some(podbag::client::choice::docker_down())
        );
        assert_eq!(s.vm, None);
    }

    #[test]
    fn docker_that_answers_is_used() {
        let engine = FakeEngine::start(|method, path| {
            if path == "/version" {
                podbag::client::fake::json(
                    200,
                    serde_json::json!({"Platform": {"Name": "Docker Desktop 4.50.0"},
                                       "Version": "29.0.1", "ApiVersion": "1.52"}),
                )
            } else {
                small_engine(method, path)
            }
        });
        let Endpoint::Socket { path, .. } = &engine.client.endpoint else {
            unreachable!()
        };
        let target = docker_at(path.clone());
        let s = super::engine(
            &target,
            &engine.client,
            Path::new("/nonexistent/podbag"),
            false,
        );
        assert!(s.reachable, "{s:?}");
        assert_eq!(s.engine, "docker");
        assert_eq!(s.vm, None, "Docker has no podbag VM");
    }

    #[test]
    fn the_choice_is_saved_for_both_uis() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ui.json");
        let podbag = Path::new("/nonexistent/podbag");
        let t = choose("docker", &file, podbag).unwrap();
        assert_eq!(t.choice, Choice::Docker);
        assert_eq!(
            podbag::client::choice::saved_in(&file),
            Some(Choice::Docker)
        );
        let t = choose("podbag", &file, podbag).unwrap();
        assert_eq!(t.choice, Choice::Podbag);
        assert!(matches!(t.endpoint, Endpoint::Socket { own: true, .. }));
        assert_eq!(
            podbag::client::choice::saved_in(&file),
            Some(Choice::Podbag)
        );
        assert!(choose("bad-engine", &file, podbag).is_err());
        assert_eq!(
            podbag::client::choice::saved_in(&file),
            Some(Choice::Podbag)
        );
    }
}
