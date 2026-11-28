#![cfg(target_os = "linux")]
//! Tests against the installed BuildKit and OCI worker, without pulling build images.

mod common;
use common::api::{Service, curl, docker_out, wait_for};
use common::{Sandbox, root_mode, stderr, stdout};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
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

/// Read the exported layers named by the Docker archive's manifest.
fn decoded_docker_layers(bytes: &[u8]) -> Vec<Vec<u8>> {
    use podbag::build::plan::{Packing, packing};
    use std::collections::BTreeMap;
    use std::io::Read;

    let mut files = BTreeMap::new();
    for entry in tar::Archive::new(bytes).entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.header().entry_type().is_file() {
            let name = entry.path().unwrap().into_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            files.insert(name, data);
        }
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&files[Path::new("manifest.json")]).unwrap();
    manifest[0]["Layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| {
            let data = &files[Path::new(layer.as_str().unwrap())];
            let mut reader: Box<dyn Read> = match packing(data) {
                Packing::Gzip => Box::new(flate2::read::MultiGzDecoder::new(data.as_slice())),
                Packing::Zstd => Box::new(podbag::store::zstd::ZstdFrames::new(data.as_slice())),
                Packing::Tar => Box::new(data.as_slice()),
                Packing::None if data.iter().all(|&b| b == 0) => Box::new(data.as_slice()),
                other => panic!("unexpected layer format {other:?}"),
            };
            let mut decoded = Vec::new();
            reader.read_to_end(&mut decoded).unwrap();
            decoded
        })
        .collect()
}

fn oci_image_platforms(archive: &[u8]) -> std::collections::BTreeSet<String> {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Read;

    let mut files = BTreeMap::new();
    for entry in tar::Archive::new(archive).entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.header().entry_type().is_file() {
            let path = entry.path().unwrap().into_owned();
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).unwrap();
            assert!(files.insert(path, contents).is_none());
        }
    }
    let blob = |descriptor: &serde_json::Value| {
        let digest = descriptor["digest"].as_str().unwrap();
        let (algorithm, hash) = digest.split_once(':').unwrap();
        &files[&PathBuf::from("blobs").join(algorithm).join(hash)]
    };
    let root: serde_json::Value = serde_json::from_slice(&files[Path::new("index.json")]).unwrap();
    let mut pending = root["manifests"].as_array().unwrap().clone();
    let mut visited = BTreeSet::new();
    let mut platforms = BTreeSet::new();
    while let Some(descriptor) = pending.pop() {
        if !visited.insert(descriptor["digest"].as_str().unwrap().to_owned()) {
            continue;
        }
        let document: serde_json::Value = serde_json::from_slice(blob(&descriptor)).unwrap();
        if let Some(manifests) = document["manifests"].as_array() {
            pending.extend(manifests.iter().cloned());
        } else {
            let config: serde_json::Value =
                serde_json::from_slice(blob(&document["config"])).unwrap();
            if config["os"] == "linux" {
                let layers = document["layers"].as_array().unwrap();
                assert!(!layers.is_empty());
                for layer in layers {
                    assert!(!blob(layer).is_empty());
                }
                platforms.insert(config["architecture"].as_str().unwrap().to_owned());
            }
        }
    }
    platforms
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
    assert_eq!(
        oci_image_platforms(&read(&archive)),
        ["amd64".to_owned(), "arm64".to_owned()].into()
    );

    let busybox = std::env::var_os("PODBAG_TEST_BUSYBOX").unwrap_or("/usr/bin/busybox".into());
    std::fs::copy(busybox, context.join("busybox")).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        r#"FROM scratch
COPY busybox /bin/busybox
COPY note /note
RUN ["/bin/busybox", "sh", "-c", "/bin/busybox head -c 32 /dev/urandom > /cache-proof"]
"#,
    )
    .unwrap();
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
    build(
        &sb,
        &context,
        &[
            "--output",
            &format!("type=local,dest={}", second.display()),
            "--cache-from",
            &format!("type=local,src={}", cache.display()),
        ],
    );
    // Compare the RUN result. A remote cache hit may log downloads instead of CACHED.
    let cached = read(&first.join("cache-proof"));
    assert_eq!(cached.len(), 32);
    assert_eq!(read(&second.join("cache-proof")), cached);
    assert_eq!(
        read(&second.join("note")),
        b"from an actual BuildKit worker\n"
    );
    clean(&sb);
    println!("PASS: BuildKit multi-platform OCI archive, local exporter, cache export and import");
}

