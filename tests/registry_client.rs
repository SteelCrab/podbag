#![cfg(unix)]
mod common;
use common::registry::{FakeRegistry, TarEntry, TestImage, layer, sha256};
use openpod::ErrorKind;
use openpod::image::manifest::Descriptor;
use openpod::image::reference::Reference;
use openpod::image::registry::Registry;
use std::io::Read;

fn small_image(text: &str) -> TestImage {
    TestImage {
        layers: vec![layer(&[TarEntry::file("hello.txt", text.as_bytes())])],
        config: serde_json::json!({"Cmd": ["cat", "/hello.txt"]}),
    }
}

fn reference(reg: &FakeRegistry, rest: &str) -> Reference {
    Reference::parse(&format!("{}/{rest}", reg.host)).unwrap()
}

#[test]
fn resolves_a_manifest_and_its_config() {
    let reg = FakeRegistry::start(false);
    let digest = reg.push("app", "v1", &small_image("hi"));
    let resolved = Registry::new().resolve(&reference(&reg, "app:v1")).unwrap();
    assert_eq!(resolved.manifest_digest, digest);
    assert_eq!(resolved.layers.len(), 1);
    assert_eq!(resolved.config.config.cmd, ["cat", "/hello.txt"]);
}

#[test]
fn resolves_through_an_index_for_this_platform() {
    let reg = FakeRegistry::start(false);
    let (ours, theirs) = (small_image("ours"), small_image("theirs"));
    let (arch, variant, other) = match std::env::consts::ARCH {
        "aarch64" => ("arm64", "v8", "amd64"),
        _ => ("amd64", "", "arm64"),
    };
    reg.push_index(
        "multi",
        "latest",
        &[(other, "", &theirs), (arch, variant, &ours)],
    );
    let resolved = Registry::new().resolve(&reference(&reg, "multi")).unwrap();
    assert_eq!(
        resolved.layers[0].digest,
        FakeRegistry::layer_digests(&ours)[0]
    );
}

#[test]
fn gets_a_token_when_asked() {
    let reg = FakeRegistry::start(true);
    reg.push("private", "latest", &small_image("hi"));
    Registry::new()
        .resolve(&reference(&reg, "private"))
        .unwrap();
    assert!(
        reg.requests().iter().any(|r| r == "GET /token"),
        "{:?}",
        reg.requests()
    );
}

#[test]
fn missing_image_is_not_found() {
    let reg = FakeRegistry::start(false);
    let e = Registry::new()
        .resolve(&reference(&reg, "nope"))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert_eq!(
        e.what,
        format!("cannot find image {}/nope:latest", reg.host)
    );
}

#[test]
fn manifest_digest_is_verified() {
    let reg = FakeRegistry::start(false);
    let real = reg.push("app", "v1", &small_image("hi"));
    // Serve the real manifest under a different digest.
    let fake = format!("sha256:{}", "0".repeat(64));
    reg.push("app", &fake, &small_image("hi"));
    let e = Registry::new()
        .resolve(&reference(&reg, &format!("app@{fake}")))
        .unwrap_err();
    assert!(e.why.contains("sha256 mismatch"), "{e}");
    assert_ne!(real, fake);
}

#[test]
fn blob_stream_matches_its_digest() {
    let reg = FakeRegistry::start(false);
    reg.push("app", "v1", &small_image("hi"));
    let r = reference(&reg, "app:v1");
    let mut registry = Registry::new();
    let resolved = registry.resolve(&r).unwrap();
    let d: &Descriptor = &resolved.layers[0];
    let mut bytes = Vec::new();
    registry
        .open_blob(&r, d)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(sha256(&bytes), d.digest);
}

#[test]
fn single_platform_image_for_another_arch_is_rejected() {
    let reg = FakeRegistry::start(false);
    reg.push_for_arch("other", "v1", &small_image("hi"), "s390x");
    let e = Registry::new()
        .resolve(&reference(&reg, "other:v1"))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.why.contains("linux/s390x"), "{e}");
}
