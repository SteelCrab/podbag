#![cfg(target_os = "linux")]
//! buildx's docker-container driver against `openpod system service`: BuildKit runs in a
//! privileged openpod container, builds through `buildctl dial-stdio` over exec, and the
//! image comes back through `POST /images/load`. Pulls moby/buildkit and alpine from Docker
//! Hub: runs only with OPENPOD_TEST_NETWORK=1.

mod common;
use common::api::*;
use common::*;
use std::process::Output;

fn enabled() -> bool {
    if std::env::var("OPENPOD_TEST_NETWORK").as_deref() != Ok("1") {
        eprintln!("OPENPOD_TEST_NETWORK is not 1: skipped");
        return false;
    }
    let buildx = std::process::Command::new("docker")
        .args(["buildx", "version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !buildx {
        eprintln!("docker buildx is missing: skipped");
    }
    buildx
}

struct Setup {
    _turn: Option<std::fs::File>,
    sb: Sandbox,
    service: Option<Service>,
    context: std::path::PathBuf,
}

impl Setup {
    fn new() -> Setup {
        let turn = network_turn();
        let sb = Sandbox::new();
        let service = Service::start(&sb, &["--timeout", "0"]);
        let context = sb.tmp.path().join("context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            "FROM alpine\nRUN echo built-here > /built\nCOPY note /note\nCMD [\"cat\", \"/built\", \"/note\"]\n",
        )
        .unwrap();
        std::fs::write(context.join("note"), "from the context\n").unwrap();
        // Readable by root's docker CLI in root mode.
        use std::os::unix::fs::PermissionsExt;
        for p in [sb.tmp.path().to_path_buf(), context.clone()] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Setup {
            _turn: turn,
            sb,
            service: Some(service),
            context,
        }
    }

    fn docker(&self, args: &[&str]) -> Output {
        docker_out(&self.sb, self.service.as_ref().unwrap(), args)
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

    fn build(&self, builder: &str, tag: &str) {
        let context = self.context.display().to_string();
        self.ok(&[
            "buildx",
            "build",
            "--builder",
            builder,
            "--load",
            "-t",
            tag,
            &context,
        ]);
    }
}

#[test]
fn buildx_builds_and_loads_an_image() {
    if !enabled() {
        return;
    }
    let s = Setup::new();
    s.ok(&[
        "buildx",
        "create",
        "--driver",
        "docker-container",
        "--name",
        "b1",
    ]);
    s.build("b1", "local/built:v1");
    assert_eq!(
        s.ok(&["run", "--rm", "local/built:v1"]),
        "built-here\nfrom the context"
    );
    // The builder is an openpod container, privileged.
    let privileged = s.ok(&[
        "inspect",
        "--format",
        "{{.HostConfig.Privileged}}",
        "buildx_buildkit_b10",
    ]);
    assert_eq!(privileged, "true");
    // The ID a quiet build prints (the config's digest) names the image.
    let context = s.context.display().to_string();
    let id = s.ok(&[
        "buildx",
        "build",
        "--builder",
        "b1",
        "-q",
        "--load",
        &context,
    ]);
    let id = id.lines().last().unwrap_or_default().to_string();
    assert_eq!(
        s.ok(&["run", "--rm", &id]),
        "built-here\nfrom the context",
        "{id}"
    );
    // Stopped and started again: BuildKit made cgroups of its own in the container's.
    s.ok(&["buildx", "stop", "b1"]);
    s.build("b1", "local/again:v1");
    assert_eq!(
        s.ok(&["run", "--rm", "local/again:v1"]),
        "built-here\nfrom the context"
    );
    // Compose builds a service's image with it.
    let compose = s.sb.tmp.path().join("compose.yaml");
    std::fs::write(
        &compose,
        format!(
            "services:\n  app:\n    build: {}\n    image: local/composed:v1\n",
            s.context.display()
        ),
    )
    .unwrap();
    s.ok(&[
        "compose",
        "-f",
        &compose.display().to_string(),
        "-p",
        "p",
        "build",
        "--builder",
        "b1",
    ]);
    assert_eq!(
        s.ok(&["run", "--rm", "local/composed:v1"]),
        "built-here\nfrom the context"
    );
    s.ok(&["buildx", "rm", "b1"]);
    let gone = s.docker(&["inspect", "buildx_buildkit_b10"]);
    assert_ne!(gone.status.code(), Some(0), "the builder container stayed");
}

#[test]
fn the_builder_outlives_the_service() {
    if !enabled() {
        return;
    }
    let mut s = Setup::new();
    s.ok(&[
        "buildx",
        "create",
        "--driver",
        "docker-container",
        "--name",
        "b2",
    ]);
    s.build("b2", "local/first:v1");
    // The service ends; the builder keeps running under the keeper.
    s.service = None;
    s.service = Some(Service::start(&s.sb, &["--timeout", "0"]));
    let state = s.ok(&[
        "inspect",
        "--format",
        "{{.State.Running}}",
        "buildx_buildkit_b20",
    ]);
    assert_eq!(state, "true");
    std::fs::write(s.context.join("note"), "changed\n").unwrap();
    s.build("b2", "local/second:v1");
    assert_eq!(
        s.ok(&["run", "--rm", "local/second:v1"]),
        "built-here\nchanged"
    );
    s.ok(&["buildx", "rm", "b2"]);
}