#[test]
fn the_image_export_does_not_copy_its_active_build_workspace() {
    if !enabled() {
        return;
    }
    let mut sb = Sandbox::new();
    let context = sb.tmp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    let data = context.join("podbag [data]");
    sb.env
        .push(("PODBAG_DATA_DIR".into(), data.to_str().unwrap().into()));
    std::fs::write(context.join("Dockerfile"), "FROM scratch\nCOPY . /\n").unwrap();
    std::fs::write(context.join("note"), "from the context").unwrap();
    std::fs::write(context.join(".dockerignore"), "!**\n").unwrap();
    build(&sb, &context, &["-t", "build-context:private"]);
    let image = checked(
        sb.podbag()
            .args(["save", "build-context:private"])
            .output()
            .unwrap(),
    );
    let mut found_note = false;
    for layer in decoded_docker_layers(&image.stdout) {
        for entry in tar::Archive::new(layer.as_slice()).entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap();
            assert!(
                !path.to_string_lossy().contains("/builds/buildkit-"),
                "the image contains private BuildKit files at {path:?}"
            );
            found_note |= path == Path::new("note");
        }
    }
    assert!(found_note, "the image includes the ordinary context file");
}

const CANCELLATION_DOCKERFILE: &str = r#"FROM scratch
COPY busybox /bin/busybox
RUN ["/bin/busybox", "sh", "-c", "echo podbag-cancel-ready; /bin/busybox sleep 90"]
"#;

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
    std::fs::write(context.join("Dockerfile"), CANCELLATION_DOCKERFILE).unwrap();
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
        .lines()
        .any(|line| line.ends_with(" podbag-cancel-ready"))
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
    // The daemon's shutdown grace period starts after buildctl has exited.
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
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

#[test]
fn terminating_the_service_cancels_an_active_http_build() {
    use common::registry::{TarEntry, layer};
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    let socket = sb.tmp.path().join("docker.sock");
    let mut service = Agent(
        sb.podbag()
            .args([
                "system",
                "service",
                "--buildkit",
                "--timeout",
                "0",
                "--socket",
            ])
            .arg(&socket)
            .spawn()
            .unwrap(),
    );
    assert!(wait_for(Duration::from_secs(10), || curl(
        &socket,
        &["/_ping"]
    )
    .0 == "OK"));

    let busybox = std::env::var_os("PODBAG_TEST_BUSYBOX").unwrap_or("/usr/bin/busybox".into());
    let busybox = std::fs::read(busybox).unwrap();
    let archive = sb.tmp.path().join("context.tar");
    std::fs::write(
        &archive,
        layer(&[
            TarEntry::file("Dockerfile", CANCELLATION_DOCKERFILE.as_bytes()),
            TarEntry::File {
                path: "busybox".into(),
                data: busybox,
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
        ]),
    )
    .unwrap();
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut curl_cmd = if root_mode() {
        let mut cmd = Command::new("sudo");
        cmd.args(["-n", "curl"]);
        cmd
    } else {
        Command::new("curl")
    };
    let _request = Agent(
        curl_cmd
            .args(["--silent", "--no-buffer", "--unix-socket"])
            .arg(&socket)
            .arg("--data-binary")
            .arg(format!("@{}", archive.display()))
            .arg("http://localhost/build?version=2&networkmode=none")
            .stdout(log.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_for(Duration::from_secs(30), || {
            std::fs::read_to_string(log.path())
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|entry| {
                    entry["stream"]
                        .as_str()
                        .is_some_and(|line| line.trim_end().ends_with(" podbag-cancel-ready"))
                })
        }),
        "HTTP build did not start: {}",
        std::fs::read_to_string(log.path()).unwrap()
    );
    assert_eq!(workers(&sb).len(), 1);

    // SAFETY: this is the service child we started and have not yet reaped.
    assert_eq!(
        unsafe { libc::kill(service.0.id() as i32, libc::SIGTERM) },
        0
    );
    assert!(
        wait_for(Duration::from_secs(30), || {
            service
                .0
                .try_wait()
                .unwrap()
                .is_some_and(|status| status.success())
        }),
        "the service did not stop cleanly"
    );
    clean(&sb);
    assert!(!std::fs::read_to_string(log.path()).unwrap().contains("aux"));
}

#[test]
fn engine_v2_requires_an_opt_in_service() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let (response, headers) = curl(&service.socket, &["--request", "POST", "/build?version=2"]);
    assert!(headers.starts_with("HTTP/1.1 501 "), "{headers} {response}");
    assert!(response.contains("system service --buildkit"), "{response}");
}

