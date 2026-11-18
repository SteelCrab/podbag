#![cfg(target_os = "linux")]
//! Images through the Docker Engine API: pull, list, inspect, tag, remove.

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TarEntry, TestImage, alpine_layer, layer};
use common::*;

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn push(reg: &FakeRegistry, repo: &str) -> String {
    reg.push(
        repo,
        "v1",
        &TestImage {
            layers: vec![alpine_layer(), layer(&[TarEntry::file("hello", b"hi\n")])],
            config: serde_json::json!({"Env": [PATH], "Cmd": ["cat", "/hello"]}),
        },
    );
    format!("{}/{repo}:v1", reg.host)
}

fn ok(out: &std::process::Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

#[test]
fn docker_pull_images_inspect_rmi() {
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "x");
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let pulled = ok(&docker_out(&sb, &service, &["pull", &image]));
    assert!(pulled.contains("Pull complete"), "{pulled}");
    assert!(pulled.contains("Downloaded newer image"), "{pulled}");
    let again = ok(&docker_out(&sb, &service, &["pull", &image]));
    assert!(again.contains("Image is up to date"), "{again}");
    let images = ok(&docker_out(
        &sb,
        &service,
        &["images", "--format", "{{.Repository}} {{.Tag}}"],
    ));
    assert!(
        images.lines().any(|l| l == format!("{}/x v1", reg.host)),
        "{images}"
    );
    let cmd = ok(&docker_out(
        &sb,
        &service,
        &[
            "image",
            "inspect",
            "--format",
            "{{json .Config.Cmd}}",
            &image,
        ],
    ));
    assert_eq!(cmd, r#"["cat","/hello"]"#);
    ok(&docker_out(&sb, &service, &["rmi", &image]));
    let images = ok(&docker_out(&sb, &service, &["images", "-q"]));
    assert_eq!(images, "", "{images}");
}

#[test]
fn docker_tag_adds_a_name() {
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "t");
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    ok(&docker_out(&sb, &service, &["pull", &image]));
    ok(&docker_out(&sb, &service, &["tag", &image, "mine/copy:2"]));
    let images = ok(&docker_out(
        &sb,
        &service,
        &["images", "--format", "{{.Repository}}:{{.Tag}}"],
    ));
    assert!(images.lines().any(|l| l == "mine/copy:2"), "{images}");
    // Removing one name keeps the image under the other.
    ok(&docker_out(&sb, &service, &["rmi", "mine/copy:2"]));
    ok(&docker_out(&sb, &service, &["image", "inspect", &image]));
}

#[test]
fn docker_rmi_of_an_image_in_use_is_a_conflict() {
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "u");
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    ok(&docker_out(&sb, &service, &["pull", &image]));
    // A container made by the CLI uses it.
    ok(&sb
        .openpod()
        .args(["create", "--name", "user", &image])
        .output()
        .unwrap());
    let out = docker_out(&sb, &service, &["rmi", &image]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("uses it"), "{}", stderr(&out));
}

#[test]
fn pull_of_a_missing_image_reports_the_error() {
    let reg = FakeRegistry::start(false);
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let out = docker_out(
        &sb,
        &service,
        &["pull", &format!("{}/nothing:v1", reg.host)],
    );
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("nothing"), "{}", stderr(&out));
}
