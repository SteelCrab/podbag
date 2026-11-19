#![cfg(target_os = "linux")]
//! A real registry: `registry:2` from Docker Hub, run by podbag, with htpasswd logins.
//! podbag logs in, pushes an image, removes it, pulls it back and runs it; then the
//! docker CLI does the same through `podbag system service`, with the same Docker
//! config. Pulls from Docker Hub: runs only with PODBAG_TEST_NETWORK=1.

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TarEntry, TestImage, alpine_layer, layer};
use common::*;
use std::io::{Read, Write};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// A test login, and its bcrypt line for htpasswd.
const USER: &str = "podbag-test";
const PASSWORD: &str = "podbag-test-password";
const HTPASSWD: &str = "podbag-test:$2y$05$58VHnGQQYv65LTDHMsnXrOj/QmJ.qpYMvZ6CBPXQfDeaFHkht40qe\n";

fn enabled() -> bool {
    let on = std::env::var("PODBAG_TEST_NETWORK").as_deref() == Ok("1");
    if !on {
        eprintln!("PODBAG_TEST_NETWORK is not 1: skipped");
    }
    on
}

fn ok(out: &Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
    stdout(out)
}

fn with_stdin(mut cmd: std::process::Command, input: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The HTTP status of `GET /v2/` on the registry, if it answers.
fn ping(port: u16) -> Option<String> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET /v2/ HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    let mut text = String::new();
    let _ = s.read_to_string(&mut text);
    text.lines().next().map(String::from)
}

#[test]
fn push_and_pull_back_through_a_real_registry() {
    if !enabled() {
        return;
    }
    let _turn = network_turn();
    let mut sb = Sandbox::new();
    // podbag and the docker CLI (see common::api::docker) share this config.
    let config = sb.tmp.path().join("docker-config");
    sb.env
        .push(("DOCKER_CONFIG".to_string(), config.display().to_string()));
    let podbag = |args: &[&str]| sb.podbag().args(args).output().unwrap();

    // The image to push, from the fake registry.
    let fake = FakeRegistry::start(false);
    fake.push(
        "app",
        "1",
        &TestImage {
            layers: vec![alpine_layer(), layer(&[TarEntry::file("hello", b"hi\n")])],
            config: serde_json::json!({"Env": [PATH], "Cmd": ["cat", "/hello"]}),
        },
    );
    let source = format!("{}/app:1", fake.host);
    ok(&podbag(&["pull", &source]));

    // registry:2 with a login.
    let auth = sb.tmp.path().join("auth");
    std::fs::create_dir(&auth).unwrap();
    std::fs::write(auth.join("htpasswd"), HTPASSWD).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(sb.tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let port = free_port();
    ok(&podbag(&[
        "run",
        "-d",
        "--name",
        "registry",
        "-p",
        &format!("127.0.0.1:{port}:5000"),
        "-v",
        &format!("{}:/auth:ro", auth.display()),
        "-e",
        "REGISTRY_AUTH=htpasswd",
        "-e",
        "REGISTRY_AUTH_HTPASSWD_REALM=podbag",
        "-e",
        "REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd",
        "registry:2",
    ]));
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ping(port).is_some_and(|l| l.contains(" 401")) {
        assert!(Instant::now() < deadline, "the registry did not start");
        std::thread::sleep(Duration::from_millis(200));
    }
    let host = format!("127.0.0.1:{port}");

    // podbag.
    let name = format!("{host}/test/app:1");
    let out = podbag(&["push", &name]);
    assert_ne!(out.status.code(), Some(0));
    let mut login = sb.podbag();
    login.args(["login", &host, "-u", USER, "--password-stdin"]);
    let out = with_stdin(login, "wrong\n");
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("login to"), "{}", stderr(&out));
    let mut login = sb.podbag();
    login.args(["login", &host, "-u", USER, "--password-stdin"]);
    assert_eq!(ok(&with_stdin(login, PASSWORD)), "Login Succeeded");
    ok(&podbag(&["tag", &source, &name]));
    let out = podbag(&["push", &name]);
    assert!(ok(&out).starts_with("1: digest: sha256:"));
    assert_eq!(
        stderr(&out).matches(": Pushed").count(),
        2,
        "{}",
        stderr(&out)
    );
    let out = podbag(&["push", &name]);
    ok(&out);
    assert_eq!(
        stderr(&out).matches("Layer already exists").count(),
        2,
        "{}",
        stderr(&out)
    );
    ok(&podbag(&["rmi", &name, &source]));
    ok(&podbag(&["pull", &name]));
    assert_eq!(ok(&podbag(&["run", "--rm", &name])), "hi");

    // The docker CLI, through the service, with the login podbag stored.
    let service = Service::start(&sb, &["--timeout", "0"]);
    let docker_name = format!("{host}/test/docker:2");
    ok(&docker_out(&sb, &service, &["tag", &name, &docker_name]));
    let pushed = ok(&docker_out(&sb, &service, &["push", &docker_name]));
    assert!(pushed.contains("2: digest: sha256:"), "{pushed}");
    assert!(pushed.contains("Mounted from test/app"), "{pushed}");
    // Logged out, and removed here: the pull must reach the registry, and log in.
    ok(&docker_out(&sb, &service, &["logout", &host]));
    ok(&docker_out(&sb, &service, &["rmi", &docker_name, &name]));
    let out = docker_out(&sb, &service, &["pull", &docker_name]);
    assert_ne!(out.status.code(), Some(0), "pulled without a login");
    let mut login = docker(
        &sb,
        &service,
        &["login", &host, "-u", USER, "--password-stdin"],
    );
    login.stdin(Stdio::piped());
    assert!(ok(&with_stdin(login, PASSWORD)).contains("Login Succeeded"));
    let pulled = ok(&docker_out(&sb, &service, &["pull", &docker_name]));
    assert!(pulled.contains("Downloaded newer image"), "{pulled}");
    assert_eq!(
        ok(&docker_out(&sb, &service, &["run", "--rm", &docker_name])),
        "hi"
    );
    ok(&podbag(&["rm", "-f", "registry"]));
}
