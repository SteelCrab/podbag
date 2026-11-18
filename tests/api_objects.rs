#![cfg(target_os = "linux")]
//! Volumes and networks through the Docker Engine API.

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::process::Output;

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

struct Setup {
    _reg: FakeRegistry,
    sb: Sandbox,
    service: Service,
    image: String,
}

fn setup() -> Setup {
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
    let s = Setup {
        _reg: reg,
        sb,
        service,
        image,
    };
    s.ok(&["pull", &s.image]);
    s
}

impl Setup {
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

    fn lines(&self, args: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = self.ok(args).lines().map(String::from).collect();
        v.sort();
        v
    }
}

#[test]
fn docker_volume_lifecycle() {
    let s = setup();
    assert_eq!(s.ok(&["volume", "create", "v1"]), "v1");
    assert!(
        s.lines(&["volume", "ls", "--format", "{{.Name}}"])
            .contains(&"v1".to_string())
    );
    let mountpoint = s.ok(&["volume", "inspect", "--format", "{{.Mountpoint}}", "v1"]);
    assert!(mountpoint.ends_with("/volumes/v1/data"), "{mountpoint}");
    assert_eq!(
        s.ok(&[
            "volume",
            "inspect",
            "--format",
            "{{.Driver}} {{.Scope}}",
            "v1"
        ]),
        "local local"
    );
    s.ok(&["volume", "rm", "v1"]);
    assert!(s.lines(&["volume", "ls", "-q"]).is_empty());
    let out = s.docker(&["volume", "inspect", "v1"]);
    assert_ne!(out.status.code(), Some(0));

    // A removed container leaves its anonymous volume; prune takes it, and with -a the
    // named ones too.
    s.ok(&["run", "--name", "anon", "-v", "/data", &s.image, "true"]);
    s.ok(&["rm", "anon"]);
    let left = s.lines(&["volume", "ls", "-q"]);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].len(), 64, "{left:?}");
    s.ok(&["volume", "create", "named"]);
    s.ok(&["volume", "prune", "-f"]);
    assert_eq!(s.lines(&["volume", "ls", "-q"]), ["named"]);
    s.ok(&["volume", "prune", "-a", "-f"]);
    assert!(s.lines(&["volume", "ls", "-q"]).is_empty());
}

#[test]
fn docker_volume_in_use_is_a_conflict() {
    let s = setup();
    s.ok(&["run", "--name", "user", "-v", "busy:/b", &s.image, "true"]);
    let out = s.docker(&["volume", "rm", "busy"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("used by"), "{}", stderr(&out));
    s.ok(&["volume", "prune", "-a", "-f"]);
    assert_eq!(
        s.lines(&["volume", "ls", "-q"]),
        ["busy"],
        "in use: not pruned"
    );
    s.ok(&["rm", "user"]);
    s.ok(&["volume", "rm", "busy"]);
}

#[test]
fn docker_network_lifecycle() {
    let s = setup();
    let id = s.ok(&["network", "create", "--subnet", "10.79.5.0/24", "apiobj-n1"]);
    assert_eq!(id.len(), 64, "{id}");
    let ls = s.lines(&["network", "ls", "--format", "{{.Name}} {{.Driver}}"]);
    for want in [
        "bridge bridge",
        "host host",
        "none null",
        "apiobj-n1 bridge",
    ] {
        assert!(ls.contains(&want.to_string()), "{want} missing from {ls:?}");
    }
    let subnet = s.ok(&[
        "network",
        "inspect",
        "--format",
        "{{(index .IPAM.Config 0).Subnet}}",
        "apiobj-n1",
    ]);
    assert_eq!(subnet, "10.79.5.0/24");
    assert_eq!(
        s.ok(&["network", "inspect", "--format", "{{.Name}}", &id[..12]]),
        "apiobj-n1",
        "found by ID"
    );
    s.ok(&[
        "run",
        "-d",
        "--name",
        "member",
        "--network",
        "apiobj-n1",
        &s.image,
        "sleep",
        "30",
    ]);
    let ip = s.ok(&[
        "inspect",
        "--format",
        "{{(index .NetworkSettings.Networks \"apiobj-n1\").IPAddress}}",
        "member",
    ]);
    assert!(ip.starts_with("10.79.5."), "{ip}");
    let members = s.ok(&[
        "network",
        "inspect",
        "--format",
        "{{range .Containers}}{{.Name}}{{end}}",
        "apiobj-n1",
    ]);
    assert_eq!(members, "member");
    let out = s.docker(&["network", "rm", "apiobj-n1"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("member"), "{}", stderr(&out));
    s.ok(&["rm", "-f", "member"]);
    s.ok(&["network", "rm", "apiobj-n1"]);
    let ls = s.lines(&["network", "ls", "--format", "{{.Name}}"]);
    assert!(!ls.contains(&"apiobj-n1".to_string()), "{ls:?}");

    s.ok(&["network", "create", "apiobj-n2"]);
    s.ok(&["network", "prune", "-f"]);
    let ls = s.lines(&["network", "ls", "--format", "{{.Name}}"]);
    assert!(!ls.contains(&"apiobj-n2".to_string()), "{ls:?}");
    assert!(ls.contains(&"bridge".to_string()), "{ls:?}");

    let out = s.docker(&["network", "rm", "bridge"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("pre-defined"), "{}", stderr(&out));
    let out = s.docker(&["network", "create", "-d", "overlay", "apiobj-n3"]);
    assert_ne!(out.status.code(), Some(0));
}
