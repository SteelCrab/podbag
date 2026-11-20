//! What the window asks for, on a [`Client`]: no Tauri here, so it is tested against a
//! fake engine. `main.rs` wraps these in Tauri commands.

use podbag::client::{Choice, Client, Endpoint, Kind, LogLines, Pruned, Target};
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

fn kind(name: &str) -> Result<Kind, String> {
    Kind::ALL
        .into_iter()
        .find(|k| k.segment() == name)
        .ok_or_else(|| format!("unknown kind {name}"))
}

/// A list, as the engine gives it (Docker's field names).
pub fn list(client: &Client, what: &str) -> Result<Value, String> {
    let json = |v: Result<Value, serde_json::Error>| v.map_err(|e| e.to_string());
    match kind(what)? {
        Kind::Containers => json(serde_json::to_value(
            client.containers().map_err(|e| e.message)?,
        )),
        Kind::Images => json(serde_json::to_value(
            client.images().map_err(|e| e.message)?,
        )),
        Kind::Volumes => json(serde_json::to_value(
            client.volumes().map_err(|e| e.message)?,
        )),
        Kind::Networks => json(serde_json::to_value(
            client.networks().map_err(|e| e.message)?,
        )),
    }
}

pub fn inspect(client: &Client, what: &str, id: &str) -> Result<Value, String> {
    client.inspect(kind(what)?, id).map_err(|e| e.message)
}

/// `start`, `stop`, `restart`, `pause` or `unpause`: nothing else is sent.
pub fn act(client: &Client, id: &str, action: &str) -> Result<(), String> {
    if !matches!(action, "start" | "stop" | "restart" | "pause" | "unpause") {
        return Err(format!("unknown action {action}"));
    }
    client.container_action(id, action).map_err(|e| e.message)
}

pub fn remove(client: &Client, what: &str, id: &str, force: bool) -> Result<(), String> {
    client.remove(kind(what)?, id, force).map_err(|e| e.message)
}

pub fn prune(client: &Client, what: &str) -> Result<Pruned, String> {
    client.prune(kind(what)?).map_err(|e| e.message)
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

/// How many containers run.
pub fn running(list: &Value) -> usize {
    list.as_array().map_or(0, |a| {
        a.iter()
            .filter(|c| c["State"].as_str() == Some("running"))
            .count()
    })
}

/// A container id or name that is safe in a command line.
fn safe_id(id: &str) -> Result<&str, String> {
    if !id.starts_with('-')
        && !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
    {
        Ok(id)
    } else {
        Err(format!("invalid container id {id:?}"))
    }
}

/// A shell in a container: the command, and what to type by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub argv: Vec<String>,
    pub by_hand: String,
}

/// `podbag exec -it <id> sh` for podbag; `docker exec -it <id> sh` for another engine,
/// with `DOCKER_HOST` set to its socket.
pub fn shell_command(
    target: &Target,
    podbag: &Path,
    docker: &Path,
    id: &str,
) -> Result<Shell, String> {
    let id = safe_id(id)?;
    let tail = ["exec", "-it", id, "sh"].map(String::from);
    if target.choice == Choice::Podbag {
        let mut argv = vec![podbag.display().to_string()];
        argv.extend(tail);
        return Ok(Shell {
            argv,
            by_hand: format!("podbag exec -it {id} sh"),
        });
    }
    let mut argv: Vec<String> = target
        .cli(podbag, Some(docker))
        .ok_or("No CLI for this engine")?
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    argv.extend(tail);
    let quoted: Vec<_> = argv
        .iter()
        .map(|arg| {
            if cfg!(windows) {
                format!("'{}'", arg.replace('\'', "''"))
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect();
    let line = quoted.join(" ");
    Ok(Shell {
        argv,
        by_hand: if cfg!(windows) {
            format!("& {line}")
        } else {
            line
        },
    })
}

/// Open a terminal window with a shell in the container. When none can be opened, the
/// error is the command to run by hand.
pub fn open_shell(shell: &Shell) -> Result<(), String> {
    if open_terminal(Some(&shell.argv)) {
        Ok(())
    } else {
        Err(shell.by_hand.clone())
    }
}

/// Open a terminal window that runs `argv` (None: a plain shell). False when no
/// terminal app was found.
pub fn open_terminal(argv: Option<&[String]>) -> bool {
    let Some(argv) = argv else {
        return if cfg!(target_os = "macos") {
            std::process::Command::new("open")
                .args(["-a", "Terminal"])
                .spawn()
                .is_ok()
        } else if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "start", "cmd"])
                .spawn()
                .is_ok()
        } else {
            linux_terminal(&[])
        };
    };
    let quoted: Vec<String> = argv
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect();
    let line = quoted.join(" ");
    if cfg!(target_os = "macos") {
        let script = format!(
            "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
            line.replace('\\', "\\\\").replace('"', "\\\"")
        );
        std::process::Command::new("osascript")
            .args(["-e", &script])
            .spawn()
            .is_ok()
    } else if cfg!(windows) {
        {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .creation_flags(0x10)
                    .spawn()
                    .is_ok()
            }
            #[cfg(not(windows))]
            {
                false
            }
        }
    } else {
        linux_terminal(argv)
    }
}