fn v2_build(service: &Service, context: &[u8], query: &str, headers: &[&str]) -> String {
    use std::io::Write;
    let mut archive = tempfile::NamedTempFile::new().unwrap();
    archive.write_all(context).unwrap();
    let mut args = vec![
        "--max-time",
        "25",
        "--request",
        "POST",
        "--header",
        "Content-Type: application/x-tar",
    ];
    let body = format!("@{}", archive.path().display());
    args.extend(["--data-binary", &body]);
    args.extend_from_slice(headers);
    let url = format!("/build?version=2&{query}");
    args.push(&url);
    let (body, headers) = curl(&service.socket, &args);
    assert!(headers.starts_with("HTTP/1.1 200 "), "{headers} {body}");
    body
}

fn build_message(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {body}")))
        .collect()
}

#[test]
fn engine_v2_loads_a_tar_build_and_reports_progress_and_errors() {
    use common::registry::{TarEntry, gzip, layer};
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--buildkit", "--timeout", "0"]);
    let (_, ping) = curl(&service.socket, &["/_ping"]);
    assert!(ping.contains("Builder-Version: 1"), "{ping}");
    let (body, headers) = curl(
        &service.socket,
        &[
            "--request",
            "POST",
            "/build?version=2&outputs=%5B%7B%22type%22%3A%22local%22%7D%5D",
        ],
    );
    assert!(headers.starts_with("HTTP/1.1 501 "), "{headers} {body}");
    assert!(body.contains("custom build outputs"), "{body}");

    let archive = gzip(&layer(&[
        TarEntry::file(
            "Containerfile",
            b"FROM scratch AS actual\nARG LABEL_VALUE\nLABEL hello=$LABEL_VALUE\nCOPY note /note\nFROM scratch AS other\n",
        ),
        TarEntry::file("note", b"Docker Engine v2 context\n"),
    ]));
    let native_arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("no BuildKit fixture for {other}"),
    };
    let query = format!(
        "t=managed-v2%3Afirst&t=managed-v2%3Asecond&dockerfile=Containerfile&target=actual&buildargs=%7B%22LABEL_VALUE%22%3A%22engine%22%7D&networkmode=none&platform={native_arch}"
    );
    let body = v2_build(&service, &archive, &query, &[]);
    let messages = build_message(&body);
    assert!(
        messages.iter().any(|item| item.get("stream").is_some()),
        "{body}"
    );
    assert!(
        messages.iter().all(|item| item.get("error").is_none()),
        "{body}"
    );
    let id = messages.last().unwrap()["aux"]["ID"].as_str().unwrap();
    assert!(id.starts_with("sha256:"), "{body}");
    let inspected = checked(docker_out(
        &sb,
        &service,
        &[
            "image",
            "inspect",
            "--format",
            r#"{{ .Id }} {{ index .Config.Labels "hello" }}"#,
            "managed-v2:second",
        ],
    ));
    assert_eq!(stdout(&inspected), format!("{id} engine"));

    let archive = layer(&[TarEntry::file(
        "Dockerfile",
        b"FROM scratch\nCOPY missing /file\n",
    )]);
    let body = v2_build(&service, &archive, "t=managed-v2%3Ainvalid&q=true", &[]);
    let messages = build_message(&body);
    assert_eq!(messages.len(), 1, "{body}");
    assert!(
        messages[0]["error"].as_str().unwrap().contains("missing"),
        "{body}"
    );
    let body = v2_build(&service, &archive, "t=managed-v2%3Ainvalid", &[]);
    let messages = build_message(&body);
    assert!(messages.last().unwrap()["error"].is_string(), "{body}");
    let final_progress = messages
        .iter()
        .filter_map(|message| message["stream"].as_str())
        .next_back()
        .expect("buildctl reports the failed instruction");
    assert!(
        final_progress.contains("failed to solve") && final_progress.contains("missing"),
        "{body}"
    );
    clean(&sb);
    println!("PASS: Docker Engine HTTP v2 tar build, tags, streamed progress, and errors");
}

