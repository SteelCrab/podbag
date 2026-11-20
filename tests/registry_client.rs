#![cfg(unix)]
mod common;
use common::registry::{Auth, FakeRegistry, REFRESH_TOKEN, TarEntry, TestImage, layer, sha256};
use podbag::ErrorKind;
use podbag::image::auth::AuthConfig;
use podbag::image::manifest::Descriptor;
use podbag::image::reference::Reference;
use podbag::image::registry::{Blob, Registry, is_login_failure, search};
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

fn login(user: &str, pass: &str) -> Option<AuthConfig> {
    Some(AuthConfig::with_password(user, pass))
}

#[test]
fn a_private_image_needs_a_login_and_pulls_with_one() {
    for auth in [Auth::basic("u", "pw"), Auth::token("u", "pw")] {
        let reg = FakeRegistry::start_with(auth.clone());
        reg.push("team/app", "v1", &small_image("secret"));
        let r = reference(&reg, "team/app:v1");
        let e = Registry::new().resolve(&r).unwrap_err();
        assert_eq!(e.kind, ErrorKind::MissingRequirement, "{auth:?}: {e}");
        assert!(
            e.next.contains(&format!("podbag login {}", reg.host)),
            "{e}"
        );
        let resolved = Registry::with_auth(login("u", "pw")).resolve(&r).unwrap();
        assert_eq!(resolved.layers.len(), 1);
        // A wrong password says so, not just "cannot pull".
        let e = Registry::with_auth(login("u", "wrong"))
            .resolve(&r)
            .unwrap_err();
        assert!(is_login_failure(&e), "{auth:?}: {e:?}");
        assert_eq!(e.what, format!("login to {} failed", reg.host));
    }
}

#[test]
fn a_token_is_asked_for_the_scope_and_kept() {
    let reg = FakeRegistry::start_with(Auth::token("u", "pw"));
    reg.push("team/app", "v1", &small_image("hi"));
    let r = reference(&reg, "team/app:v1");
    let mut registry = Registry::with_auth(login("u", "pw"));
    let resolved = registry.resolve(&r).unwrap();
    let mut bytes = Vec::new();
    registry
        .open_blob(&r, &resolved.layers[0])
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    // One token for the pulls: the manifest, the config and the blob reuse it.
    let tokens = reg.requests().iter().filter(|q| *q == "GET /token").count();
    assert_eq!(tokens, 1, "{:?}", reg.requests());
}

#[test]
fn login_checks_the_password_and_keeps_an_identity_token() {
    let reg = FakeRegistry::start_with(Auth::token("u", "pw"));
    let token = Registry::with_auth(login("u", "pw"))
        .login(&reg.host)
        .unwrap();
    assert_eq!(token.as_deref(), Some(REFRESH_TOKEN));
    let e = Registry::with_auth(login("u", "nope"))
        .login(&reg.host)
        .unwrap_err();
    assert!(is_login_failure(&e), "{e:?}");

    // The identity token logs in without the password (OAuth refresh token).
    reg.push("team/app", "v1", &small_image("hi"));
    let with_token = AuthConfig {
        username: "u".into(),
        identitytoken: REFRESH_TOKEN.into(),
        ..AuthConfig::default()
    };
    Registry::with_auth(Some(with_token))
        .resolve(&reference(&reg, "team/app:v1"))
        .unwrap();
    assert!(reg.requests().contains(&"POST /token".to_string()));

    let basic = FakeRegistry::start_with(Auth::basic("u", "pw"));
    assert_eq!(
        Registry::with_auth(login("u", "pw"))
            .login(&basic.host)
            .unwrap(),
        None
    );
    assert!(is_login_failure(
        &Registry::with_auth(login("u", "x"))
            .login(&basic.host)
            .unwrap_err()
    ));
    // An open registry takes any login.
    let open = FakeRegistry::start(false);
    Registry::with_auth(login("u", "x"))
        .login(&open.host)
        .unwrap();
}

#[test]
fn uploads_blobs_and_a_manifest() {
    let reg = FakeRegistry::start_with(Auth::token("u", "pw"));
    let r = reference(&reg, "team/new:v2");
    let mut registry = Registry::with_auth(login("u", "pw"));
    let layer = b"layer bytes".to_vec();
    let layer_digest = sha256(&layer);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layer");
    std::fs::write(&path, &layer).unwrap();
    assert!(!registry.blob_exists(&r, &layer_digest).unwrap());
    registry
        .upload_blob(&r, &layer_digest, Blob::File(&path))
        .unwrap();
    assert!(registry.blob_exists(&r, &layer_digest).unwrap());
    assert_eq!(reg.blob(&layer_digest).unwrap(), layer);
    let config = br#"{"architecture":"arm64","os":"linux"}"#;
    let config_digest = sha256(config);
    registry
        .upload_blob(&r, &config_digest, Blob::Bytes(config))
        .unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer_digest, "size": layer.len()}],
    }))
    .unwrap();
    let digest = registry
        .put_manifest(&r, "application/vnd.oci.image.manifest.v1+json", &manifest)
        .unwrap();
    assert_eq!(digest, sha256(&manifest));
    assert_eq!(reg.manifest("team/new", "v2").unwrap().1, manifest);
    // A wrong digest is refused, with the registry's reason.
    let e = registry
        .upload_blob(&r, &sha256(b"other"), Blob::Bytes(b"not it"))
        .unwrap_err();
    assert!(e.why.contains("did not match"), "{e}");
    // Pushing needs a login that may push.
    let e = Registry::new()
        .upload_blob(&r, &layer_digest, Blob::Bytes(&layer))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::MissingRequirement, "{e}");
}

#[test]
fn mounts_a_blob_from_another_repository() {
    let reg = FakeRegistry::start_with(Auth::token("u", "pw"));
    let image = small_image("base");
    reg.push("library/base", "1", &image);
    let digest = FakeRegistry::layer_digests(&image)[0].clone();
    let r = reference(&reg, "team/derived:1");
    let mut registry = Registry::with_auth(login("u", "pw"));
    assert!(registry.mount_blob(&r, &digest, "library/base").unwrap());
    assert!(reg.holds("team/derived", &digest));
    // A blob the source lacks is not mounted; the upload session it began is cancelled.
    let missing = sha256(b"nowhere");
    assert!(!registry.mount_blob(&r, &missing, "library/base").unwrap());
    assert!(reg.requests().iter().any(|q| q.starts_with("DELETE ")));
}

#[test]
fn searches_a_registry_index() {
    let reg = FakeRegistry::start(false);
    let found = search(&format!("{}/web", reg.host), 2).unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].name, "web-0");
    assert!(found[0].is_official);
    assert_eq!(found[1].star_count, 9);
}
