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

/// Every capability this kernel has, as `CapEff` in /proc/<pid>/status writes them.
fn all_capabilities() -> String {
    let last: u32 = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    format!("{:016x}", (1u64 << (last + 1)) - 1)
}

#[test]
fn privileged_gets_every_capability() {
    let sb = Sandbox::new();
    assert_eq!(
        status_field(&sb, &["--privileged"], "CapEff"),
        all_capabilities()
    );
    // Without it, the default set.
    assert_ne!(status_field(&sb, &[], "CapEff"), all_capabilities());
}

#[test]
fn privileged_has_no_masked_paths() {
    let sb = Sandbox::new();
    // Masked files are covered with /dev/null, a character device.
    let masked = sb.run_with(&[], &["test", "-c", "/proc/kcore"]);
    assert_eq!(masked.status.code(), Some(0), "{}", stderr(&masked));
    let open = sb.run_with(&["--privileged"], &["test", "!", "-c", "/proc/kcore"]);
    assert_eq!(open.status.code(), Some(0), "{}", stderr(&open));
    // The container's cgroup can take groups of its own (BuildKit makes them).
    let script = "mkdir /sys/fs/cgroup/inner && rmdir /sys/fs/cgroup/inner";
    let cgroup = sb.run_with(&["--privileged"], &["sh", "-c", script]);
    assert_eq!(cgroup.status.code(), Some(0), "{}", stderr(&cgroup));
}

#[test]
fn privileged_can_mount() {
    let sb = Sandbox::new();
    let script = "mount -t tmpfs none /mnt && touch /mnt/x && umount /mnt";
    let out = sb.run_with(&["--privileged"], &["sh", "-c", script]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let refused = sb.run_with(&[], &["sh", "-c", script]);
    assert_ne!(refused.status.code(), Some(0));
}

#[test]
fn privileged_rootless_stays_in_its_namespace() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    // Still mapped: the host's IDs are not the container's.
    let map = ok(&sb.run_with(&["--privileged"], &["cat", "/proc/self/uid_map"]));
    assert!(!map.contains("4294967295"), "{map}");
    // The host's kernel settings stay the host's.
    let out = sb.run_with(
        &["--privileged"],
        &["sh", "-c", "echo 0 > /proc/sys/kernel/sysrq"],
    );
    assert_ne!(out.status.code(), Some(0), "a host setting was changed");
}