#[test]
fn engine_v2_uses_the_requests_registry_credentials() {
    use common::registry::{Auth, FakeRegistry, TarEntry, TestImage, base64, layer};
    if !enabled() || !root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--buildkit", "--timeout", "0"]);
    let secret = "engine-build-fixture-token";
    let registry = FakeRegistry::start_with(Auth::basic("builder", secret));
    registry.push(
        "protected/base",
        "test",
        &TestImage {
            layers: vec![layer(&[TarEntry::file(
                "private-note",
                b"access granted\n",
            )])],
            config: serde_json::json!({}),
        },
    );
    let dockerfile = format!(
        "FROM {}/protected/base:test\nLABEL private=verified\n",
        registry.host
    );
    let archive = layer(&[TarEntry::file("Dockerfile", dockerfile.as_bytes())]);

    let body = v2_build(&service, &archive, "pull=true&q=true", &[]);
    let messages = build_message(&body);
    assert!(messages.last().unwrap().get("error").is_some(), "{body}");

    let credentials =
        serde_json::json!({registry.host: {"username": "builder", "password": secret}});
    let header = format!(
        "X-Registry-Config: {}",
        base64(credentials.to_string().as_bytes())
    );
    let body = v2_build(
        &service,
        &archive,
        "pull=true&q=true&t=managed-v2%3Aprivate",
        &["--header", &header],
    );
    assert!(!body.contains(secret), "{body}");
    let messages = build_message(&body);
    assert_eq!(messages.len(), 1, "{body}");
    assert!(messages[0]["aux"]["ID"].is_string(), "{body}");
    let inspected = checked(docker_out(
        &sb,
        &service,
        &[
            "image",
            "inspect",
            "--format",
            r#"{{ index .Config.Labels "private" }}"#,
            "managed-v2:private",
        ],
    ));
    assert_eq!(stdout(&inspected), "verified");
    clean(&sb);
    println!("PASS: Docker Engine HTTP v2 uses only the supplied registry credentials");
}

fn buildx_sandbox() -> Option<Sandbox> {
    let Some(buildx) = std::env::var_os("PODBAG_TEST_BUILDX") else {
        eprintln!("PODBAG_TEST_BUILDX is not set: skipped");
        return None;
    };
    let sb = Sandbox::new();
    let plugins = sb.tmp.path().join("docker-config/cli-plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::os::unix::fs::symlink(
        std::fs::canonicalize(buildx).unwrap(),
        plugins.join("docker-buildx"),
    )
    .unwrap();
    Some(sb)
}

