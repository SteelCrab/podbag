#![cfg(target_os = "linux")]
//! `podbag login`, `logout`, `tag`, `push`, `search` and private pulls, against the fake
//! registry. Logins go to the sandbox's own Docker config.

mod common;
use common::registry::{Auth, FakeRegistry, TarEntry, TestImage, alpine_layer, layer};
use common::*;
use std::io::Write;
use std::process::{Output, Stdio};

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn image() -> TestImage {
    TestImage {
        layers: vec![alpine_layer(), layer(&[TarEntry::file("hello", b"hi\n")])],
        config: serde_json::json!({"Env": [PATH], "Cmd": ["cat", "/hello"]}),
    }
}

/// A sandbox with a Docker config directory of its own.
fn sandbox() -> Sandbox {
    let mut sb = Sandbox::new();
    let config = sb.tmp.path().join("docker-config");
    sb.env
        .push(("DOCKER_CONFIG".to_string(), config.display().to_string()));
    sb
}

fn podbag(sb: &Sandbox, args: &[&str]) -> Output {
    sb.podbag().args(args).output().unwrap()
}

fn with_stdin(sb: &Sandbox, args: &[&str], input: &str) -> Output {
    let mut child = sb
        .podbag()
        .args(args)
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

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

#[test]
fn login_pull_push_and_pull_back_a_private_image() {
    let reg = FakeRegistry::start_with(Auth::token("u", "pw"));
    reg.push("team/app", "v1", &image());
    let sb = sandbox();
    let name = format!("{}/team/app:v1", reg.host);

    // Without a login: a clear error that says how to log in.
    let out = podbag(&sb, &["pull", &name]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains(&format!("podbag login {}", reg.host)),
        "{}",
        stderr(&out)
    );

    // A wrong password is refused, and nothing is stored.
    let out = with_stdin(
        &sb,
        &["login", &reg.host, "-u", "u", "--password-stdin"],
        "nope\n",
    );
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("login to"), "{}", stderr(&out));
    let out = with_stdin(
        &sb,
        &["login", &reg.host, "-u", "u", "--password-stdin"],
        "pw\n",
    );
    assert_eq!(ok(&out), "Login Succeeded");
    // The registry gave a refresh token: it is kept instead of the password.
    if !root_mode() {
        let config =
            std::fs::read_to_string(sb.tmp.path().join("docker-config/config.json")).unwrap();
        assert!(config.contains("identitytoken"), "{config}");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(sb.tmp.path().join("docker-config/config.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    ok(&podbag(&sb, &["pull", &name]));
    let copy = format!("{}/team/copy:v2", reg.host);
    ok(&podbag(&sb, &["tag", &name, &copy]));
    let out = podbag(&sb, &["push", &copy]);
    let printed = ok(&out);
    assert!(printed.starts_with("v2: digest: sha256:"), "{printed}");
    let progress = stderr(&out);
    assert!(
        progress.contains(&format!(
            "The push refers to repository [{}/team/copy]",
            reg.host
        )),
        "{progress}"
    );
    assert_eq!(
        progress.matches("Mounted from team/app").count(),
        2,
        "{progress}"
    );

    // Again: nothing is sent.
    reg.clear_requests();
    let out = podbag(&sb, &["push", &copy]);
    ok(&out);
    assert_eq!(
        stderr(&out).matches("Layer already exists").count(),
        2,
        "{}",
        stderr(&out)
    );
    assert!(
        !reg.requests().iter().any(|r| r.contains("/blobs/uploads/")),
        "{:?}",
        reg.requests()
    );

    // Removed here, pulled back, and run.
    ok(&podbag(&sb, &["rmi", &name, &copy]));
    ok(&podbag(&sb, &["pull", &copy]));
    let out = podbag(&sb, &["run", "--rm", &copy]);
    assert_eq!(ok(&out), "hi");

    let out = podbag(&sb, &["logout", &reg.host]);
    assert_eq!(
        ok(&out),
        format!("Removing login credentials for {}", reg.host)
    );
    let out = podbag(&sb, &["logout", &reg.host]);
    assert_eq!(ok(&out), format!("Not logged in to {}", reg.host));
    ok(&podbag(&sb, &["rmi", &copy]));
    let out = podbag(&sb, &["pull", &copy]);
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn a_basic_auth_registry_gets_written_layers() {
    let source = FakeRegistry::start(false);
    source.push("base", "1", &image());
    let target = FakeRegistry::start_with(Auth::basic("u", "pw"));
    let sb = sandbox();
    let base = format!("{}/base:1", source.host);
    ok(&podbag(&sb, &["pull", &base]));
    let name = format!("{}/mine/app:1", target.host);
    ok(&podbag(&sb, &["tag", &base, &name]));
    let out = podbag(&sb, &["push", &name]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("podbag login"), "{}", stderr(&out));

    let out = podbag(&sb, &["login", &target.host, "-u", "u", "-p", "pw"]);
    assert_eq!(ok(&out), "Login Succeeded");
    assert!(
        stderr(&out).contains("--password-stdin"),
        "{}",
        stderr(&out)
    );
    let out = podbag(&sb, &["push", &name]);
    ok(&out);
    assert_eq!(
        stderr(&out).matches(": Pushed").count(),
        2,
        "{}",
        stderr(&out)
    );
    let out = podbag(&sb, &["push", &name]);
    ok(&out);
    assert_eq!(
        stderr(&out).matches("Layer already exists").count(),
        2,
        "{}",
        stderr(&out)
    );
}

#[test]
fn search_lists_a_registry_index() {
    let reg = FakeRegistry::start(false);
    let sb = sandbox();
    let out = podbag(
        &sb,
        &["search", "--limit", "2", &format!("{}/web", reg.host)],
    );
    let table = ok(&out);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), 3, "{table}");
    assert!(lines[0].starts_with("NAME"), "{table}");
    assert!(
        lines[1].starts_with("web-0") && lines[1].contains("[OK]"),
        "{table}"
    );
}

#[test]
fn login_needs_a_user_and_a_password_without_a_terminal() {
    let sb = sandbox();
    let out = with_stdin(&sb, &["login", "127.0.0.1:1", "--password-stdin"], "x");
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("--username"), "{}", stderr(&out));
    let out = with_stdin(&sb, &["login", "127.0.0.1:1", "-u", "u"], "");
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("--password-stdin"),
        "{}",
        stderr(&out)
    );
}
