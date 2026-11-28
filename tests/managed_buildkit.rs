#![cfg(target_os = "linux")]
//! Tests against the installed BuildKit and OCI worker, without pulling build images.

mod common;
use common::api::{Service, curl, docker_out, wait_for};
use common::{Sandbox, root_mode, stderr, stdout};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn enabled() -> bool {
    if std::env::var_os("PODBAG_TEST_BUILDKIT").as_deref() != Some(std::ffi::OsStr::new("1")) {
        eprintln!("PODBAG_TEST_BUILDKIT is not 1: skipped");
        return false;
    }
    true
}

fn checked(out: Output) -> Output {
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    out
}

fn read(path: &Path) -> Vec<u8> {
    if root_mode() {
        checked(
            Command::new("sudo")
                .args(["-n", "cat"])
                .arg(path)
                .output()
                .unwrap(),
        )
        .stdout
    } else {
        std::fs::read(path).unwrap()
    }
}

fn build(sb: &Sandbox, context: &Path, options: &[&str]) -> Output {
    checked(
        sb.podbag()
            .args(["build", "--buildkit", "--network", "none"])
            .args(options)
            .arg(context)
            .output()
            .unwrap(),
    )
}

fn workers(sb: &Sandbox) -> Vec<OsString> {
    sb.data()
        .join("builds")
        .read_dir()
        .unwrap()
        .filter_map(|entry| entry.ok().map(|e| e.file_name()))
        .filter(|name| name.to_string_lossy().starts_with("buildkit-"))
        .collect()
}

fn clean(sb: &Sandbox) {
    let leftovers = workers(sb);
    assert!(
        leftovers.is_empty(),
        "leftover BuildKit state: {leftovers:?}"
    );
}

struct Agent(std::process::Child);
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn secrets_ssh_and_cache_mounts_make_an_image_without_leaking_a_secret() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    let context = sb.tmp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    let busybox = std::env::var_os("PODBAG_TEST_BUSYBOX").unwrap_or("/usr/bin/busybox".into());
    std::fs::copy(busybox, context.join("busybox")).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        r#"
FROM scratch
COPY busybox /bin/busybox
SHELL ["/bin/busybox", "sh", "-c"]
RUN --mount=type=secret,id=fixture,required=true --mount=type=ssh,required=true --mount=type=cache,target=/cache \
  test -s /run/secrets/fixture && test -S "$SSH_AUTH_SOCK" && echo ready >/cache/ready
RUN --mount=type=cache,target=/cache test -s /cache/ready && echo advanced-build >/result \
  && /bin/busybox mkdir -m 700 /owned && echo worker >/owned/note \
  && /bin/busybox chown -R 1000:1000 /owned
CMD ["/bin/busybox", "cat", "/result"]
"#,
    )
    .unwrap();
    let secret = sb.tmp.path().join("secret");
    let marker = "podbag-test-secret-do-not-copy";
    std::fs::write(&secret, marker).unwrap();
    let agent_socket = sb.tmp.path().join("agent.sock");
    let mut agent = Agent(
        Command::new("ssh-agent")
            .args(["-D", "-a"])
            .arg(&agent_socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !agent_socket.exists() {
        assert!(agent.0.try_wait().unwrap().is_none());
        assert!(
            std::time::Instant::now() < deadline,
            "the SSH agent did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let out = build(
        &sb,
        &context,
        &[
            "-t",
            "managed-kit:verified",
            "-t",
            "managed-kit:second",
            "--secret",
            &format!("id=fixture,src={}", secret.display()),
            "--ssh",
            &format!("default={}", agent_socket.display()),
        ],
    );
    assert!(!stdout(&out).contains(marker) && !stderr(&out).contains(marker));
    let ran = checked(
        sb.podbag()
            .args(["run", "--rm", "--network", "none", "managed-kit:second"])
            .output()
            .unwrap(),
    );
    assert_eq!(stdout(&ran).trim(), "advanced-build");
    let archive = checked(
        sb.podbag()
            .args(["save", "managed-kit:verified"])
            .output()
            .unwrap(),
    );
    assert!(
        !archive
            .stdout
            .windows(marker.len())
            .any(|part| part == marker.as_bytes())
    );
    clean(&sb);
    println!(
        "PASS: BuildKit secret, SSH agent, cache mounts, image import, and private-worker cleanup"
    );
}

#[test]
fn worker_exports_oci_platforms_and_reuses_external_cache() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    let context = sb.tmp.path().join("build");
    std::fs::create_dir(&context).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        "FROM scratch\nCOPY note /note\n",
    )
    .unwrap();
    std::fs::write(context.join("note"), "from an actual BuildKit worker\n").unwrap();
    let archive = sb.tmp.path().join("multi.tar");
    build(
        &sb,
        &context,
        &[
            "--platform",
            "linux/amd64,linux/arm64",
            "--output",
            &format!("type=oci,dest={}", archive.display()),
        ],
    );
    let archive = read(&archive);
    let mut archive = tar::Archive::new(&archive[..]);
    let mut architectures = std::collections::BTreeSet::new();
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        let Ok(value) = serde_json::from_reader::<_, serde_json::Value>(entry) else {
            continue;
        };
        for manifest in value["manifests"].as_array().iter().flat_map(|m| m.iter()) {
            if let Some(arch) = manifest["platform"]["architecture"].as_str() {
                architectures.insert(arch.to_owned());
            }
        }
    }
    assert!(architectures.contains("amd64") && architectures.contains("arm64"));

    let cache = sb.tmp.path().join("cache");
    let first = sb.tmp.path().join("first");
    let second = sb.tmp.path().join("second");
    build(
        &sb,
        &context,
        &[
            "--output",
            &format!("type=local,dest={}", first.display()),
            "--cache-to",
            &format!("type=local,dest={},mode=max", cache.display()),
        ],
    );
    let reused = build(
        &sb,
        &context,
        &[
            "--output",
            &format!("type=local,dest={}", second.display()),
            "--cache-from",
            &format!("type=local,src={}", cache.display()),
        ],
    );
    assert!(stderr(&reused).contains("CACHED"), "{}", stderr(&reused));
    assert_eq!(
        read(&second.join("note")),
        b"from an actual BuildKit worker\n"
    );
    clean(&sb);
    println!("PASS: BuildKit multi-platform OCI archive, local exporter, cache export and import");
}

