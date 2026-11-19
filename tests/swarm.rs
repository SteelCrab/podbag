#![cfg(target_os = "linux")]
//! Swarm mode on one node against `podbag system service`, with the real docker CLI:
//! services, rolling updates and rollbacks, stacks with published ports, secrets and
//! configs, and `swarm leave --force`. Skipped with a message without the docker CLI.

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::collections::BTreeSet;
use std::process::Output;
use std::time::Duration;

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A task's command: says hello, then waits; it ends at once on SIGTERM.
const WAIT: &str = "echo hello from $(hostname); trap 'exit 0' TERM; while true; do sleep 1; done";

fn have_docker() -> bool {
    let ok = std::process::Command::new("docker")
        .args(["version", "--format", "{{.Client.Version}}"])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty());
    if !ok {
        eprintln!("the docker CLI is missing: skipped");
    }
    ok
}

struct Swarm {
    _reg: FakeRegistry,
    _turn: Option<std::fs::File>,
    sb: Sandbox,
    service: Service,
    /// `<registry>/img`: tags v1 and v2.
    repo: String,
}

impl Swarm {
    fn start() -> Swarm {
        let turn = network_turn();
        let reg = FakeRegistry::start(false);
        for tag in ["v1", "v2"] {
            reg.push(
                "img",
                tag,
                &TestImage {
                    layers: vec![alpine_layer()],
                    config: serde_json::json!({"Env": [PATH, format!("TAG={tag}")], "Cmd": ["sh"]}),
                },
            );
        }
        let repo = format!("{}/img", reg.host);
        let sb = Sandbox::new();
        let service = Service::start(&sb, &["--timeout", "0"]);
        let s = Swarm {
            _reg: reg,
            _turn: turn,
            sb,
            service,
            repo,
        };
        let out = s.ok(&["swarm", "init"]);
        assert!(out.contains("Swarm initialized"), "{out}");
        s
    }

