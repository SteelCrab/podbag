#![cfg(target_os = "linux")]
//! The classic builder, `commit`, `diff` and `export` through `podbag system service`,
//! with the real docker CLI and Compose. The base image comes from a fake registry; RUN
//! steps use no network. Skipped with a message when the docker CLI is missing.
//!
//! The docker CLI sends builds to its buildx plugin when that is installed;
//! `DOCKER_BUILDKIT=0` selects the classic builder (`POST /build`).

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::process::{Command, Output};

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

struct Setup {
    _reg: FakeRegistry,
    sb: Sandbox,
    service: Service,
    base: String,
}

fn have_docker() -> bool {
    let ok = Command::new("docker")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("the docker CLI is missing: skipped");
    }
    ok
}

impl Setup {
    fn new() -> Setup {
        let reg = FakeRegistry::start(false);
        reg.push(
            "base",
            "v1",
            &TestImage {
                layers: vec![alpine_layer()],
                config: serde_json::json!({"Env": [PATH], "Cmd": ["sh"]}),
            },
        );
        let base = format!("{}/base:v1", reg.host);
        let sb = Sandbox::new();
        let service = Service::start(&sb, &["--timeout", "0"]);
        Setup {
            _reg: reg,
            sb,
            service,
            base,
        }
    }

    /// The docker CLI with the classic builder (and `extra` environment).
    fn docker(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let config = self.sb.tmp.path().join("docker-config");
        std::fs::create_dir_all(&config).unwrap();
        let host = format!("unix://{}", self.service.socket.display());
        let mut env = vec![
            ("DOCKER_BUILDKIT".to_string(), "0".to_string()),
            ("DOCKER_CONFIG".to_string(), config.display().to_string()),
            ("DOCKER_HOST".to_string(), host),
        ];
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let mut cmd = if root_mode() {
            let mut c = Command::new("sudo");
            c.args(["-n", "env"])
                .args(env.iter().map(|(k, v)| format!("{k}={v}")))
                .arg("docker");
            c
        } else {
            let mut c = Command::new("docker");
            c.envs(env);
            c
        };
        cmd.args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.docker(args, &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "docker {args:?}\nstdout: {}\nstderr: {}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    fn context(&self, files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.sb.tmp.path().join("ctx");
        for (path, content) in files {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(path), content.replace("@BASE@", &self.base)).unwrap();
            std::fs::set_permissions(dir.join(path), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        for d in [self.sb.tmp.path(), dir.as_path()] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }
}

#[test]
fn ping_announces_the_classic_builder() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &[]);
    let (body, head) = curl(&service.socket, &["/_ping"]);
    assert_eq!(body, "OK");
    assert!(
        head.to_ascii_lowercase().contains("builder-version: 1"),
        "{head}"
    );
}

#[test]
fn docker_build_uses_the_classic_builder() {
    if !have_docker() {
        return;
    }
    let s = Setup::new();
    let dir = s.context(&[
        (
            "Dockerfile",
            "FROM @BASE@\nARG WHO=nobody\nRUN echo \"hello $WHO\" > /hello\nCOPY note /note\n\
             LABEL made.by=test\nCMD [\"cat\", \"/hello\", \"/note\"]\n",
        ),
        ("note", "from the context\n"),
        ("skipped", "left out\n"),
        (".dockerignore", "skipped\n"),
    ]);
    let ctx = dir.display().to_string();
    let out = s.ok(&[
        "build",
        "--network",
        "none",
        "--build-arg",
        "WHO=docker",
        "-t",
        "built:1",
        &ctx,
    ]);
    assert!(out.contains("Successfully built"), "{out}");
    assert!(out.contains("Successfully tagged built:1"), "{out}");
    assert_eq!(
        s.ok(&["run", "--rm", "--network", "none", "built:1"]),
        "hello docker\nfrom the context"
    );
    assert_eq!(
        s.ok(&[
            "image",
            "inspect",
            "-f",
            "{{json .Config.Labels}}",
            "built:1"
        ]),
        r#"{"made.by":"test"}"#
    );
    // Again: from the cache, and -q prints only the ID.
    let again = s.ok(&[
        "build",
        "-q",
        "--network",
        "none",
        "--build-arg",
        "WHO=docker",
        "-t",
        "built:1",
        &ctx,
    ]);
    assert!(again.starts_with("sha256:"), "{again}");
    assert_eq!(
        again,
        s.ok(&["image", "inspect", "-f", "{{.Id}}", "built:1"])
    );
    // A failing step fails the build with its code.
    std::fs::write(
        dir.join("Dockerfile"),
        format!("FROM {}\nRUN exit 7\n", s.base),
    )
    .unwrap();
    let failed = s.docker(&["build", "--network", "none", &ctx], &[]);
    assert_ne!(failed.status.code(), Some(0));
    assert!(
        format!("{}{}", stdout(&failed), stderr(&failed)).contains("non-zero code: 7"),
        "{}",
        stderr(&failed)
    );
}

#[test]
fn compose_build_uses_the_classic_builder() {
    if !have_docker() {
        return;
    }
    let s = Setup::new();
    let compose_ok = s.docker(&["compose", "version"], &[]).status.success();
    if !compose_ok {
        eprintln!("docker compose is missing: skipped");
        return;
    }
    let dir = s.context(&[
        ("Dockerfile", "FROM @BASE@\nRUN echo composed > /made\nCMD [\"cat\", \"/made\"]\n"),
        (
            "compose.yaml",
            "services:\n  app:\n    image: composed:1\n    build:\n      context: .\n      network: none\n",
        ),
    ]);
    let file = dir.join("compose.yaml").display().to_string();
    s.ok(&["compose", "-f", &file, "-p", "p", "build"]);
    assert_eq!(
        s.ok(&["run", "--rm", "--network", "none", "composed:1"]),
        "composed"
    );
}

#[test]
fn commit_diff_and_export_through_the_api() {
    if !have_docker() {
        return;
    }
    let s = Setup::new();
    s.ok(&["pull", &s.base]);
    s.ok(&[
        "run",
        "-d",
        "--name",
        "c",
        "--network",
        "none",
        &s.base,
        "sleep",
        "300",
    ]);
    s.ok(&["exec", "c", "sh", "-c", "echo hi > /made && rm /etc/motd"]);
    let diff = s.ok(&["diff", "c"]);
    assert!(diff.lines().any(|l| l == "A /made"), "{diff}");
    assert!(diff.lines().any(|l| l == "D /etc/motd"), "{diff}");
    let id = s.ok(&["commit", "-c", "CMD [\"cat\", \"/made\"]", "c", "snap:1"]);
    assert!(id.starts_with("sha256:"), "{id}");
    assert_eq!(s.ok(&["run", "--rm", "--network", "none", "snap:1"]), "hi");
    let tar = s.sb.tmp.path().join("c.tar");
    s.ok(&["export", "-o", tar.to_str().unwrap(), "c"]);
    let list = Command::new(if root_mode() { "sudo" } else { "tar" })
        .args(if root_mode() {
            vec!["-n", "tar"]
        } else {
            vec![]
        })
        .arg("-tf")
        .arg(&tar)
        .output()
        .unwrap();
    assert!(
        stdout(&list).lines().any(|l| l == "made"),
        "{}",
        stdout(&list)
    );
    s.ok(&["rm", "-f", "c"]);
}