#[test]
fn exec_reaches_a_container_that_nests_cgroups() {
    // As BuildKit and docker-in-docker do: the main process moves into a group of its own
    // and turns on a controller for the groups below: no process may stay at the top.
    let sb = Sandbox::new();
    let script = "c=$(cut -d' ' -f1 /sys/fs/cgroup/cgroup.controllers); [ -n \"$c\" ] || exit 7; \
                  mkdir /sys/fs/cgroup/init && echo $$ > /sys/fs/cgroup/init/cgroup.procs && \
                  echo +$c > /sys/fs/cgroup/cgroup.subtree_control && touch /run/nested && sleep 300";
    let rootfs = rootfs();
    let out = sb
        .openpod()
        .args([
            "run",
            "-d",
            "--name",
            "nest",
            "--privileged",
            "--tmpfs",
            "/run",
            "--rootfs",
            &rootfs,
            "sh",
            "-c",
            script,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let ready = sb
            .openpod()
            .args(["exec", "nest", "test", "-e", "/run/nested"])
            .output()
            .unwrap();
        if ready.status.code() == Some(0) {
            break;
        }
        let info = ok(&sb.openpod().args(["inspect", "nest"]).output().unwrap());
        if info.contains("\"exit_code\":7") {
            eprintln!("no controller to turn on: skipped");
            let _ = sb.openpod().args(["rm", "-f", "nest"]).output();
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "exec never reached it: {}",
            stderr(&ready)
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    ok(&sb
        .openpod()
        .args(["exec", "nest", "echo", "inside"])
        .output()
        .unwrap());
    ok(&sb.openpod().args(["rm", "-f", "nest"]).output().unwrap());
}

/// The host's block device that holds `/`, as major and minor numbers.
fn host_disk() -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata("/").unwrap().dev();
    (libc::major(dev), libc::minor(dev))
}

#[test]
fn host_devices_stay_closed() {
    // As root a container may make device nodes (CAP_MKNOD, as with Docker), but opening
    // one is refused unless it is one of the usual few (/dev/null, …). Rootless, the user
    // namespace refuses both. The node goes in the image's overlay: the test directories
    // are `nodev` and would refuse it for another reason.
    let reg = common::registry::FakeRegistry::start(false);
    reg.push(
        "img",
        "v1",
        &common::registry::TestImage {
            layers: vec![common::registry::alpine_layer()],
            config: serde_json::json!({"Env": ["PATH=/usr/sbin:/usr/bin:/sbin:/bin"]}),
        },
    );
    let image = format!("{}/img:v1", reg.host);
    let sb = Sandbox::new();
    ok(&sb.openpod().args(["pull", &image]).output().unwrap());
    let (major, minor) = host_disk();
    let script =
        format!("mknod /disk b {major} {minor}; head -c 1 /disk > /dev/null && echo OPENED");
    let out = sb
        .openpod()
        .args(["run", "--rm", &image, "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(
        !stdout(&out).contains("OPENED"),
        "a container opened the host's disk: {}",
        stderr(&out)
    );
    // The usual devices still work.
    let usual = sb.run_with(
        &[],
        &[
            "sh",
            "-c",
            "head -c 4 /dev/zero > /dev/null && head -c 4 /dev/urandom | wc -c",
        ],
    );
    assert_eq!(ok(&usual), "4");
}

/// The cgroup directory of a running container, from `inspect`.
fn cgroup_of(sb: &Sandbox, name: &str) -> String {
    let text = ok(&sb.openpod().args(["inspect", name]).output().unwrap());
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let v = if v.is_array() { v[0].clone() } else { v };
    v["state"]["cgroup"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn a_container_that_nests_cgroups_starts_again() {
    // BuildKit's way (see above). Its groups below the container's must go with it, or
    // the next start finds the container's group busy, and the group stays behind.
    let sb = Sandbox::new();
    let script = "c=$(cut -d' ' -f1 /sys/fs/cgroup/cgroup.controllers); [ -n \"$c\" ] || exit 7; \
                  mkdir /sys/fs/cgroup/init && echo $$ > /sys/fs/cgroup/init/cgroup.procs && \
                  echo +$c > /sys/fs/cgroup/cgroup.subtree_control && touch /run/nested && sleep 300";
    let rootfs = rootfs();
    ok(&sb
        .openpod()
        .args([
            "run",
            "-d",
            "--name",
            "nest2",
            "--privileged",
            "--tmpfs",
            "/run",
            "--rootfs",
            &rootfs,
            "sh",
            "-c",
            script,
        ])
        .output()
        .unwrap());
    let nested = |sb: &Sandbox| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ready = sb
                .openpod()
                .args(["exec", "nest2", "test", "-e", "/run/nested"])
                .output()
                .unwrap();
            if ready.status.code() == Some(0) {
                return true;
            }
            let info = ok(&sb.openpod().args(["inspect", "nest2"]).output().unwrap());
            if info.contains("\"exit_code\":7") {
                return false;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "it never nested: {}",
                stderr(&ready)
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };
    if !nested(&sb) {
        eprintln!("no controller to turn on: skipped");
        let _ = sb.openpod().args(["rm", "-f", "nest2"]).output();
        return;
    }
    ok(&sb
        .openpod()
        .args(["stop", "-t", "1", "nest2"])
        .output()
        .unwrap());
    ok(&sb.openpod().args(["start", "nest2"]).output().unwrap());
    assert!(nested(&sb));
    let group = cgroup_of(&sb, "nest2");
    assert!(
        !group.is_empty() && std::path::Path::new(&group).exists(),
        "{group}"
    );
    ok(&sb.openpod().args(["rm", "-f", "nest2"]).output().unwrap());
    assert!(
        !std::path::Path::new(&group).exists(),
        "the container's cgroup stayed: {group}"
    );
}

#[test]
fn privileged_gets_no_host_devices_and_keeps_kernel_settings_apart() {
    let sb = Sandbox::new();
    // No disks or loop devices of the host in /dev.
    let devices = ok(&sb.run_with(&["--privileged"], &["ls", "/dev"]));
    for host in ["vda", "sda", "nvme0", "loop0", "mem", "kmsg"] {
        assert!(
            !devices.lines().any(|d| d == host),
            "/dev/{host}: {devices}"
        );
    }
    // The kernel settings of its own network namespace are writable; without
    // --privileged /proc/sys is read-only.
    let set = "echo 1 > /proc/sys/net/ipv4/ip_forward";
    let privileged = sb.run_with(&["--privileged"], &["sh", "-c", set]);
    assert_eq!(privileged.status.code(), Some(0), "{}", stderr(&privileged));
    let plain = sb.run_with(&[], &["sh", "-c", set]);
    assert_ne!(plain.status.code(), Some(0));
    // Rootless on the host's network: the host's /sys stays the host's.
    if !root_mode() {
        let host_sys = sb.run_with(
            &["--privileged", "--network", "host"],
            &["sh", "-c", "echo 0 > /sys/kernel/mm/ksm/run"],
        );
        assert_ne!(
            host_sys.status.code(),
            Some(0),
            "a host setting was changed"
        );
    }
}