    fn docker(&self, args: &[&str]) -> Output {
        docker_out(&self.sb, &self.service, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.docker(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "docker {args:?}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    fn lines(&self, args: &[&str]) -> Vec<String> {
        self.ok(args).lines().map(String::from).collect()
    }

    /// The running tasks of a service: (name, image).
    fn running(&self, service: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .lines(&[
                "service",
                "ps",
                service,
                "--filter",
                "desired-state=running",
                "--format",
                "{{.Name}} {{.Image}} {{.CurrentState}}",
            ])
            .iter()
            .filter(|l| l.contains(" Running "))
            .map(|l| {
                let mut w = l.split(' ');
                (w.next().unwrap().to_string(), w.next().unwrap().to_string())
            })
            .collect();
        out.sort();
        out
    }

    /// The podbag containers of the sandbox.
    fn containers(&self) -> Vec<String> {
        let out = self.sb.podbag().args(["ps", "-aq"]).output().unwrap();
        stdout(&out).lines().map(String::from).collect()
    }
}

impl Drop for Swarm {
    fn drop(&mut self) {
        let _ = self.docker(&["swarm", "leave", "--force"]);
    }
}

#[test]
fn services_scale_update_and_roll_back() {
    if !have_docker() {
        return;
    }
    let s = Swarm::start();
    let v1 = format!("{}:v1", s.repo);
    let v2 = format!("{}:v2", s.repo);

    // Before anything: the one node, and the swarm in /info.
    let nodes = s.ok(&[
        "node",
        "ls",
        "--format",
        "{{.Self}} {{.Status}} {{.ManagerStatus}}",
    ]);
    assert_eq!(nodes, "true Ready Leader");
    assert_eq!(
        s.ok(&[
            "info",
            "--format",
            "{{.Swarm.LocalNodeState}} {{.Swarm.Nodes}}"
        ]),
        "active 1"
    );

    s.ok(&[
        "service",
        "create",
        "-q",
        "--name",
        "web",
        "--replicas",
        "3",
        "--stop-grace-period",
        "1s",
        &v1,
        "sh",
        "-c",
        WAIT,
    ]);
    let tasks = s.running("web");
    assert_eq!(
        tasks.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(),
        ["web.1", "web.2", "web.3"],
        "{tasks:?}"
    );
    assert_eq!(
        s.ok(&["service", "ls", "--format", "{{.Name}} {{.Replicas}}"]),
        "web 3/3"
    );
    // Each task is a container with Docker's names and labels.
    let names = s.lines(&[
        "ps",
        "--format",
        "{{.Names}} {{.Label \"com.docker.swarm.service.name\"}}",
    ]);
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(
        names
            .iter()
            .all(|n| n.starts_with("web.") && n.ends_with(" web")),
        "{names:?}"
    );

    // Scale up and down.
    s.ok(&["service", "scale", "web=5"]);
    assert_eq!(s.running("web").len(), 5);
    s.ok(&["service", "scale", "web=2"]);
    assert_eq!(s.running("web").len(), 2);
    assert_eq!(s.containers().len(), 2);

    // A rolling update to another image, then back.
    s.ok(&[
        "service",
        "update",
        "-q",
        "--image",
        &v2,
        "--update-parallelism",
        "1",
        "web",
    ]);
    let tasks = s.running("web");
    assert_eq!(tasks.len(), 2, "{tasks:?}");
    assert!(tasks.iter().all(|t| t.1 == v2), "{tasks:?}");
    let status = s.ok(&[
        "service",
        "inspect",
        "web",
        "--format",
        "{{.UpdateStatus.State}}",
    ]);
    assert_eq!(status, "completed");
    s.ok(&["service", "rollback", "-q", "web"]);
    let tasks = s.running("web");
    assert!(tasks.iter().all(|t| t.1 == v1), "{tasks:?}");
    let status = s.ok(&[
        "service",
        "inspect",
        "web",
        "--format",
        "{{.UpdateStatus.State}}",
    ]);
    assert_eq!(status, "rollback_completed");

    // The tasks' output, with Docker's task prefixes.
    let logs = s.docker(&["service", "logs", "web"]);
    let text = stdout(&logs);
    assert!(text.contains("hello from"), "{text}{}", stderr(&logs));
    assert!(text.lines().all(|l| l.starts_with("web.")), "{text}");

    // A task removed by hand comes back at the next look.
    let one = s.containers()[0].clone();
    assert!(
        s.sb.podbag()
            .args(["rm", "-f", &one])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(wait_for(Duration::from_secs(20), || s.running("web").len() == 2));

    s.ok(&["service", "rm", "web"]);
    assert!(s.containers().is_empty(), "{:?}", s.containers());
    s.ok(&["swarm", "leave", "--force"]);
    assert_eq!(
        s.ok(&["info", "--format", "{{.Swarm.LocalNodeState}}"]),
        "inactive"
    );
    assert!(!s.sb.data().join("swarm").exists());
}

#[test]
fn a_stack_publishes_a_port_and_mounts_secrets_and_configs() {
    if !have_docker() {
        return;
    }
    let s = Swarm::start();
    let port = 20000 + std::process::id() % 9000;
    let dir = s.sb.tmp.path().join("project");
    std::fs::create_dir_all(&dir).unwrap();
    // Secret data made at run time, never written in the source.
    let mut secret = [0u8; 18];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut secret))
        .unwrap();
    let secret: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(dir.join("pw.txt"), &secret).unwrap();
    std::fs::write(dir.join("site.conf"), "listen 80;\n").unwrap();
    let compose = format!(
        r#"services:
  web:
    image: {repo}:v1
    command: ["sh", "-c", "trap 'exit 0' TERM; while true; do hostname | nc -l -p 80; done"]
    ports: ["{port}:80"]
    stop_grace_period: 1s
    deploy:
      replicas: 3
    secrets: [pw]
    configs:
      - source: site
        target: /etc/site.conf
        mode: 0440
secrets:
  pw:
    file: ./pw.txt
configs:
  site:
    file: ./site.conf
"#,
        repo = s.repo
    );
    let file = dir.join("compose.yml");
    std::fs::write(&file, compose).unwrap();
    s.ok(&[
        "stack",
        "deploy",
        "--detach=false",
        "-q",
        "-c",
        file.to_str().unwrap(),
        "app",
    ]);
    assert_eq!(s.running("app_web").len(), 3);
    assert_eq!(
        s.ok(&["stack", "ls", "--format", "{{.Name}} {{.Services}}"]),
        "app 1"
    );
    let networks = s.lines(&[
        "network",
        "ls",
        "--filter",
        "driver=overlay",
        "--format",
        "{{.Name}} {{.Scope}}",
    ]);
    assert!(
        networks.contains(&"app_default swarm".to_string()),
        "{networks:?}"
    );
    assert!(
        networks.contains(&"ingress swarm".to_string()),
        "{networks:?}"
    );
    assert_eq!(s.ok(&["secret", "ls", "--format", "{{.Name}}"]), "app_pw");
    assert_eq!(s.ok(&["config", "ls", "--format", "{{.Name}}"]), "app_site");
    // The API never shows a secret's data; a config's it does.
    let inspect = s.ok(&["secret", "inspect", "app_pw"]);
    assert!(
        !inspect.contains(&secret) && !inspect.contains("\"Data\""),
        "{inspect}"
    );

    // In a task: the secret and the config, read-only with their modes, and the names.
    let task = s.ok(&["ps", "-q", "--filter", "name=app_web.1"]);
    let seen = s.ok(&[
        "exec", &task, "sh", "-c",
        "cat /run/secrets/pw; echo; stat -c '%a %U' /run/secrets/pw /etc/site.conf; cat /etc/site.conf; \
         echo x > /run/secrets/pw 2>/dev/null || echo read-only; cat /etc/hosts",
    ]);
    assert!(seen.contains(&secret), "{seen}");
    assert!(seen.contains("444 root"), "{seen}");
    assert!(seen.contains("440 root"), "{seen}");
    assert!(seen.contains("listen 80;"), "{seen}");
    assert!(seen.contains("read-only"), "{seen}");
    assert!(seen.contains("tasks.app_web"), "{seen}");
    let names = s.ok(&["exec", &task, "sh", "-c", "getent hosts app_web"]);
    assert!(!names.is_empty(), "the service name resolves in its tasks");

    // The published port from this host, spread over the tasks as root.
    let mut answers = BTreeSet::new();
    for _ in 0..30 {
        if let Ok(mut c) = std::net::TcpStream::connect(("127.0.0.1", port as u16)) {
            let _ = c.set_read_timeout(Some(Duration::from_secs(3)));
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut c, &mut text);
            if !text.trim().is_empty() {
                answers.insert(text.trim().to_string());
            }
        }
    }
    assert!(!answers.is_empty(), "nothing answered on port {port}");
    if root_mode() {
        assert!(
            answers.len() >= 2,
            "one task answered every time: {answers:?}"
        );
        let rules = std::process::Command::new("sudo")
            .args(["-n", "nft", "list", "table", "ip", "podbag"])
            .output()
            .unwrap();
        let rules = stdout(&rules);
        assert!(
            rules.contains(&format!("tcp dport {port} numgen random mod 3")),
            "{rules}"
        );
        assert!(rules.contains("ct status dnat masquerade"), "{rules}");
        // From another network, as from another host: through the forward chain.
        let gateway = s.ok(&[
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{(index .IPAM.Config 0).Gateway}}",
        ]);
        let image = format!("{}:v1", s.repo);
        let far = s.ok(&[
            "run",
            "--rm",
            &image,
            "sh",
            "-c",
            &format!("nc -w 3 {gateway} {port} </dev/null"),
        ]);
        assert!(!far.is_empty(), "no answer through {gateway}:{port}");
    }

    // Leaving takes everything with it.
    s.ok(&["swarm", "leave", "--force"]);
    assert!(s.containers().is_empty(), "{:?}", s.containers());
    let networks = s.lines(&["network", "ls", "--format", "{{.Name}}"]);
    assert!(
        !networks
            .iter()
            .any(|n| n == "app_default" || n == "ingress"),
        "{networks:?}"
    );
    assert!(!s.sb.data().join("swarm").exists());
    if root_mode() {
        let rules = std::process::Command::new("sudo")
            .args(["-n", "nft", "list", "table", "ip", "podbag"])
            .output()
            .unwrap();
        assert!(!stdout(&rules).contains("svc-"), "{}", stdout(&rules));
    }
}

