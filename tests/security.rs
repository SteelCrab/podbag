#![cfg(target_os = "linux")]
//! Security defaults: capabilities, no_new_privs, seccomp, hidden and read-only paths.

mod common;
use common::*;

fn ok(out: &std::process::Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

/// A field of `/proc/self/status` in a container run with `options`.
fn status_field(sb: &Sandbox, options: &[&str], field: &str) -> String {
    let pattern = format!("^{field}:");
    let out = sb.run_with(options, &["grep", &pattern, "/proc/self/status"]);
    let line = ok(&out);
    line.split_once(':')
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default()
}

#[test]
fn default_capabilities_match_docker() {
    let sb = Sandbox::new();
    assert_eq!(status_field(&sb, &[], "CapBnd"), "00000000a80425fb");
    assert_eq!(status_field(&sb, &[], "CapEff"), "00000000a80425fb");
}

#[test]
fn cap_drop_all_and_add_one() {
    let sb = Sandbox::new();
    let opts = ["--cap-drop", "ALL", "--cap-add", "NET_RAW"];
    assert_eq!(status_field(&sb, &opts, "CapBnd"), "0000000000002000");
}

#[test]
fn no_new_privileges_flag() {
    let sb = Sandbox::new();
    assert_eq!(status_field(&sb, &[], "NoNewPrivs"), "0");
    let opts = ["--security-opt", "no-new-privileges"];
    assert_eq!(status_field(&sb, &opts, "NoNewPrivs"), "1");
}

#[test]
fn non_root_user_has_no_capabilities() {
    let sb = Sandbox::new();
    assert_eq!(
        status_field(&sb, &["-u", "405"], "CapEff"),
        "0000000000000000"
    );
}

#[test]
fn unknown_capability_is_rejected() {
    let sb = Sandbox::new();
    let out = sb.run_with(&["--cap-add", "NOPE"], &["true"]);
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
    assert!(stderr(&out).contains("NOPE"), "{}", stderr(&out));
}

#[test]
fn seccomp_is_on_by_default() {
    let sb = Sandbox::new();
    assert_eq!(status_field(&sb, &[], "Seccomp"), "2");
    let out = sb.run(&["unshare", "-U", "true"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("Operation not permitted"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn seccomp_unconfined_option() {
    let sb = Sandbox::new();
    let opts = ["--security-opt", "seccomp=unconfined"];
    assert_eq!(status_field(&sb, &opts, "Seccomp"), "0");
}

#[test]
fn masked_paths_are_hidden() {
    let sb = Sandbox::new();
    let script = "wc -c < /proc/kcore; ls -A /sys/firmware | wc -l; cat /proc/keys | wc -c";
    let out = sb.run(&["sh", "-c", script]);
    assert_eq!(
        stdout(&out).split_whitespace().collect::<Vec<_>>(),
        ["0", "0", "0"],
        "{}",
        stderr(&out)
    );
}

#[test]
fn readonly_proc_paths() {
    let sb = Sandbox::new();
    let script = r#"
        for p in /proc/bus /proc/fs /proc/irq /proc/sys /proc/sysrq-trigger; do
            [ -e $p ] && echo $p
        done
        echo --
        awk '$6 ~ /^ro/ {print $5}' /proc/self/mountinfo |
            grep -E '^/proc/(bus|fs|irq|sys|sysrq-trigger)$' | sort -u
    "#;
    let out = ok(&sb.run(&["sh", "-c", script]));
    let (present, read_only) = out.split_once("--").unwrap();
    let present: Vec<&str> = present.split_whitespace().collect();
    assert!(!present.is_empty(), "{out}");
    assert_eq!(present, read_only.split_whitespace().collect::<Vec<_>>());
}

#[test]
fn sys_is_read_only() {
    let sb = Sandbox::new();
    let out = sb.run(&["mkdir", "/sys/fs/x"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn mounts_do_not_leak_to_the_host() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    let args = [
        "run", "-d", "--name", "leak", "--rootfs", &rootfs, "sleep", "30",
    ];
    let id = ok(&sb.openpod().args(args).output().unwrap());
    std::thread::sleep(std::time::Duration::from_millis(300));
    let host = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(!host.contains(&id), "{host}");
    ok(&sb.openpod().args(["rm", "-f", "leak"]).output().unwrap());
}

#[test]
fn exec_gets_the_same_restrictions() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    let args = [
        "run",
        "-d",
        "--name",
        "e",
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_RAW",
        "--security-opt",
        "no-new-privileges",
        "--rootfs",
        &rootfs,
        "sleep",
        "30",
    ];
    ok(&sb.openpod().args(args).output().unwrap());
    std::thread::sleep(std::time::Duration::from_millis(200));
    let out = sb
        .openpod()
        .args(["exec", "e", "grep", "-E", "^(CapBnd|NoNewPrivs|Seccomp):"])
        .arg("/proc/self/status")
        .output()
        .unwrap();
    let fields: Vec<String> = ok(&out)
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        fields,
        ["CapBnd: 0000000000002000", "NoNewPrivs: 1", "Seccomp: 2"]
    );
    ok(&sb.openpod().args(["rm", "-f", "e"]).output().unwrap());
}

// A caller with inheritable or ambient capabilities (pam_cap, a systemd service with
// AmbientCapabilities=, capsh) must not pass them into a root-mode container.
#[test]
fn inherited_capabilities_do_not_reach_the_container() {
    if !root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let script = format!(
        "exec env OPENPOD_DATA_DIR={} OPENPOD_RUNTIME_DIR={} {BIN} run --rm --rootfs {} \
         grep -E '^Cap(Inh|Eff|Amb):' /proc/self/status",
        sb.data().display(),
        sb.runtime().display(),
        rootfs()
    );
    let out = std::process::Command::new("sudo")
        .args([
            "-n",
            "capsh",
            "--inh=cap_sys_admin",
            "--addamb=cap_sys_admin",
        ])
        .args(["--", "-c", &script])
        .output()
        .unwrap();
    let fields: Vec<String> = ok(&out)
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        fields,
        [
            "CapInh: 0000000000000000",
            "CapEff: 00000000a80425fb",
            "CapAmb: 0000000000000000"
        ]
    );
}

// Like Docker: granting a capability also lifts the seccomp rule for its calls.
#[test]
fn cap_add_sys_admin_allows_mount() {
    let sb = Sandbox::new();
    let out = sb.run_with(
        &["--cap-add", "SYS_ADMIN"],
        &["sh", "-c", "mount -t tmpfs none /mnt && echo ok"],
    );
    assert_eq!(ok(&out), "ok");
}