/// Set PODBAG_TEST_BUILDX to an executable Docker Buildx plugin to enable this test.
#[test]
fn engine_driver_transfers_context_secret_and_image_and_reaps_its_worker() {
    let Some(sb) = buildx_sandbox() else {
        return;
    };

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
            "--progress",
            "plain",
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
    let progress = stderr(&output);
    assert!(progress.contains(" using docker driver"), "{progress}");
    assert!(!stdout(&output).contains(marker) && !progress.contains(marker));
    let layers = decoded_docker_layers(&read(&archive));
    let contains = |needle: &[u8]| {
        layers
            .iter()
            .any(|layer| layer.windows(needle.len()).any(|bytes| bytes == needle))
    };
    assert!(
        contains(b"engine build\n"),
        "the exported note must be present"
    );
    assert!(
        !contains(marker.as_bytes()),
        "the secret appeared in a decoded image layer"
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
fn engine_docker_driver_loads_implicit_and_explicit_images() {
    if !enabled() {
        return;
    }
    let Some(sb) = buildx_sandbox() else {
        return;
    };

    let context = sb.tmp.path().join("context");
    std::fs::create_dir(&context).unwrap();
    let busybox = std::env::var_os("PODBAG_TEST_BUSYBOX").unwrap_or("/usr/bin/busybox".into());
    std::fs::copy(busybox, context.join("busybox")).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        "FROM scratch\nCOPY busybox /bin/busybox\nCOPY message /message\nCMD [\"/bin/busybox\", \"cat\", \"/message\"]\n",
    )
    .unwrap();
    let service = Service::start(&sb, &["--buildkit", "--timeout", "0"]);
    for (tag, flags) in [
        ("engine-kit:implicit", &[][..]),
        (
            "engine-kit:load",
            &["--load", "--tag", "engine-kit:alias"][..],
        ),
    ] {
        std::fs::write(context.join("message"), format!("{tag}\n")).unwrap();
        let mut args = vec![
            "buildx",
            "build",
            "--builder",
            "default",
            "--progress",
            "plain",
            "--network",
            "none",
            "--tag",
            tag,
        ];
        args.extend_from_slice(flags);
        args.push(context.to_str().unwrap());
        let build = checked(docker_out(&sb, &service, &args));
        assert!(
            stderr(&build).contains(" using docker driver"),
            "{}",
            stderr(&build)
        );
        let inspected = checked(docker_out(
            &sb,
            &service,
            &["image", "inspect", "--format", "{{.Id}}", tag],
        ));
        assert!(stdout(&inspected).starts_with("sha256:"));
        let ran = checked(
            sb.podbag()
                .args(["run", "--rm", "--network", "none", tag])
                .output()
                .unwrap(),
        );
        assert_eq!(stdout(&ran), tag);
    }
    let ran = checked(
        sb.podbag()
            .args(["run", "--rm", "--network", "none", "engine-kit:alias"])
            .output()
            .unwrap(),
    );
    assert_eq!(stdout(&ran), "engine-kit:load");
    let iid = sb.tmp.path().join("image-id");
    std::fs::write(context.join("message"), "untagged image\n").unwrap();
    checked(docker_out(
        &sb,
        &service,
        &[
            "buildx",
            "build",
            "--builder",
            "default",
            "--network",
            "none",
            "--iidfile",
            iid.to_str().unwrap(),
            context.to_str().unwrap(),
        ],
    ));
    let id = std::fs::read_to_string(iid).unwrap();
    assert!(id.starts_with("sha256:"));
    let ran = checked(
        sb.podbag()
            .args(["run", "--rm", "--network", "none", &id])
            .output()
            .unwrap(),
    );
    assert_eq!(stdout(&ran), "untagged image");

    std::fs::write(
        context.join("Dockerfile"),
        "FROM scratch\nCOPY missing /missing\n",
    )
    .unwrap();
    let failure = docker_out(
        &sb,
        &service,
        &[
            "buildx",
            "build",
            "--builder",
            "default",
            "--network",
            "none",
            "--load",
            "--tag",
            "engine-kit:load",
            context.to_str().unwrap(),
        ],
    );
    assert!(!failure.status.success());
    assert!(stderr(&failure).contains("missing"), "{}", stderr(&failure));
    let ran = checked(
        sb.podbag()
            .args(["run", "--rm", "--network", "none", "engine-kit:load"])
            .output()
            .unwrap(),
    );
    assert_eq!(stdout(&ran), "engine-kit:load");
    println!("PASS: stock Docker Buildx default output and --load are visible to inspect and run");
}

#[test]
fn engine_docker_driver_loads_registry_base_layers() {
    use common::registry::{FakeRegistry, TarEntry, TestImage, layer};
    if !enabled() || !root_mode() {
        return;
    }
    let Some(sb) = buildx_sandbox() else {
        return;
    };
    let registry = FakeRegistry::start(false);
    let busybox = std::env::var_os("PODBAG_TEST_BUSYBOX").unwrap_or("/usr/bin/busybox".into());
    registry.push(
        "fixtures/busybox",
        "v1",
        &TestImage {
            layers: vec![layer(&[
                TarEntry::File {
                    path: "busybox".into(),
                    data: std::fs::read(busybox).unwrap(),
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                },
                TarEntry::file("message", b"registry base layer\n"),
            ])],
            config: serde_json::json!({ "Cmd": ["/busybox", "cat", "/message"] }),
        },
    );
    std::fs::write(
        sb.tmp.path().join("Dockerfile"),
        format!(
            "FROM {}/fixtures/busybox:v1\nLABEL test=registry-layer\n",
            registry.host,
        ),
    )
    .unwrap();
    let service = Service::start(&sb, &["--buildkit", "--timeout", "0"]);
    checked(docker_out(
        &sb,
        &service,
        &[
            "buildx",
            "build",
            "--builder",
            "default",
            "--network",
            "none",
            "--load",
            "--tag",
            "engine-kit:base",
            sb.tmp.path().to_str().unwrap(),
        ],
    ));
    let ran = checked(
        sb.podbag()
            .args(["run", "--rm", "--network", "none", "engine-kit:base"])
            .output()
            .unwrap(),
    );
    assert_eq!(stdout(&ran), "registry base layer");
    println!("PASS: stock Buildx --load includes an unused base layer from a fixture registry");
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
