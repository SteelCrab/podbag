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
        .podbag()
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

#[test]
fn docker_save_then_load_round_trips() {
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "saved");
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let docker = |args: &[&str]| docker_out(&sb, &service, args);
    ok(&docker(&["pull", &image]));
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let file = dir.path().join("saved.tar").display().to_string();
    ok(&docker(&["save", "-o", &file, &image]));
    ok(&docker(&["rmi", &image]));
    assert_ne!(docker(&["image", "inspect", &image]).status.code(), Some(0));
    let loaded = ok(&docker(&["load", "-i", &file]));
    assert!(
        loaded.contains(&format!("Loaded image: {image}")),
        "{loaded}"
    );
    assert_eq!(ok(&docker(&["run", "--rm", &image])), "hi");
    let cmd = ok(&docker(&[
        "image",
        "inspect",
        "--format",
        "{{json .Config.Cmd}}",
        &image,
    ]));
    assert_eq!(cmd, r#"["cat","/hello"]"#);
    // Loading again keeps one image under the name.
    ok(&docker(&["load", "-i", &file]));
    let names = ok(&docker(&["images", "--format", "{{.Repository}}:{{.Tag}}"]));
    assert_eq!(names.lines().filter(|l| *l == image).count(), 1, "{names}");
    // A name that is not there: an error before any archive.
    let missing = docker(&["save", "-o", &file, "nothing:here"]);
    assert_ne!(missing.status.code(), Some(0));
}

#[test]
fn docker_load_of_an_archive_made_elsewhere() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let docker = |args: &[&str]| docker_out(&sb, &service, args);
    let layer = alpine_layer();
    let diff_id = format!("sha256:{}", sha256_hex(&layer));
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64", "os": "linux",
        "config": {"Env": [PATH], "Cmd": ["echo", "made", "elsewhere"]},
        "rootfs": {"type": "layers", "diff_ids": [diff_id]},
    }))
    .unwrap();
    let config_name = format!("{}.json", sha256_hex(&config));
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": config_name, "RepoTags": ["local/elsewhere:v1"], "Layers": ["l1/layer.tar"]}]))
    .unwrap();
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in [
        (config_name.as_str(), &config),
        ("l1/layer.tar", &layer),
        ("manifest.json", &manifest),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, name, data.as_slice()).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let file = dir.path().join("elsewhere.tar");
    std::fs::write(&file, b.into_inner().unwrap()).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let loaded = ok(&docker(&["load", "-i", &file.display().to_string()]));
    assert!(
        loaded.contains("Loaded image: local/elsewhere:v1"),
        "{loaded}"
    );
    assert_eq!(
        ok(&docker(&["run", "--rm", "local/elsewhere:v1"])),
        "made elsewhere"
    );
    // Not an archive.
    std::fs::write(&file, vec![b'x'; 4096]).unwrap();
    let junk = docker(&["load", "-i", &file.display().to_string()]);
    assert_ne!(junk.status.code(), Some(0));
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn a_load_without_a_body_length_is_answered() {
    // Neither Content-Length nor chunked: the handler reads an empty body. (It once
    // reached an unreachable branch: the connection's thread panicked, and a release
    // build, which aborts on panic, ended the whole service.)
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let (body, head) = curl(&service.socket, &["-X", "POST", "/images/load"]);
    assert!(head.starts_with("HTTP/1.1 400"), "{head}{body}");
    let (pong, _) = curl(&service.socket, &["/_ping"]);
    assert_eq!(pong, "OK");
}

#[test]
fn image_prune_removes_images_without_a_name() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let docker = |args: &[&str]| docker_out(&sb, &service, args);
    // An archive without names, as `docker save <id>` makes.
    let layer = alpine_layer();
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64", "os": "linux",
        "config": {"Env": [PATH], "Cmd": ["true"]},
        "rootfs": {"type": "layers", "diff_ids": [format!("sha256:{}", sha256_hex(&layer))]},
    }))
    .unwrap();
    let id = format!("sha256:{}", sha256_hex(&config));
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": format!("{}.json", sha256_hex(&config)), "RepoTags": null, "Layers": ["l/layer.tar"]}]))
    .unwrap();
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in [
        (format!("{}.json", sha256_hex(&config)), &config),
        ("l/layer.tar".to_string(), &layer),
        ("manifest.json".to_string(), &manifest),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, name, data.as_slice()).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let file = dir.path().join("unnamed.tar");
    std::fs::write(&file, b.into_inner().unwrap()).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let loaded = ok(&docker(&["load", "-i", &file.display().to_string()]));
    assert!(
        loaded.contains(&format!("Loaded image ID: {id}")),
        "{loaded}"
    );
    ok(&docker(&["image", "inspect", &id]));
    let pruned = ok(&docker(&["image", "prune", "-f"]));
    assert!(pruned.contains(&id), "{pruned}");
    assert_ne!(docker(&["image", "inspect", &id]).status.code(), Some(0));
}
