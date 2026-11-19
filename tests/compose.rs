#![cfg(target_os = "linux")]
//! Docker Compose against `podbag system service`: the real `docker compose`.
//! Skipped with a message when the docker CLI has no compose plugin.

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::process::Output;
use std::time::{Duration, Instant};

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

struct Project {
    _reg: FakeRegistry,
    _turn: Option<std::fs::File>,
    sb: Sandbox,
    service: Service,
    image: String,
    file: std::path::PathBuf,
}

fn have_compose() -> bool {
    let ok = std::process::Command::new("docker")
        .args(["compose", "version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("docker compose is missing: skipped");
    }
    ok
}

/// A project named `p` from `compose` (with `IMAGE` standing for a test image).
fn project(compose: &str) -> Project {
    let turn = network_turn();
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
    let dir = sb.tmp.path().join("project");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("compose.yaml");
    std::fs::write(&file, compose.replace("IMAGE", &image)).unwrap();
    Project {
        _reg: reg,
        _turn: turn,
        sb,
        service,
        image,
        file,
    }
}

impl Project {
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

    fn compose(&self, args: &[&str]) -> Output {
        let file = self.file.display().to_string();
        let mut all = vec!["compose", "-f", &file, "-p", "p"];
        all.extend_from_slice(args);
        self.docker(&all)
    }

    fn compose_ok(&self, args: &[&str]) -> String {
        let out = self.compose(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "compose {args:?}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    fn inspect(&self, name: &str) -> serde_json::Value {
        let text = self.ok(&["inspect", name]);
        serde_json::from_str::<serde_json::Value>(&text).unwrap()[0].clone()
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .ok(&["ps", "-aq", "--no-trunc"])
            .lines()
            .map(String::from)
            .collect();
        ids.sort();
        ids
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.compose(&["down", "-v", "--timeout", "0"]);
    }
}

#[test]
fn up_twice_then_down_leaves_nothing() {
    if !have_compose() {
        return;
    }
    let p = project(
        "services:
  web:
    image: IMAGE
    command: [sleep, '300']
    volumes: [data:/srv]
  db:
    image: IMAGE
    command: [sleep, '300']
volumes:
  data: {}
",
    );
    p.compose_ok(&["up", "-d"]);
    let first = p.ids();
    assert_eq!(first.len(), 2);
    // Nothing changed: nothing is made again.
    p.compose_ok(&["up", "-d"]);
    assert_eq!(p.ids(), first);
    let networks = p.ok(&["network", "ls", "--format", "{{.Name}}"]);
    assert!(networks.lines().any(|n| n == "p_default"), "{networks}");
    p.compose_ok(&["down", "--timeout", "0"]);
    assert!(p.ids().is_empty());
    let networks = p.ok(&["network", "ls", "--format", "{{.Name}}"]);
    assert!(!networks.lines().any(|n| n == "p_default"), "{networks}");
    // The volume stays until `down -v`.
    assert!(p.ok(&["volume", "ls", "-q"]).lines().any(|v| v == "p_data"));
    p.compose_ok(&["up", "-d"]);
    p.compose_ok(&["down", "-v", "--timeout", "0"]);
    assert!(!p.ok(&["volume", "ls", "-q"]).lines().any(|v| v == "p_data"));
}

#[test]
fn services_find_each_other_by_name() {
    if !have_compose() {
        return;
    }
    let p = project(
        "services:
  web:
    image: IMAGE
    command: [sleep, '300']
  db:
    image: IMAGE
    command: [sleep, '300']
",
    );
    p.compose_ok(&["up", "-d"]);
    p.compose_ok(&["exec", "web", "ping", "-c1", "-W2", "db"]);
    p.compose_ok(&["exec", "db", "ping", "-c1", "-W2", "web"]);
    // By the container's name too.
    p.compose_ok(&["exec", "web", "ping", "-c1", "-W2", "p-db-1"]);
}

#[test]
fn depends_on_waits_for_a_healthy_service() {
    if !have_compose() {
        return;
    }
    let p = project(
        "services:
  db:
    image: IMAGE
    command: [sh, -c, 'sleep 2; touch /run/ready; sleep 300']
    tmpfs: [/run]
    healthcheck:
      test: [CMD, test, -e, /run/ready]
      interval: 500ms
      retries: 20
  web:
    image: IMAGE
    command: [sleep, '300']
    depends_on:
      db:
        condition: service_healthy
",
    );
    p.compose_ok(&["up", "-d"]);
    let db = p.inspect("p-db-1");
    assert_eq!(db["State"]["Health"]["Status"], "healthy", "{db}");
    let web = p.inspect("p-web-1");
    assert_eq!(web["State"]["Status"], "running", "{web}");
    // Started after the first successful check.
    let healthy_at = db["State"]["Health"]["Log"]
        .as_array()
        .unwrap()
        .iter()
        .find(|probe| probe["ExitCode"] == 0)
        .map(|probe| probe["End"].as_str().unwrap().to_string())
        .unwrap();
    let started = web["State"]["StartedAt"].as_str().unwrap().to_string();
    assert!(started[..19] >= healthy_at[..19], "{started} {healthy_at}");
}

#[test]
fn a_service_with_restart_always_comes_back() {
    if !have_compose() {
        return;
    }
    let p = project(
        "services:
  job:
    image: IMAGE
    command: [sh, -c, 'sleep 1; exit 3']
    restart: always
",
    );
    p.compose_ok(&["up", "-d"]);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let job = p.inspect("p-job-1");
        if job["RestartCount"].as_u64().unwrap_or(0) >= 1 {
            assert_eq!(
                job["HostConfig"]["RestartPolicy"]["Name"], "always",
                "{job}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "never started again: {job}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn run_exec_logs_and_ps() {
    if !have_compose() {
        return;
    }
    let p = project(
        "services:
  web:
    image: IMAGE
    command: [sh, -c, 'echo hello from web; sleep 300']
",
    );
    p.compose_ok(&["up", "-d"]);
    let ran = p.compose_ok(&["run", "--rm", "web", "echo", "one-off"]);
    assert!(ran.contains("one-off"), "{ran}");
    let exec = p.compose_ok(&["exec", "web", "echo", "inside"]);
    assert!(exec.contains("inside"), "{exec}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let logs = p.compose_ok(&["logs", "web"]);
        if logs.contains("hello from web") {
            break;
        }
        assert!(Instant::now() < deadline, "{logs}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let ps = p.compose_ok(&["ps", "--format", "{{.Service}} {{.State}}"]);
    assert!(ps.lines().any(|l| l == "web running"), "{ps}");
    // The one-off container is gone.
    assert_eq!(p.ids().len(), 1);
    let _ = &p.image;
}

#[test]
fn a_project_with_two_networks() {
    if !have_compose() {
        return;
    }
    let p = project(
        "services:
  a:
    image: IMAGE
    command: [sleep, '300']
    networks: [front]
  b:
    image: IMAGE
    command: [sleep, '300']
    networks: [front, back]
  c:
    image: IMAGE
    command: [sleep, '300']
    networks: [back]
networks:
  front: {}
  back: {}
",
    );
    p.compose_ok(&["up", "-d"]);
    p.compose_ok(&["exec", "a", "ping", "-c1", "-W2", "b"]);
    p.compose_ok(&["exec", "c", "ping", "-c1", "-W2", "b"]);
    p.compose_ok(&["exec", "b", "ping", "-c1", "-W2", "a"]);
    p.compose_ok(&["exec", "b", "ping", "-c1", "-W2", "c"]);
    let hosts = p.compose_ok(&["exec", "a", "cat", "/etc/hosts"]);
    assert!(
        !hosts.contains(" c\n") && !hosts.contains("\tc\n"),
        "{hosts}"
    );
    // Nothing is made again on a second `up`.
    let first = p.ids();
    p.compose_ok(&["up", "-d"]);
    assert_eq!(p.ids(), first);
}