#[test]
fn a_failing_update_rolls_back_by_itself() {
    if !have_docker() {
        return;
    }
    let s = Swarm::start();
    let v1 = format!("{}:v1", s.repo);
    let v2 = format!("{}:v2", s.repo);
    s.ok(&[
        "service",
        "create",
        "-q",
        "--name",
        "web",
        "--replicas",
        "2",
        "--stop-grace-period",
        "1s",
        &v1,
        "sh",
        "-c",
        WAIT,
    ]);
    // The new tasks end at once: the update gives up and goes back.
    let out = s.docker(&[
        "service",
        "update",
        "--image",
        &v2,
        "--args",
        "false",
        "--update-failure-action",
        "rollback",
        "--update-monitor",
        "3s",
        "web",
    ]);
    let said = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        said.contains("rolled back") || said.contains("rollback"),
        "{said}"
    );
    // The CLI stops once the tasks run; the rollback still watches them a moment.
    let state = || {
        s.ok(&[
            "service",
            "inspect",
            "web",
            "--format",
            "{{.UpdateStatus.State}}",
        ])
    };
    assert!(
        wait_for(Duration::from_secs(20), || state() == "rollback_completed"),
        "{}",
        state()
    );
    let tasks = s.running("web");
    assert_eq!(tasks.len(), 2, "{tasks:?}");
    assert!(tasks.iter().all(|t| t.1 == v1), "{tasks:?}");
}

