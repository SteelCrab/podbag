#![cfg(target_os = "linux")]
mod common;
use common::*;

#[test]
fn true_exits_zero() {
    let out = Sandbox::new().run(&["true"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn exit_code_propagates() {
    assert_eq!(
        Sandbox::new().run(&["sh", "-c", "exit 7"]).status.code(),
        Some(7)
    );
}

#[test]
fn command_is_pid_1_and_sees_no_other_process() {
    let out = Sandbox::new().run(&["sh", "-c", "echo $$; echo /proc/[0-9]*"]);
    assert_eq!(stdout(&out), "1\n/proc/1", "{}", stderr(&out));
}

#[test]
fn user_is_root_inside() {
    assert_eq!(stdout(&Sandbox::new().run(&["id", "-u"])), "0");
}

#[test]
fn namespaces_differ_from_the_host() {
    for ns in ["mnt", "pid", "uts", "ipc"] {
        let host = std::fs::read_link(format!("/proc/self/ns/{ns}")).unwrap();
        let inside = stdout(&Sandbox::new().run(&["readlink", &format!("/proc/self/ns/{ns}")]));
        assert!(!inside.is_empty(), "{ns}: no output");
        assert_ne!(
            inside,
            host.to_string_lossy(),
            "{ns} namespace is shared with the host"
        );
    }
}

#[test]
fn rootless_maps_root_to_the_caller_and_the_rest_to_subuids() {
    if root_mode() {
        return;
    }
    let out = Sandbox::new().run(&["cat", "/proc/self/uid_map"]);
    let rows: Vec<Vec<u64>> = stdout(&out)
        .lines()
        .map(|l| l.split_whitespace().map(|n| n.parse().unwrap()).collect())
        .collect();
    let uid = rustix::process::getuid().as_raw() as u64;
    assert_eq!(rows.len(), 2, "{}", stdout(&out));
    assert_eq!(rows[0], [0, uid, 1]);
    assert_eq!(rows[1][0], 1);
    assert!(rows[1][2] >= 65536);
}

#[test]
fn host_files_are_not_visible() {
    let marker = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let path = marker.path().to_str().unwrap();
    assert_eq!(
        Sandbox::new().run(&["test", "-e", path]).status.code(),
        Some(1)
    );
}

#[test]
fn device_nodes_work() {
    let out = Sandbox::new().run(&[
        "sh",
        "-c",
        "echo x > /dev/null && head -c 4 /dev/zero | wc -c",
    ]);
    assert_eq!(stdout(&out), "4", "{}", stderr(&out));
}

#[test]
fn command_not_found_exits_127_with_a_three_part_message() {
    let out = Sandbox::new().run(&["/no/such/program"]);
    assert_eq!(out.status.code(), Some(127));
    let err = stderr(&out);
    assert!(
        err.starts_with("openpod: cannot execute /no/such/program\n"),
        "{err}"
    );
    assert!(
        err.contains("\n  cause: no such file in the container"),
        "{err}"
    );
    assert!(err.contains("\n  fix:   "), "{err}");
}

#[test]
fn command_is_searched_in_the_container_path() {
    let out = Sandbox::new().run(&["no-such-program"]);
    assert_eq!(out.status.code(), Some(127));
    assert!(
        stderr(&out).contains(
            "searched PATH: /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        ),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_file_that_cannot_be_executed_exits_126() {
    assert_eq!(
        Sandbox::new().run(&["/etc/passwd"]).status.code(),
        Some(126)
    );
}

#[test]
fn missing_rootfs_exits_125() {
    let sb = Sandbox::new();
    let out = sb
        .openpod()
        .args(["run", "--rootfs", "/does/not/exist", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(125));
    assert!(
        stderr(&out).starts_with("openpod: cannot use root filesystem /does/not/exist\n"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn relative_rootfs_path_works() {
    let rootfs = std::path::PathBuf::from(rootfs());
    let sb = Sandbox::new();
    let out = sb
        .openpod()
        .current_dir(rootfs.parent().unwrap())
        .args([
            "run",
            "--rootfs",
            &format!("./{}", rootfs.file_name().unwrap().to_str().unwrap()),
            "true",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn arguments_after_the_command_are_not_parsed_as_options() {
    let out = Sandbox::new().run(&["sh", "-c", "echo $0 $1", "--rootfs", "-e"]);
    assert_eq!(stdout(&out), "--rootfs -e");
}

#[test]
fn nothing_is_left_behind() {
    let sb = Sandbox::new();
    sb.run(&["true"]);
    sb.run(&["/no/such/program"]);
    let left: Vec<_> = std::fs::read_dir(sb.data().join("containers"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
