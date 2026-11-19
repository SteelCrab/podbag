#![cfg(target_os = "linux")]
//! Containers through the Docker Engine API: the docker CLI against `openpod system service`.

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

struct Setup {
    _reg: FakeRegistry,
    sb: Sandbox,
    service: Service,
    image: String,
}

/// A service and an alpine image in the fake registry, not pulled yet.
fn unpulled() -> Setup {
    let reg = FakeRegistry::start(false);
    reg.push(
        "img",
        "v1",
        &TestImage {
            layers: vec![alpine_layer()],
            config: serde_json::json!({"Env": [PATH], "Cmd": ["sh"]}),
        },
    );
    let image = format!("{}/img:v1", reg.host);
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    Setup {
        _reg: reg,
        sb,
        service,
        image,
    }
}

fn setup() -> Setup {
    let s = unpulled();
    s.ok(&["pull", &s.image]);
    s
}

impl Setup {
    fn cmd(&self, args: &[&str]) -> Command {
        docker(&self.sb, &self.service, args)
    }

    fn docker(&self, args: &[&str]) -> Output {
        docker_out(&self.sb, &self.service, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.docker(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "docker {args:?}: {}",
            stderr(&out)
        );
        stdout(&out)
    }

    /// The docker CLI killed with SIGKILL after `secs`: it cannot close anything itself.
    fn killed_after(&self, secs: &str, args: &[&str]) -> Output {
        docker_killed_after(&self.sb, &self.service, secs, args)
    }
}

#[test]
fn docker_run_rm_prints_output_and_exit_code() {
    let s = unpulled();
    let out = s.docker(&[
        "run",
        "--rm",
        &s.image,
        "sh",
        "-c",
        "echo out; echo err >&2; exit 3",
    ]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "out");
    let err = stderr(&out);
    assert!(
        err.contains("Unable to find image"),
        "pulled on demand: {err}"
    );
    assert!(err.lines().any(|l| l == "err"), "{err}");
    assert!(
        wait_for(Duration::from_secs(5), || s.ok(&["ps", "-aq"]).is_empty()),
        "--rm left the container"
    );
}

#[test]
fn docker_run_d_ps_logs_stop_rm() {
    let s = setup();
    let id = s.ok(&[
        "run",
        "-d",
        "--name",
        "web",
        &s.image,
        "sh",
        "-c",
        "echo started; trap 'exit 0' TERM; while true; do sleep 0.1; done",
    ]);
    assert_eq!(id.len(), 64, "{id}");
    let ps = s.ok(&["ps", "--format", "{{.Names}} {{.Status}}"]);
    assert!(ps.lines().any(|l| l.starts_with("web Up")), "{ps}");
    assert!(wait_for(Duration::from_secs(5), || s
        .ok(&["logs", "web"])
        .contains("started")));
    s.ok(&["stop", "-t", "1", "web"]);
    let ps = s.ok(&["ps", "-a", "--format", "{{.Names}} {{.Status}}"]);
    assert!(ps.lines().any(|l| l.starts_with("web Exited (0)")), "{ps}");
    assert_eq!(s.ok(&["ps", "-q"]), "", "not running any more");
    s.ok(&["rm", "web"]);
    assert_eq!(s.ok(&["ps", "-aq"]), "");
}

#[test]
fn docker_run_interactive_reads_stdin() {
    let s = setup();
    let mut child = s
        .cmd(&["run", "-i", "--rm", &s.image, "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "hello");
}

#[test]
fn docker_run_tty_through_script() {
    let s = setup();
    let config = s.sb.tmp.path().join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    let docker = if root_mode() {
        format!("sudo -n env DOCKER_CONFIG={} docker", config.display())
    } else {
        format!("env DOCKER_CONFIG={} docker", config.display())
    };
    let line = format!(
        "{docker} -H unix://{} run -it --rm {} sh -c 'stty size; echo tty-ok'",
        s.service.socket.display(),
        s.image
    );
    let mut child = Command::new("script")
        .args(["-qec", &line, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Held open until the end: script passes an end of input on as Ctrl+D.
    let stdin = child.stdin.take();
    let mut text = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let status = child.wait().unwrap();
    drop(stdin);
    assert!(text.contains("tty-ok"), "{text}");
    assert_eq!(status.code(), Some(0), "{text}");
}

#[test]
fn docker_create_start_attach_wait() {
    let s = setup();
    s.ok(&[
        "create",
        "--name",
        "c1",
        &s.image,
        "sh",
        "-c",
        "echo hi-from-c1; exit 5",
    ]);
    let ps = s.ok(&["ps", "-a", "--format", "{{.Names}} {{.Status}}"]);
    assert!(ps.lines().any(|l| l == "c1 Created"), "{ps}");
    let out = s.docker(&["start", "-a", "c1"]);
    assert_eq!(stdout(&out), "hi-from-c1", "stderr: {}", stderr(&out));
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(s.ok(&["wait", "c1"]), "5");
    // A second start -a runs it again with the same writable layer.
    let out = s.docker(&["start", "-a", "c1"]);
    assert_eq!(stdout(&out), "hi-from-c1");
    s.ok(&["rm", "c1"]);
}

#[test]
fn docker_logs_follow_and_tail() {
    let s = setup();
    s.ok(&[
        "run",
        "-d",
        "--name",
        "lg",
        &s.image,
        "sh",
        "-c",
        "echo one; echo two >&2; sleep 2; echo three",
    ]);
    assert!(wait_for(Duration::from_secs(5), || {
        let out = s.docker(&["logs", "lg"]);
        stderr(&out).contains("two")
    }));
    let out = s.docker(&["logs", "--tail", "1", "lg"]);
    assert_eq!(stdout(&out), "", "the last line went to stderr");
    assert_eq!(stderr(&out).trim(), "two");
    // Ends when the container ends.
    let out = s.docker(&["logs", "-f", "lg"]);
    assert_eq!(stdout(&out), "one\nthree");
    assert_eq!(stderr(&out).trim(), "two");
    let stamped = s.ok(&["logs", "-t", "lg"]);
    assert!(
        stamped.lines().all(|l| l.contains("T") && l.contains("Z ")),
        "{stamped}"
    );
    s.ok(&["rm", "lg"]);
}

#[test]
fn docker_ps_filters_by_label() {
    let s = setup();
    for (name, app) in [("a1", "app=x"), ("b1", "app=y")] {
        s.ok(&[
            "run", "-d", "--name", name, "--label", app, &s.image, "sleep", "30",
        ]);
    }
    let names = |filter: &str| {
        let mut v: Vec<String> = s
            .ok(&["ps", "--filter", filter, "--format", "{{.Names}}"])
            .lines()
            .map(String::from)
            .collect();
        v.sort();
        v.join(" ")
    };
    assert_eq!(names("label=app=x"), "a1");
    assert_eq!(names("label=app"), "a1 b1");
    assert_eq!(names("name=b1"), "b1");
    let labels = s.ok(&["inspect", "--format", "{{json .Config.Labels}}", "a1"]);
    assert_eq!(labels, r#"{"app":"x"}"#);
    s.ok(&["rm", "-f", "a1", "b1"]);
}

#[test]
fn binds_and_volumes_through_the_api() {
    let s = setup();
    let host = s.sb.tmp.path().join("h");
    std::fs::create_dir_all(&host).unwrap();
    s.ok(&[
        "run",
        "--rm",
        "-v",
        &format!("{}:/h", host.display()),
        "-v",
        "vol:/v",
        &s.image,
        "sh",
        "-c",
        "echo bound > /h/f; echo kept > /v/f",
    ]);
    assert_eq!(std::fs::read_to_string(host.join("f")).unwrap(), "bound\n");
    assert_eq!(
        s.ok(&["run", "--rm", "-v", "vol:/v:ro", &s.image, "cat", "/v/f"]),
        "kept"
    );
    // The API takes host paths as given: a relative one is refused.
    let out = s.docker(&[
        "run",
        "--rm",
        "--mount",
        "type=bind,src=rel,dst=/r",
        &s.image,
    ]);
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn ports_through_the_api() {
    let s = setup();
    s.ok(&[
        "run",
        "-d",
        "--name",
        "srv",
        "-p",
        "0:80",
        &s.image,
        "sh",
        "-c",
        "while true; do echo hi | nc -l -p 80; done",
    ]);
    let shown = s.ok(&["port", "srv", "80"]);
    let port: u16 = shown
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in {shown:?}"));
    assert!(port > 0);
    let answered = wait_for(Duration::from_secs(10), || {
        let Ok(mut conn) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
            return false;
        };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
        let mut text = String::new();
        let _ = conn.read_to_string(&mut text);
        text.contains("hi")
    });
    assert!(answered, "nothing answered on port {port}");
    s.ok(&["rm", "-f", "srv"]);
}

#[test]
fn a_client_that_leaves_mid_stream_leaves_the_container_running() {
    let s = setup();
    s.ok(&[
        "run",
        "-d",
        "--name",
        "long",
        &s.image,
        "sh",
        "-c",
        "while true; do echo tick; sleep 0.2; done",
    ]);
    let logs = s.killed_after("2", &["logs", "-f", "long"]);
    assert!(stdout(&logs).contains("tick"), "{}", stderr(&logs));
    let attach = s.killed_after("2", &["attach", "long"]);
    assert!(stdout(&attach).contains("tick"), "{}", stderr(&attach));
    let before = s.ok(&["logs", "long"]).lines().count();
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        s.ok(&["logs", "long"]).lines().count() > before,
        "still logging"
    );
    assert_eq!(s.ok(&["ps", "--format", "{{.Names}}"]), "long");
    assert_eq!(curl(&s.service.socket, &["/_ping"]).0, "OK");
    s.ok(&["rm", "-f", "long"]);
}

#[test]
fn parallel_runs_through_the_api() {
    let s = setup();
    std::thread::scope(|scope| {
        let runs: Vec<_> = (0..8)
            .map(|n| {
                let s = &s;
                scope.spawn(move || {
                    let n = n.to_string();
                    let out = s.docker(&["run", "--rm", &s.image, "echo", &n]);
                    (n, out)
                })
            })
            .collect();
        for run in runs {
            let (n, out) = run.join().unwrap();
            assert_eq!(out.status.code(), Some(0), "run {n}: {}", stderr(&out));
            assert_eq!(stdout(&out), n);
        }
    });
    assert_eq!(s.ok(&["ps", "-aq"]), "");
}

#[test]
fn odd_container_names_are_refused() {
    let s = setup();
    let out = s.docker(&["create", "--name", "../x", &s.image]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("Invalid container name"),
        "{}",
        stderr(&out)
    );
    let body = format!(r#"{{"Image":"{}"}}"#, s.image);
    let (reply, head) = curl(
        &s.service.socket,
        &[
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            "/containers/create?name=%2e%2e",
        ],
    );
    assert!(head.contains(" 400 "), "{head}");
    assert!(reply.contains("Invalid container name"), "{reply}");
    assert_eq!(s.ok(&["ps", "-aq"]), "");
}

/// A running container for exec tests.
fn running(s: &Setup, name: &str) {
    s.ok(&["run", "-d", "--name", name, &s.image, "sleep", "300"]);
}

#[test]
fn docker_exec_runs_and_returns_the_code() {
    let s = setup();
    running(&s, "e1");
    let out = s.docker(&["exec", "e1", "sh", "-c", "echo in; echo err >&2; exit 4"]);
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "in");
    assert_eq!(stderr(&out).trim(), "err");
    let shown = s.ok(&[
        "exec",
        "-e",
        "X=1",
        "-w",
        "/tmp",
        "-u",
        "nobody",
        "e1",
        "sh",
        "-c",
        "echo $X $(pwd) $(id -un)",
    ]);
    assert_eq!(shown, "1 /tmp nobody");
    let missing = s.docker(&["exec", "e1", "no-such-command"]);
    assert_eq!(missing.status.code(), Some(127), "{}", stderr(&missing));
    assert!(
        stderr(&missing).contains("no-such-command"),
        "{}",
        stderr(&missing)
    );
    s.ok(&["rm", "-f", "e1"]);
}

#[test]
fn docker_exec_interactive() {
    let s = setup();
    running(&s, "e2");
    let mut child = s
        .cmd(&["exec", "-i", "e2", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"x\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "x");
    s.ok(&["rm", "-f", "e2"]);
}

#[test]
fn docker_exec_tty_through_script() {
    let s = setup();
    running(&s, "e3");
    let config = s.sb.tmp.path().join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    let docker = if root_mode() {
        format!("sudo -n env DOCKER_CONFIG={} docker", config.display())
    } else {
        format!("env DOCKER_CONFIG={} docker", config.display())
    };
    let line = format!(
        "{docker} -H unix://{} exec -it e3 sh -c 'stty size; echo tty-ok'",
        s.service.socket.display()
    );
    let mut child = Command::new("script")
        .args(["-qec", &line, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let mut text = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let status = child.wait().unwrap();
    drop(stdin);
    assert!(text.contains("tty-ok"), "{text}");
    let size = text.lines().find(|l| {
        let parts: Vec<&str> = l.trim().split(' ').collect();
        parts.len() == 2 && parts.iter().all(|p| p.parse::<u16>().is_ok())
    });
    assert!(size.is_some(), "stty size printed no size: {text}");
    assert_eq!(status.code(), Some(0), "{text}");
    s.ok(&["rm", "-f", "e3"]);
}

#[test]
fn docker_exec_on_a_stopped_container_is_a_conflict() {
    let s = setup();
    s.ok(&["create", "--name", "e4", &s.image, "true"]);
    let out = s.docker(&["exec", "e4", "true"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("is not running"), "{}", stderr(&out));
    s.ok(&["rm", "e4"]);
}

#[test]
fn an_exec_client_that_leaves_ends_the_relay() {
    let s = setup();
    running(&s, "e5");
    let out = s.killed_after("2", &["exec", "e5", "sleep", "31"]);
    assert_ne!(out.status.code(), Some(0));
    let gone = wait_for(Duration::from_secs(10), || {
        !s.ok(&["exec", "e5", "ps", "-o", "args"])
            .contains("sleep 31")
    });
    assert!(gone, "the exec's command still runs");
    assert_eq!(s.ok(&["ps", "--format", "{{.Names}}"]), "e5");
    s.ok(&["rm", "-f", "e5"]);
}

#[test]
fn filters_openpod_lacks_are_refused() {
    let s = setup();
    s.ok(&["run", "-d", "--name", "keep", &s.image, "sleep", "30"]);
    let out = s.docker(&["ps", "-aq", "--filter", "volume=x"]);
    assert_ne!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stderr(&out).contains("volume"), "{}", stderr(&out));
    let out = s.docker(&["images", "-q", "--filter", "before=x"]);
    assert_ne!(out.status.code(), Some(0), "{}", stdout(&out));
    assert_eq!(s.ok(&["ps", "-q"]).lines().count(), 1);
    // Events filter by the labels containers were created with.
    std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            s.killed_after(
                "5",
                &[
                    "events",
                    "--filter",
                    "label=app=x",
                    "--format",
                    "{{.Actor.Attributes.name}}",
                ],
            )
        });
        std::thread::sleep(Duration::from_secs(1));
        s.ok(&["create", "--name", "lab1", "--label", "app=x", &s.image]);
        s.ok(&["create", "--name", "nolab", &s.image]);
        let seen = stdout(&watcher.join().unwrap());
        assert!(seen.lines().any(|l| l == "lab1"), "{seen}");
        assert!(!seen.contains("nolab"), "{seen}");
    });
    s.ok(&["rm", "-f", "keep", "lab1", "nolab"]);
}

#[test]
fn a_failed_run_with_rm_leaves_nothing() {
    let s = setup();
    // Docker removes an auto-remove container whose start fails, and the CLI waits for
    // that removal: without it, this would hang.
    let out = s.killed_after("30", &["run", "--rm", &s.image, "no-such-command"]);
    // timeout(1) exits 124 when it had to kill the command.
    assert!(
        !matches!(out.status.code(), Some(124 | 137)),
        "docker run hung: {}",
        stderr(&out)
    );
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("no-such-command"), "{}", stderr(&out));
    assert!(
        wait_for(Duration::from_secs(5), || s.ok(&["ps", "-aq"]).is_empty()),
        "the container stayed"
    );
}

#[test]
fn stop_then_start_through_the_api_starts_again() {
    let s = setup();
    s.ok(&["run", "-d", "--name", "again", &s.image, "sleep", "300"]);
    for round in 0..10 {
        s.ok(&["stop", "-t", "1", "again"]);
        s.ok(&["start", "again"]);
        let ps = s.ok(&["ps", "--format", "{{.Names}}"]);
        assert_eq!(ps, "again", "round {round}: not running after start");
    }
    s.ok(&["restart", "-t", "1", "again"]);
    assert_eq!(s.ok(&["ps", "--format", "{{.Names}}"]), "again");
    s.ok(&["rm", "-f", "again"]);
}

#[test]
fn signals_for_the_service_do_not_reach_its_containers() {
    if root_mode() {
        return; // sudo stands between the test and the service; the code is the same
    }
    let reg = FakeRegistry::start(false);
    reg.push(
        "img",
        "v1",
        &TestImage {
            layers: vec![alpine_layer()],
            config: serde_json::json!({"Env": [PATH], "Cmd": ["sh"]}),
        },
    );
    let image = format!("{}/img:v1", reg.host);
    let sb = Sandbox::new();
    let service = Service::start_in_own_group(&sb, &["--timeout", "0"]);
    assert_eq!(
        docker_out(&sb, &service, &["pull", &image]).status.code(),
        Some(0)
    );
    // A shell with handlers: as PID 1 in its namespace it gets the signals it handles.
    let script = "trap 'exit 3' INT HUP TERM; while true; do sleep 0.1; done";
    let mut attached = docker(
        &sb,
        &service,
        &["run", "--name", "att", &image, "sh", "-c", script],
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let running = || {
        let out = sb.openpod().args(["ps"]).output().unwrap();
        stdout(&out)
            .lines()
            .any(|l| l.split_whitespace().last() == Some("att"))
    };
    assert!(
        wait_for(Duration::from_secs(10), running),
        "att did not start"
    );
    // A Ctrl+C in the service's terminal, sent to its process group.
    // SAFETY: a signal to a process group this test made.
    unsafe { libc::kill(-(service.pid() as i32), libc::SIGINT) };
    std::thread::sleep(Duration::from_secs(1));
    assert!(running(), "the container died with the service's signal");
    let _ = attached.kill();
    let _ = attached.wait();
    let _ = sb.openpod().args(["rm", "-f", "att"]).output();
}

#[test]
fn restart_policy_through_the_api() {
    let s = setup();
    let run = s.docker(&[
        "run",
        "-d",
        "--name",
        "flaky",
        "--restart",
        "on-failure:2",
        &s.image,
        "false",
    ]);
    assert_eq!(run.status.code(), Some(0), "{}", stderr(&run));
    assert!(
        !stderr(&run).to_lowercase().contains("restart"),
        "{}",
        stderr(&run)
    );
    let inspect = || -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(&s.ok(&["inspect", "flaky"])).unwrap()[0].clone()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let i = inspect();
        if i["RestartCount"] == 2 && i["State"]["Status"] == "exited" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no two restarts: {i}");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(1500));
    let i = inspect();
    assert_eq!(i["RestartCount"], 2, "{i}");
    assert_eq!(
        i["HostConfig"]["RestartPolicy"],
        serde_json::json!({"Name": "on-failure", "MaximumRetryCount": 2})
    );
    // With AutoRemove a restart policy makes no sense (the docker CLI refuses it too).
    let (body, head) = curl(
        &s.service.socket,
        &[
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &serde_json::json!({
                "Image": s.image,
                "Cmd": ["true"],
                "HostConfig": {"AutoRemove": true, "RestartPolicy": {"Name": "always"}}
            })
            .to_string(),
            "/containers/create",
        ],
    );
    assert!(head.starts_with("HTTP/1.1 400"), "{head}\n{body}");
    s.ok(&["rm", "-f", "flaky"]);
}

#[test]
fn health_through_the_api() {
    let s = setup();
    // An image with a check of its own.
    s._reg.push(
        "checked",
        "v1",
        &TestImage {
            layers: vec![alpine_layer()],
            config: serde_json::json!({
                "Env": [PATH],
                "Cmd": ["sleep", "300"],
                "Healthcheck": {"Test": ["CMD-SHELL", "true"], "Interval": 1_000_000_000_u64}
            }),
        },
    );
    let checked = format!("{}/checked:v1", s._reg.host);
    s.ok(&["pull", &checked]);
    let inspect = |name: &str| -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(&s.ok(&["inspect", name])).unwrap()[0].clone()
    };
    let wait_health = |name: &str, want: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let i = inspect(name);
            if i["State"]["Health"]["Status"] == want {
                return i;
            }
            assert!(std::time::Instant::now() < deadline, "not {want}: {i}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    // Events from before the first check on.
    let mut events = s
        .cmd(&[
            "events",
            "--filter",
            "event=health_status",
            "--format",
            "{{.Actor.Attributes.name}} {{.Action}}",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let run = s.docker(&["run", "-d", "--name", "h1", &checked]);
    assert_eq!(run.status.code(), Some(0), "{}", stderr(&run));
    assert!(
        !stderr(&run).to_lowercase().contains("health"),
        "{}",
        stderr(&run)
    );
    let h1 = wait_health("h1", "healthy");
    assert!(
        h1["State"]["Health"]["Log"]
            .as_array()
            .is_some_and(|l| !l.is_empty()),
        "{h1}"
    );
    assert_eq!(
        h1["Config"]["Healthcheck"]["Test"],
        serde_json::json!(["CMD-SHELL", "true"])
    );
    assert!(s.ok(&["ps"]).contains("(healthy)"));
    assert_eq!(
        s.ok(&["ps", "--filter", "health=healthy", "--format", "{{.Names}}"])
            .trim(),
        "h1"
    );
    // Turned off: no health at all.
    s.ok(&["run", "-d", "--name", "h2", "--no-healthcheck", &checked]);
    assert!(inspect("h2")["State"]["Health"].is_null());
    // A check of its own that fails.
    s.ok(&[
        "run",
        "-d",
        "--name",
        "h3",
        "--health-cmd",
        "exit 1",
        "--health-interval",
        "1s",
        "--health-retries",
        "1",
        &s.image,
        "sleep",
        "300",
    ]);
    wait_health("h3", "unhealthy");
    assert_eq!(
        s.ok(&[
            "ps",
            "--filter",
            "health=unhealthy",
            "--format",
            "{{.Names}}"
        ])
        .trim(),
        "h3"
    );
    std::thread::sleep(Duration::from_millis(800));
    // SIGTERM, not SIGKILL: in root mode the child is sudo, which passes it on (a killed
    // sudo would leave the docker CLI running with the pipe open).
    // SAFETY: a signal to our own child, not reaped yet.
    unsafe { libc::kill(events.id() as i32, libc::SIGTERM) };
    let mut seen = String::new();
    events
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut seen)
        .unwrap();
    let _ = events.wait();
    assert!(seen.contains("h1 health_status: healthy"), "{seen}");
    assert!(seen.contains("h3 health_status: unhealthy"), "{seen}");
    s.ok(&["rm", "-f", "h1", "h2", "h3"]);
}

#[test]
fn a_terminal_starts_with_the_size_that_was_asked_for() {
    let s = setup();
    let (body, head) = curl(
        &s.service.socket,
        &[
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &serde_json::json!({
                "Image": s.image,
                "Cmd": ["stty", "size"],
                "Tty": true,
                "HostConfig": {"ConsoleSize": [30, 100]}
            })
            .to_string(),
            "/containers/create?name=sized",
        ],
    );
    assert!(head.starts_with("HTTP/1.1 201"), "{head}\n{body}");
    s.ok(&["start", "sized"]);
    assert_eq!(s.ok(&["wait", "sized"]).trim(), "0");
    let logs = s.ok(&["logs", "sized"]);
    assert_eq!(logs.trim(), "30 100", "{logs}");
    s.ok(&["rm", "sized"]);
}

#[test]
fn privileged_through_the_api() {
    let s = setup();
    let caps = s.ok(&[
        "run",
        "--rm",
        "--privileged",
        &s.image,
        "sh",
        "-c",
        "grep CapEff /proc/self/status",
    ]);
    let last: u32 = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let all = format!("{:016x}", (1u64 << (last + 1)) - 1);
    assert!(caps.contains(&all), "{caps}");
    s.ok(&["create", "--name", "p", "--privileged", &s.image, "true"]);
    let inspect: serde_json::Value =
        serde_json::from_str::<serde_json::Value>(&s.ok(&["inspect", "p"])).unwrap()[0].clone();
    assert_eq!(inspect["HostConfig"]["Privileged"], true);
    s.ok(&["rm", "p"]);
}

#[test]
fn docker_cp_into_a_created_and_a_running_container() {
    let s = setup();
    let dir = tempfile::tempdir().unwrap();
    let note = dir.path().join("note");
    std::fs::write(&note, "from the host\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o644)).unwrap();
    let note = note.display().to_string();
    // Before its start (as buildx gives BuildKit its configuration).
    s.ok(&["create", "--name", "c1", &s.image, "cat", "/etc/note"]);
    s.ok(&["cp", &note, "c1:/etc/"]);
    let out = s.ok(&["start", "-a", "c1"]);
    assert_eq!(out.trim(), "from the host", "{out}");
    // While it runs.
    s.ok(&["run", "-d", "--name", "c2", &s.image, "sleep", "300"]);
    s.ok(&["cp", &note, "c2:/tmp/"]);
    assert_eq!(
        s.ok(&["exec", "c2", "cat", "/tmp/note"]).trim(),
        "from the host"
    );
    s.ok(&["rm", "-f", "c1", "c2"]);
}

#[test]
fn docker_cp_out_of_a_running_and_an_ended_container() {
    let s = setup();
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let out = |name: &str| dir.path().join(name).display().to_string();
    s.ok(&[
        "run",
        "-d",
        "--name",
        "r",
        &s.image,
        "sh",
        "-c",
        "mkdir -p /data/sub && echo one > /data/sub/a && ln -s sub/a /data/link && sleep 300",
    ]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while s
        .docker(&["exec", "r", "test", "-e", "/data/link"])
        .status
        .code()
        != Some(0)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the files never appeared"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // A directory, its symbolic link kept as one.
    s.ok(&["cp", "r:/data", &out("data")]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("data/sub/a")).unwrap(),
        "one\n"
    );
    let link = std::fs::read_link(dir.path().join("data/link")).unwrap();
    assert_eq!(link.display().to_string(), "sub/a");
    // The contents of a directory, with a trailing `/.`.
    s.ok(&["cp", "r:/data/sub/.", &out("contents")]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("contents/a")).unwrap(),
        "one\n"
    );
    // A container that ended keeps its files.
    s.ok(&[
        "run",
        "--name",
        "e",
        &s.image,
        "sh",
        "-c",
        "echo two > /left",
    ]);
    s.ok(&["cp", "e:/left", &out("left")]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("left")).unwrap(),
        "two\n"
    );
    let missing = s.docker(&["cp", "e:/nothing", &out("nothing")]);
    assert_ne!(missing.status.code(), Some(0));
    assert!(
        stderr(&missing).to_lowercase().contains("could not find"),
        "{}",
        stderr(&missing)
    );
    s.ok(&["rm", "-f", "r", "e"]);
}

#[test]
fn a_hostile_archive_stays_in_the_container() {
    let s = setup();
    s.ok(&["create", "--name", "h", &s.image, "true"]);
    let dir = tempfile::tempdir().unwrap();
    let marker = format!("openpod-escape-{}", std::process::id());
    // Raw names: tar-rs refuses to write `..` itself.
    let entry = |name: &str, kind: tar::EntryType, link: &str, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(kind);
        h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
        h.as_gnu_mut().unwrap().linkname[..link.len()].copy_from_slice(link.as_bytes());
        h.set_cksum();
        (h, data.to_vec())
    };
    let tar_file = |name: &str, entries: Vec<(tar::Header, Vec<u8>)>| {
        let mut b = tar::Builder::new(Vec::new());
        for (h, data) in entries {
            b.append(&h, data.as_slice()).unwrap();
        }
        let path = dir.path().join(name);
        std::fs::write(&path, b.into_inner().unwrap()).unwrap();
        path.display().to_string()
    };
    let put = |file: &str, path: &str| {
        let (body, head) = curl(
            &s.service.socket,
            &[
                "-X",
                "PUT",
                "-H",
                "Content-Type: application/x-tar",
                "--data-binary",
                &format!("@{file}"),
                &format!("/containers/h/archive?path={path}&noOverwriteDirNonDir=true"),
            ],
        );
        (head.lines().next().unwrap_or("").to_string(), body)
    };
    // `..` leaving the destination is refused.
    let up = format!("../../../../../../../tmp/{marker}");
    let file = tar_file(
        "up.tar",
        vec![entry(&up, tar::EntryType::Regular, "", b"x")],
    );
    let (status, body) = put(&file, "/tmp");
    assert!(status.contains(" 400 "), "{status} {body}");
    assert!(!Path::new("/tmp").join(&marker).exists());
    // A link to `/` leads to the container's root, not the host's.
    let file = tar_file(
        "link.tar",
        vec![
            entry("root", tar::EntryType::Symlink, "/", b""),
            entry(
                &format!("root/tmp/{marker}"),
                tar::EntryType::Regular,
                "",
                b"x",
            ),
        ],
    );
    let (status, body) = put(&file, "/");
    assert!(!Path::new("/tmp").join(&marker).exists(), "{status} {body}");
    // Not an archive at all.
    let junk = dir.path().join("junk");
    std::fs::write(&junk, vec![b'j'; 4096]).unwrap();
    let (status, body) = put(&junk.display().to_string(), "/tmp");
    assert!(status.contains(" 400 "), "{status} {body}");
    s.ok(&["rm", "h"]);
}

#[test]
fn docker_cp_of_proc_is_an_empty_directory() {
    // As with Docker, which reads the container's files from outside: the kernel's
    // file systems (/proc, /sys) come out as empty directories.
    let s = setup();
    s.ok(&["run", "-d", "--name", "p", &s.image, "sleep", "300"]);
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let out = dir.path().join("proc");
    s.ok(&["cp", "p:/proc", &out.display().to_string()]);
    let entries: Vec<_> = std::fs::read_dir(&out).unwrap().collect();
    assert!(entries.is_empty(), "{} entries", entries.len());
    s.ok(&["rm", "-f", "p"]);
}

#[test]
fn a_copy_that_fails_midway_is_an_error() {
    // Rootless, a file of the host's root that a bind mount shows is not readable to the
    // container's root: the copy fails after its details were sent.
    if root_mode() {
        return;
    }
    let s = setup();
    s.ok(&[
        "run",
        "-d",
        "--name",
        "f",
        "-v",
        "/etc/shadow:/secret:ro",
        &s.image,
        "sleep",
        "300",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = s.docker(&[
        "cp",
        "f:/secret",
        &dir.path().join("secret").display().to_string(),
    ]);
    assert_ne!(out.status.code(), Some(0), "the copy reported success");
    s.ok(&["rm", "-f", "f"]);
}

#[test]
fn links_a_running_container_plants_stay_inside_it() {
    // The container makes links to `/` and to its init's root; copies through them reach
    // its own files, never the host's (as root the helper runs as the host's root).
    let s = setup();
    let marker = format!("openpod-marker-{}", std::process::id());
    let script = format!(
        "ln -s / /out && ln -s /proc/1/root /out2 && echo inside > /etc/{marker} && sleep 300"
    );
    s.ok(&["run", "-d", "--name", "l", &s.image, "sh", "-c", &script]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while s
        .docker(&["exec", "l", "test", "-e", &format!("/etc/{marker}")])
        .status
        .code()
        != Some(0)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the links never appeared"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    for (i, via) in ["/out", "/out2"].iter().enumerate() {
        let got = dir.path().join(format!("got{i}"));
        s.ok(&[
            "cp",
            &format!("l:{via}/etc/{marker}"),
            &got.display().to_string(),
        ]);
        assert_eq!(std::fs::read_to_string(&got).unwrap(), "inside\n", "{via}");
    }
    // In through the links: the container's /tmp, not the host's.
    let note = dir.path().join(&marker);
    std::fs::write(&note, "copied in\n").unwrap();
    std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o644)).unwrap();
    for via in ["/out", "/out2"] {
        s.ok(&["cp", &note.display().to_string(), &format!("l:{via}/tmp/")]);
        assert!(
            !Path::new("/tmp").join(&marker).exists(),
            "{via} reached the host"
        );
        assert_eq!(
            s.ok(&["exec", "l", "cat", &format!("/tmp/{marker}")]),
            "copied in"
        );
        s.ok(&["exec", "l", "rm", &format!("/tmp/{marker}")]);
    }
    s.ok(&["rm", "-f", "l"]);
}

/// Read an answer's head, to its empty line.
fn read_head(s: &mut std::os::unix::net::UnixStream) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut byte).unwrap(), 1, "{head:?}");
        head.push(byte[0]);
    }
}