#[test]
fn interrupting_the_client_stops_the_worker_and_removes_its_state() {
    if !enabled() || root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let context = sb.tmp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    let busybox = std::env::var_os("PODBAG_TEST_BUSYBOX").unwrap_or("/usr/bin/busybox".into());
    std::fs::copy(busybox, context.join("busybox")).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        "FROM scratch\nCOPY busybox /bin/busybox\nRUN [\"/bin/busybox\", \"sleep\", \"30\"]\n",
    )
    .unwrap();
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut build = Agent(
        sb.podbag()
            .args(["build", "--buildkit", "--network", "none"])
            .arg(&context)
            .stdout(Stdio::null())
            .stderr(log.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_to_string(log.path())
        .unwrap()
        .contains("RUN [")
    {
        assert!(build.0.try_wait().unwrap().is_none());
        assert!(
            std::time::Instant::now() < deadline,
            "the RUN step did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // SAFETY: this is the child we started and have not yet reaped.
    assert_eq!(unsafe { libc::kill(build.0.id() as i32, libc::SIGINT) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = build.0.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the build did not stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    clean(&sb);
    println!("PASS: interrupting BuildKit stops the private worker and removes its state");
}

/// Set PODBAG_TEST_BUILDX to an executable Docker Buildx plugin to enable this test.
#[test]
fn engine_driver_transfers_context_secret_and_image_and_reaps_its_worker() {
    let Some(buildx) = std::env::var_os("PODBAG_TEST_BUILDX") else {
        eprintln!("PODBAG_TEST_BUILDX is unset: skipped");
        return;
    };
    let buildx = std::fs::canonicalize(buildx).unwrap();
    let sb = Sandbox::new();
    let plugins = sb.tmp.path().join("docker-config/cli-plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::os::unix::fs::symlink(buildx, plugins.join("docker-buildx")).unwrap();

    let context = sb.tmp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    let busybox = std::env::var_os("PODBAG_TEST_BUSYBOX").unwrap_or("/usr/bin/busybox".into());
    std::fs::copy(busybox, context.join("busybox")).unwrap();
    std::fs::write(context.join("note"), "engine build\n").unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        "FROM scratch\nCOPY busybox /bin/busybox\nCOPY note /note\nRUN --mount=type=secret,id=fixture,required=true [\"/bin/busybox\", \"test\", \"-s\", \"/run/secrets/fixture\"]\nCMD [\"/bin/busybox\", \"cat\", \"/note\"]\n",
    )
    .unwrap();
    let secret = sb.tmp.path().join("secret");
    let marker = "podbag-test-engine-secret-do-not-copy";
    std::fs::write(&secret, marker).unwrap();

    let service = Service::start(&sb, &["--buildkit", "--timeout", "0"]);
    let (_, ping) = curl(&service.socket, &["/_ping"]);
    assert!(ping.contains("Builder-Version: 1"), "{ping}");
    let archive = sb.tmp.path().join("image.tar");
    let output = checked(docker_out(
        &sb,
        &service,
        &[
            "buildx",
            "build",
            "--builder",
            "default",
            "--network",
            "none",
            "--secret",
            &format!("id=fixture,src={}", secret.display()),
            "--tag",
            "engine-kit:verified",
            "--output",
            &format!("type=docker,dest={}", archive.display()),
            context.to_str().unwrap(),
        ],
    ));
    assert!(!stdout(&output).contains(marker) && !stderr(&output).contains(marker));
    assert!(
        !read(&archive)
            .windows(marker.len())
            .any(|part| part == marker.as_bytes())
    );
    checked(docker_out(
        &sb,
        &service,
        &["load", "--input", archive.to_str().unwrap()],
    ));
    let ran = checked(
        sb.podbag()
            .args(["run", "--rm", "--network", "none", "engine-kit:verified"])
            .output()
            .unwrap(),
    );
    assert_eq!(stdout(&ran), "engine build");
    assert!(wait_for(Duration::from_secs(25), || workers(&sb).is_empty()));
    println!("PASS: stock Buildx session, archive import, and idle-worker cleanup");
}

#[test]
fn malformed_secret_options_are_rejected_without_echoing_the_value() {
    let sb = Sandbox::new();
    std::fs::write(sb.tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let marker = "podbag-test-invalid-secret";
    let out = sb
        .podbag()
        .args([
            "build",
            "--buildkit",
            "--secret",
            &format!("id=example,value={marker}"),
        ])
        .arg(sb.tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("invalid BuildKit secret"));
    assert!(!stderr(&out).contains(marker));
    clean(&sb);
}
