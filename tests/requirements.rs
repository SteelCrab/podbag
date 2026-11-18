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

/// The line of `openpod check` output for one item: `<level> <name> ...`.
fn check_line<'a>(out: &'a str, name: &str) -> &'a str {
    out.lines()
        .find(|l| {
            l.split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ")
                .starts_with(name)
        })
        .unwrap_or_else(|| panic!("no {name} line in:\n{out}"))
}

#[test]
fn check_passes_on_the_test_system() {
    let sb = Sandbox::new();
    let out = sb.openpod().arg("check").output().unwrap();
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}{}", stderr(&out));
    assert!(check_line(&text, "kernel").starts_with("ok"), "{text}");
    assert!(check_line(&text, "cgroup v2").starts_with("ok"), "{text}");
    if root_mode() {
        assert!(check_line(&text, "pasta").starts_with("n/a"), "{text}");
    } else {
        assert!(check_line(&text, "pasta").starts_with("ok"), "{text}");
        assert!(check_line(&text, "apparmor").starts_with("ok"), "{text}");
    }
}

#[test]
fn check_shows_the_image_mount_method() {
    let sb = Sandbox::new();
    let out = sb.openpod().arg("check").output().unwrap();
    let text = stdout(&out);
    let line = check_line(&text, "image mount");
    if root_mode() {
        assert!(line.starts_with("ok"), "{text}");
        assert!(line.contains("composefs"), "{text}");
    } else {
        assert!(line.starts_with("n/a"), "{text}");
    }
}

#[test]
fn check_reports_a_missing_pasta() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let out = sb
        .openpod()
        .arg("check")
        .env("OPENPOD_PASTA", "/nonexistent/pasta")
        .output()
        .unwrap();
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(check_line(&text, "pasta").starts_with("fail"), "{text}");
    assert!(text.contains("fix:") && text.contains("passt"), "{text}");
}

#[test]
fn check_prints_the_apparmor_profile() {
    let sb = Sandbox::new();
    let out = sb
        .openpod()
        .args(["check", "--apparmor-profile"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("  userns,"), "{text}");
    // The profile names this binary's real path.
    let bin = std::fs::canonicalize(env!("CARGO_BIN_EXE_openpod")).unwrap();
    assert!(
        text.contains(&format!("profile openpod {} ", bin.display())),
        "{text}"
    );
}

#[test]
fn check_shows_the_keeper() {
    let on = Sandbox::with_env("OPENPOD_KEEPER", "1");
    let text = stdout(&on.openpod().arg("check").output().unwrap());
    let line = check_line(&text, "keeper");
    assert!(line.starts_with("ok"), "{text}");
    assert!(
        line.contains("one process watches all detached containers"),
        "{text}"
    );
    let off = Sandbox::with_env("OPENPOD_KEEPER", "0");
    let text = stdout(&off.openpod().arg("check").output().unwrap());
    let line = check_line(&text, "keeper");
    assert!(line.starts_with("ok"), "{text}");
    assert!(
        line.contains("off: every container has a shim of its own"),
        "{text}"
    );
}
