#![cfg(target_os = "linux")]
//! Registries through the Docker Engine API: `docker login` (POST /auth), private pulls
//! and pushes with `X-Registry-Auth`, and `docker search`, against the fake registry.

mod common;
use common::api::*;
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

fn docker_login(sb: &Sandbox, service: &Service, host: &str, password: &str) -> Output {
    let mut child = docker(sb, service, &["login", host, "-u", "u", "--password-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(password.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn docker_login_pull_push_and_pull_back() {
    for auth in [Auth::basic("u", "pw"), Auth::token("u", "pw")] {
        let reg = FakeRegistry::start_with(auth.clone());
        reg.push("team/app", "v1", &image());
        let sb = Sandbox::new();
        let service = Service::start(&sb, &["--timeout", "0"]);
        let name = format!("{}/team/app:v1", reg.host);

        let out = docker_out(&sb, &service, &["pull", &name]);
        assert_ne!(out.status.code(), Some(0), "{auth:?}");
        assert!(stderr(&out).contains("login"), "{}", stderr(&out));

        let out = docker_login(&sb, &service, &reg.host, "wrong");
        assert_ne!(out.status.code(), Some(0), "{auth:?}: {}", stdout(&out));
        assert!(stderr(&out).contains("failed"), "{}", stderr(&out));
        let out = docker_login(&sb, &service, &reg.host, "pw");
        assert!(ok(&out).contains("Login Succeeded"), "{auth:?}");

        let pulled = ok(&docker_out(&sb, &service, &["pull", &name]));
        assert!(pulled.contains("Downloaded newer image"), "{pulled}");

        let copy = format!("{}/team/copy:v2", reg.host);
        ok(&docker_out(&sb, &service, &["tag", &name, &copy]));
        let pushed = ok(&docker_out(&sb, &service, &["push", &copy]));
        assert!(
            pushed.contains(&format!(
                "The push refers to repository [{}/team/copy]",
                reg.host
            )),
            "{pushed}"
        );
        assert!(pushed.contains("Mounted from team/app"), "{pushed}");
        assert!(pushed.contains("v2: digest: sha256:"), "{pushed}");
        let again = ok(&docker_out(&sb, &service, &["push", &copy]));
        assert!(again.contains("Layer already exists"), "{again}");

        ok(&docker_out(&sb, &service, &["rmi", &name, &copy]));
        let back = ok(&docker_out(&sb, &service, &["pull", &copy]));
        assert!(back.contains("Downloaded newer image"), "{back}");
        let ran = ok(&docker_out(&sb, &service, &["run", "--rm", &copy]));
        assert_eq!(ran, "hi");

        ok(&docker_out(&sb, &service, &["logout", &reg.host]));
        ok(&docker_out(&sb, &service, &["rmi", &copy]));
        let out = docker_out(&sb, &service, &["pull", &copy]);
        assert_ne!(out.status.code(), Some(0));
    }
}

#[test]
fn auth_answers_like_docker() {
    let reg = FakeRegistry::start_with(Auth::token("u", "pw"));
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let body = format!(
        r#"{{"username":"u","password":"pw","serveraddress":"{}"}}"#,
        reg.host
    );
    let (reply, head) = curl(
        &service.socket,
        &[
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            "/auth",
        ],
    );
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["Status"], "Login Succeeded");
    assert_eq!(reply["IdentityToken"], common::registry::REFRESH_TOKEN);
    let body = format!(
        r#"{{"username":"u","password":"no","serveraddress":"{}"}}"#,
        reg.host
    );
    let (_, head) = curl(
        &service.socket,
        &[
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            "/auth",
        ],
    );
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
}

#[test]
fn docker_search_lists_a_registry_index() {
    let reg = FakeRegistry::start(false);
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let found = ok(&docker_out(
        &sb,
        &service,
        &[
            "search",
            "--format",
            "{{.Name}}:{{.StarCount}}:{{.IsOfficial}}",
            &format!("{}/web", reg.host),
        ],
    ));
    assert_eq!(found, "web-0:10:[OK]\nweb-1:9:\nweb-2:8:");
    let filtered = ok(&docker_out(
        &sb,
        &service,
        &[
            "search",
            "--filter",
            "is-official=true",
            "--format",
            "{{.Name}}",
            &format!("{}/web", reg.host),
        ],
    ));
    assert_eq!(filtered, "web-0");
}