/// One request on a connection of its own; the whole answer.
fn http(socket: &Path, method: &str, path: &str, body: &str) -> String {
    let mut s = std::os::unix::net::UnixStream::connect(socket).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn a_removal_is_answered_as_soon_as_it_happens() {
    if root_mode() {
        return; // the test user cannot connect to root's socket; the code is the same
    }
    // The docker CLI asks to hear of a --rm container's removal before it starts it, and
    // starts it when that answer's headers arrive: the answer must follow the removal at
    // once, not at the next look at the container's state.
    let s = setup();
    let create = format!(
        r#"{{"Image":"{}","Cmd":["true"],"HostConfig":{{"NetworkMode":"none","AutoRemove":true}}}}"#,
        s.image
    );
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let created = http(&s.service.socket, "POST", "/containers/create", &create);
        let id = created
            .split("\"Id\":\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or_else(|| panic!("{created}"))
            .to_string();
        // As the docker CLI does: attach, ask for the removal, then start.
        let mut attach = std::os::unix::net::UnixStream::connect(&s.service.socket).unwrap();
        write!(
            attach,
            "POST /containers/{id}/attach?stream=1&stdout=1&stderr=1 HTTP/1.1\r\nHost: d\r\n\
             Upgrade: tcp\r\nConnection: Upgrade\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
        read_head(&mut attach);
        let mut wait = std::os::unix::net::UnixStream::connect(&s.service.socket).unwrap();
        write!(
            wait,
            "POST /containers/{id}/wait?condition=removed HTTP/1.1\r\nHost: d\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        read_head(&mut wait);
        let started = std::time::Instant::now();
        let start = http(
            &s.service.socket,
            "POST",
            &format!("/containers/{id}/start"),
            "",
        );
        assert!(start.starts_with("HTTP/1.1 204"), "{start}");
        let mut rest = String::new();
        wait.read_to_string(&mut rest).unwrap();
        assert!(rest.contains("\"StatusCode\":0"), "{rest}");
        best = best.min(started.elapsed());
    }
    assert!(best < Duration::from_millis(40), "best of 3: {best:?}");
}