/// The first terminal found: $TERMINAL, then the common ones.
fn linux_terminal(argv: &[String]) -> bool {
    let mut candidates: Vec<(String, &str)> = Vec::new();
    if let Ok(t) = std::env::var("TERMINAL") {
        candidates.push((t, "-e"));
    }
    for (t, flag) in [
        ("x-terminal-emulator", "-e"),
        ("gnome-terminal", "--"),
        ("konsole", "-e"),
        ("xfce4-terminal", "-x"),
        ("kitty", "--"),
        ("alacritty", "-e"),
        ("foot", "--"),
        ("xterm", "-e"),
    ] {
        candidates.push((t.to_string(), flag));
    }
    candidates.into_iter().any(|(t, flag)| {
        let mut cmd = std::process::Command::new(&t);
        if !argv.is_empty() {
            cmd.arg(flag).args(argv);
        }
        cmd.spawn().is_ok()
    })
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
        let c = list(&engine.client, "containers").unwrap();
        assert_eq!(c[0]["Names"][0], "/web");
        assert_eq!(c[0]["Ports"][0]["PublicPort"], 8080);
        assert_eq!(running(&c), 1);
        assert_eq!(
            list(&engine.client, "images").unwrap()[0]["RepoTags"][0],
            "nginx:alpine"
        );
        assert_eq!(
            list(&engine.client, "volumes").unwrap()[0]["Name"],
            "pgdata"
        );
        assert_eq!(
            list(&engine.client, "networks").unwrap()[1]["IPAM"]["Config"][0]["Subnet"],
            "10.77.1.0/24"
        );
        assert!(list(&engine.client, "pods").is_err());
    }

    #[test]
    fn actions_send_only_what_is_allowed() {
        let engine = FakeEngine::start(small_engine);
        let c = &engine.client;
        act(c, "web", "restart").unwrap();
        assert!(act(c, "web", "kill?signal=9").is_err());
        let e = act(c, "web", "pause").unwrap_err();
        assert!(e.contains("not supported"), "{e}");
        remove(c, "volumes", "pgdata", false).unwrap();
        assert_eq!(prune(c, "containers").unwrap().count, 1);
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

    #[test]
    fn docker_shells_use_the_docker_cli_and_its_socket() {
        let target = docker_at("/x/docker.sock");
        let shell = shell_command(
            &target,
            Path::new("/opt/podbag"),
            Path::new("/usr/bin/docker"),
            "web",
        )
        .unwrap();
        assert_eq!(
            shell.argv,
            [
                "/usr/bin/docker",
                "--host=unix:///x/docker.sock",
                "exec",
                "-it",
                "web",
                "sh"
            ]
        );
        assert!(shell.by_hand.contains("--host=unix:///x/docker.sock"));
        let podbag = Target {
            choice: Choice::Podbag,
            endpoint: Endpoint::Socket {
                path: "/p.sock".into(),
                own: true,
            },
        };
        let shell = shell_command(
            &podbag,
            Path::new("/opt/podbag"),
            Path::new("docker"),
            "web",
        )
        .unwrap();
        assert_eq!(shell.argv, ["/opt/podbag", "exec", "-it", "web", "sh"]);
        assert_eq!(shell.by_hand, "podbag exec -it web sh");
    }

    #[test]
    fn shell_commands_take_only_plain_ids() {
        let t = Target {
            choice: Choice::Podbag,
            endpoint: Endpoint::Socket {
                path: "/p.sock".into(),
                own: true,
            },
        };
        let docker = Path::new("docker");
        let shell = shell_command(&t, Path::new("/usr/local/bin/podbag"), docker, "web-1").unwrap();
        assert_eq!(
            shell.argv,
            ["/usr/local/bin/podbag", "exec", "-it", "web-1", "sh"]
        );
        assert!(shell_command(&t, Path::new("podbag"), docker, "web; rm -rf /").is_err());
        assert!(shell_command(&t, Path::new("podbag"), docker, "").is_err());
    }
}
