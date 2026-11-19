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
        err.starts_with("podbag: cannot execute /no/such/program\n"),
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
        .podbag()
        .args(["run", "--rootfs", "/does/not/exist", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(125));
    assert!(
        stderr(&out).starts_with("podbag: cannot use root filesystem /does/not/exist\n"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn relative_rootfs_path_works() {
    let rootfs = std::path::PathBuf::from(rootfs());
    let sb = Sandbox::new();
    let out = sb
        .podbag()
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

/// A directory with a glibc program (the VM's own `true`) and a script for a missing
/// interpreter, to mount into the musl test root.
fn foreign_programs() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy("/usr/bin/true", dir.path().join("app")).unwrap();
    std::fs::write(
        dir.path().join("run.py"),
        "#!/nonexistent/python3\nprint(1)\n",
    )
    .unwrap();
    for f in ["app", "run.py"] {
        std::fs::set_permissions(dir.path().join(f), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

#[test]
fn a_glibc_program_in_a_musl_root_is_explained() {
    let dir = foreign_programs();
    let mount = format!("{}:/opt/t:ro", dir.path().display());
    let out = Sandbox::new().run_with(&["-v", &mount], &["/opt/t/app"]);
    assert_eq!(out.status.code(), Some(127), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with("podbag: cannot execute /opt/t/app\n"),
        "{err}"
    );
    assert!(err.contains("glibc") && err.contains("ld-linux"), "{err}");
    assert!(err.contains("check-image"), "{err}");
}

#[test]
fn a_script_for_a_missing_interpreter_is_explained() {
    let dir = foreign_programs();
    let mount = format!("{}:/opt/t:ro", dir.path().display());
    let out = Sandbox::new().run_with(&["-v", &mount], &["/opt/t/run.py"]);
    assert_eq!(out.status.code(), Some(127), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("/nonexistent/python3"), "{err}");
    assert!(err.contains("first line"), "{err}");
}

#[test]
fn running_a_fifo_fails_without_hanging() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    std::fs::set_permissions(&fifo, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mount = format!("{}:/opt/t", dir.path().display());
    let sb = Sandbox::new();
    let mut child = sb
        .podbag()
        .args([
            "run",
            "--rm",
            "--rootfs",
            &rootfs(),
            "-v",
            &mount,
            "/opt/t/pipe",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().ok();
            // Release the container that waits on the FIFO.
            std::fs::OpenOptions::new().write(true).open(&fifo).ok();
            panic!("running a FIFO hangs");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert_eq!(status.code(), Some(126));
}
