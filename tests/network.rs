#![cfg(target_os = "linux")]
//! Pulls real images from Docker Hub. Runs only with OPENPOD_TEST_NETWORK=1.

mod common;
use common::*;

fn enabled() -> bool {
    std::env::var("OPENPOD_TEST_NETWORK").as_deref() == Ok("1")
}

#[test]
fn runs_alpine_from_docker_hub() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    let out = sb
        .openpod()
        .args(["run", "alpine", "cat", "/etc/os-release"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("Alpine"), "{}", stdout(&out));
}

#[test]
fn runs_a_glibc_image_from_docker_hub() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    let out = sb
        .openpod()
        .args([
            "run",
            "debian:stable-slim",
            "sh",
            "-c",
            "cat /etc/os-release; ls /lib/*/libc.so.6",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("Debian"), "{}", stdout(&out));
    assert!(stdout(&out).contains("libc.so.6"), "{}", stdout(&out));
}

#[test]
fn check_image_passes_alpine_and_debian() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    for image in ["alpine", "debian:stable-slim"] {
        let out = sb.openpod().args(["pull", image]).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        let out = sb.openpod().args(["check-image", image]).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{image}: {}", stdout(&out));
        assert!(
            stdout(&out).contains("no problems found"),
            "{image}: {}",
            stdout(&out)
        );
    }
}