#[test]
fn global_mode_and_a_drained_node() {
    if !have_docker() {
        return;
    }
    let s = Swarm::start();
    let image = format!("{}:v1", s.repo);
    s.ok(&[
        "service",
        "create",
        "-q",
        "--name",
        "agent",
        "--mode",
        "global",
        "--stop-grace-period",
        "1s",
        &image,
        "sh",
        "-c",
        WAIT,
    ]);
    let node = s.ok(&["node", "ls", "-q"]);
    let names = s.lines(&["ps", "--format", "{{.Names}}"]);
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].starts_with(&format!("agent.{node}.")), "{names:?}");
    assert_eq!(
        s.ok(&["service", "ls", "--format", "{{.Mode}} {{.Replicas}}"]),
        "global 1/1"
    );
    // A drained node runs no tasks; an active one runs them again.
    s.ok(&["node", "update", "--availability", "drain", &node]);
    assert!(s.containers().is_empty(), "{:?}", s.containers());
    s.ok(&[
        "node",
        "update",
        "--availability",
        "active",
        "--label-add",
        "zone=a",
        &node,
    ]);
    assert!(wait_for(Duration::from_secs(20), || s
        .running("agent")
        .len()
        == 1));
    let labels = s.ok(&[
        "node",
        "inspect",
        "self",
        "--format",
        "{{.Spec.Labels.zone}}",
    ]);
    assert_eq!(labels, "a");
}

#[test]
fn the_swarm_api_answers_like_docker() {
    if !have_docker() {
        return;
    }
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    // No swarm yet: Docker's 503, and an inactive swarm in /info.
    let (body, head) = curl(&service.socket, &["/v1.47/services"]);
    assert!(head.contains(" 503 "), "{head}");
    assert!(body.contains("not a swarm manager"), "{body}");
    let (body, _) = curl(&service.socket, &["/v1.47/info"]);
    let info: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(info["Swarm"]["LocalNodeState"], "inactive");
    let out = docker_out(&sb, &service, &["network", "create", "-d", "overlay", "x"]);
    assert_ne!(out.status.code(), Some(0));
    // Init twice, join, leave without force.
    assert!(
        docker_out(&sb, &service, &["swarm", "init"])
            .status
            .success()
    );
    let out = docker_out(&sb, &service, &["swarm", "init"]);
    assert!(
        stderr(&out).contains("already part of a swarm"),
        "{}",
        stderr(&out)
    );
    let out = docker_out(&sb, &service, &["swarm", "leave"]);
    assert!(stderr(&out).contains("--force"), "{}", stderr(&out));
    // A service that names a missing secret is refused; jobs are not supported.
    let out = docker_out(
        &sb,
        &service,
        &[
            "service", "create", "--secret", "nope", "--name", "x", "alpine",
        ],
    );
    assert_ne!(out.status.code(), Some(0));
    let (body, head) = curl(
        &service.socket,
        &[
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            r#"{"Name":"job","Mode":{"ReplicatedJob":{}},"TaskTemplate":{"ContainerSpec":{"Image":"alpine"}}}"#,
            "/v1.47/services/create",
        ],
    );
    assert!(head.contains(" 501 "), "{head} {body}");
    let out = docker_out(&sb, &service, &["swarm", "leave", "--force"]);
    assert!(out.status.success(), "{}", stderr(&out));
}
