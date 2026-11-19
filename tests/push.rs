#![cfg(unix)]
//! Pushing images of a store to a registry: mounts, uploads, and a second push that sends
//! nothing.
mod common;
use common::registry::{Auth, FakeRegistry, TarEntry, TestImage, layer};
use podbag::image::PushEvent;
use podbag::image::auth::AuthConfig;
use podbag::image::registry::Registry;
use podbag::store::Store;
use std::collections::HashMap;

fn base_image() -> TestImage {
    TestImage {
        layers: vec![
            layer(&[
                TarEntry::Dir {
                    path: "etc".into(),
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                },
                TarEntry::file("etc/base", b"base layer"),
            ]),
            layer(&[TarEntry::file("app.txt", b"the app")]),
        ],
        config: serde_json::json!({"Cmd": ["cat", "/app.txt"], "Env": ["A=1"]}),
    }
}

fn store(tmp: &tempfile::TempDir, name: &str) -> Store {
    Store::open_at(tmp.path().join(name), false, false).unwrap()
}

fn login() -> Registry {
    Registry::with_auth(Some(AuthConfig::with_password("u", "pw")))
}

fn pull(s: &Store, name: &str, registry: &mut Registry) {
    let none = HashMap::new;
    s.pull(name, registry, u64::MAX, &none, &mut |_| {})
        .unwrap();
}

fn push(s: &Store, name: &str, registry: &mut Registry) -> Vec<PushEvent> {
    let mut events = Vec::new();
    s.push(name, registry, &mut |e| events.push(e)).unwrap();
    events
}

/// Requests that send data: uploads and mounts (not HEAD checks or the manifest).
fn sends(reg: &FakeRegistry) -> Vec<String> {
    reg.requests()
        .into_iter()
        .filter(|r| r.contains("/blobs/uploads/") && !r.starts_with("DELETE"))
        .collect()
}

#[test]
fn a_pulled_image_goes_back_by_mounts_and_twice_sends_nothing() {
    let reg = FakeRegistry::start_with(Auth::token("u", "pw"));
    reg.push("library/base", "1", &base_image());
    let tmp = tempfile::tempdir().unwrap();
    let s = store(&tmp, "store");
    let mut registry = login();
    let base = format!("{}/library/base:1", reg.host);
    pull(&s, &base, &mut registry);
    let target = format!("{}/team/app:v1", reg.host);
    s.tag(&base, &target, &HashMap::new).unwrap();

    let events = push(&s, &target, &mut login());
    let mounted = events
        .iter()
        .filter(|e| matches!(e, PushEvent::LayerMounted { from, .. } if from == "library/base"))
        .count();
    assert_eq!(mounted, 2, "{events:?}");
    let Some(PushEvent::Done { tag, digest, .. }) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(tag, "v1");
    let (content_type, body) = reg.manifest("team/app", "v1").unwrap();
    // It came as OCI, and goes as OCI.
    assert_eq!(content_type, "application/vnd.oci.image.manifest.v1+json");
    assert_eq!(&common::registry::sha256(&body), digest);
    // The original layers: the same digests as the base image's.
    let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let layers: Vec<&str> = manifest["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["digest"].as_str().unwrap())
        .collect();
    assert_eq!(layers, FakeRegistry::layer_digests(&base_image()));

    reg.clear_requests();
    let again = push(&s, &target, &mut login());
    assert!(sends(&reg).is_empty(), "{:?}", reg.requests());
    let exists = again
        .iter()
        .filter(|e| matches!(e, PushEvent::LayerExists { .. }))
        .count();
    assert_eq!(exists, 2, "{again:?}");
}

#[test]
fn another_registry_gets_written_layers_once_and_they_pull_back() {
    let source = FakeRegistry::start(false);
    source.push("base", "1", &base_image());
    let target = FakeRegistry::start_with(Auth::basic("u", "pw"));
    let tmp = tempfile::tempdir().unwrap();
    let s = store(&tmp, "store");
    let base = format!("{}/base:1", source.host);
    pull(&s, &base, &mut Registry::new());
    let name = format!("{}/copy:1", target.host);
    s.tag(&base, &name, &HashMap::new).unwrap();

    // Without a login, the registry refuses, and says how to log in.
    let e = s
        .push(&name, &mut Registry::new(), &mut |_| {})
        .unwrap_err();
    assert!(e.next.contains("podbag login"), "{e}");

    let events = push(&s, &name, &mut login());
    let pushed = events
        .iter()
        .filter(|e| matches!(e, PushEvent::LayerPushed { .. }))
        .count();
    assert_eq!(pushed, 2, "{events:?}");
    let first = target.manifest("copy", "1").unwrap();

    // Pushed again: nothing is sent, and the manifest is the same.
    target.clear_requests();
    let again = push(&s, &name, &mut login());
    assert!(sends(&target).is_empty(), "{:?}", target.requests());
    assert!(
        again
            .iter()
            .all(|e| !matches!(e, PushEvent::LayerPushed { .. })),
        "{again:?}"
    );
    assert_eq!(target.manifest("copy", "1").unwrap(), first);

    // The same layers under another name of the same registry: mounted, not sent.
    let other = format!("{}/other:2", target.host);
    s.tag(&base, &other, &HashMap::new).unwrap();
    target.clear_requests();
    let events = push(&s, &other, &mut login());
    let mounted = events
        .iter()
        .filter(|e| matches!(e, PushEvent::LayerMounted { from, .. } if from == "copy"))
        .count();
    assert_eq!(mounted, 2, "{events:?}");

    // A fresh store pulls it back with the same files and settings.
    let fresh = store(&tmp, "fresh");
    pull(&fresh, &name, &mut login());
    let (_, record) = fresh.find(&name).unwrap();
    assert_eq!(record.config.cmd, ["cat", "/app.txt"]);
    assert_eq!(record.config.env, [("A".to_string(), "1".to_string())]);
    let root = fresh.rootfs_path(&record.digest);
    assert_eq!(std::fs::read(root.join("app.txt")).unwrap(), b"the app");
    assert_eq!(std::fs::read(root.join("etc/base")).unwrap(), b"base layer");
}

#[test]
fn a_name_with_a_digest_or_an_unknown_image_is_refused() {
    let reg = FakeRegistry::start(false);
    let tmp = tempfile::tempdir().unwrap();
    let s = store(&tmp, "store");
    let digest = format!("sha256:{}", "a".repeat(64));
    let e = s
        .push(
            &format!("{}/x@{digest}", reg.host),
            &mut Registry::new(),
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(e.kind, podbag::ErrorKind::InvalidInput);
    let e = s
        .push(
            &format!("{}/none:1", reg.host),
            &mut Registry::new(),
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(e.kind, podbag::ErrorKind::NotFound);
    assert!(reg.requests().is_empty());
}
