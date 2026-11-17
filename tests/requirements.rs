#![cfg(target_os = "linux")]
mod common;
use common::*;

#[test]
fn missing_newuidmap_is_explained() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let out = sb
        .openpod()
        .env("PATH", "/nonexistent")
        .args(["run", "--rootfs", &rootfs(), "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(125));
    // Without PATH, systemd-run is missing too: a warning comes first.
    let err: String = stderr(&out)
        .lines()
        .filter(|l| !l.starts_with("openpod: warning:"))
        .map(|l| format!("{l}\n"))
        .collect();
    assert!(
        err.starts_with("openpod: cannot map user IDs for the container\n"),
        "{err}"
    );
    assert!(
        err.contains("cause: newuidmap was not found in PATH"),
        "{err}"
    );
    assert!(err.contains("uidmap"), "{err}");
}

#[test]
fn apparmor_restriction_is_explained() {
    if root_mode() {
        return;
    }
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
    if !restricted {
        eprintln!("skipped: this kernel does not restrict unprivileged user namespaces");
        return;
    }
    // Copy the binary to a path that no AppArmor profile covers.
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let copy = dir.path().join("openpod");
    std::fs::copy(BIN, &copy).unwrap();
    let sb = Sandbox::new();
    let out = sb
        .openpod_bin(copy.to_str().unwrap())
        .args(["run", "--rootfs", &rootfs(), "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(125));
    let err = stderr(&out);
    assert!(
        err.starts_with("openpod: cannot create an unprivileged user namespace\n"),
        "{err}"
    );
    assert!(err.contains("AppArmor"), "{err}");
}

#[test]
fn failed_setup_leaves_nothing_behind() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    sb.openpod()
        .env("PATH", "/nonexistent")
        .args(["run", "--rootfs", &rootfs(), "true"])
        .output()
        .unwrap();
    let left: Vec<_> = std::fs::read_dir(sb.data().join("containers"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
