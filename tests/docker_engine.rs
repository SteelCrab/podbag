#![cfg(unix)]
//! The UIs' client against a real Docker Engine. Runs only with PODBAG_TEST_DOCKER=1 and a
//! Docker that already runs (it is never started): in the dev VM, start it for the test
//! and stop it after with dev/bench/docker-off.sh. It only reads, except a container of
//! its own that it removes.

use podbag::client::{Choice, Client, Target};
use std::path::Path;

fn docker() -> Option<Client> {
    if std::env::var("PODBAG_TEST_DOCKER").as_deref() != Ok("1") {
        eprintln!("PODBAG_TEST_DOCKER is not 1: skipped");
        return None;
    }
    let target = Target::find(Some(&Choice::Docker), Path::new("podbag")).unwrap();
    target
        .ensure_running(Path::new("podbag"), false)
        .expect("Docker runs");
    Some(Client::new(target.endpoint))
}

#[test]
fn lists_and_features_work_against_docker() {
    let Some(c) = docker() else { return };
    c.ping().unwrap();
    let v = c.version().unwrap();
    assert!(v["Version"].is_string(), "{v}");
    c.containers().unwrap();
    c.images().unwrap();
    c.volumes().unwrap();
    let networks = c.networks().unwrap();
    assert!(networks.iter().any(|n| n.name == "bridge"), "{networks:?}");
    // Docker has what podbag's API lacks so far: the UIs show these actions.
    let f = c.features(None);
    assert!(f.pause && f.rename && f.stats && f.top, "{f:?}");
}

#[test]
fn search_works_against_docker() {
    let Some(c) = docker() else { return };
    match c.search("alpine", 5) {
        Ok(found) => assert!(found.iter().any(|r| r.name.contains("alpine")), "{found:?}"),
        // No network in the VM: still not "unsupported".
        Err(e) => assert!(!e.unsupported(), "{e:?}"),
    }
}
