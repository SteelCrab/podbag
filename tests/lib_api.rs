#![cfg(target_os = "linux")]
mod common;
use common::*;
use openpod::Container;
use std::sync::OnceLock;

static TMP: OnceLock<tempfile::TempDir> = OnceLock::new();

// Library tests run openpod as the test user, so they are rootless only.
fn setup() {
    TMP.get_or_init(|| {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: every test sets the same values once, before any test reads them.
        unsafe {
            std::env::set_var("OPENPOD_BIN", BIN);
            std::env::set_var("OPENPOD_DATA_DIR", tmp.path().join("data"));
            std::env::set_var("OPENPOD_RUNTIME_DIR", tmp.path().join("run"));
        }
        tmp
    });
}

#[test]
fn run_captures_output_and_exit_code() {
    if root_mode() {
        return;
    }
    setup();
    let out = Container::from_rootfs(rootfs())
        .command(["sh", "-c", "echo out; echo err >&2; exit 3"])
        .run()
        .unwrap();
    assert_eq!(
        (out.exit_code, out.stdout.as_slice(), out.stderr.as_slice()),
        (3, &b"out\n"[..], &b"err\n"[..])
    );
}

#[test]
fn run_captures_large_output_on_both_streams() {
    if root_mode() {
        return;
    }
    setup();
    let out = Container::from_rootfs(rootfs())
        .command([
            "sh",
            "-c",
            "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2",
        ])
        .run()
        .unwrap();
    assert_eq!(
        (out.exit_code, out.stdout.len(), out.stderr.len()),
        (0, 1048576, 1048576)
    );
}

#[test]
fn builder_options_are_applied() {
    if root_mode() {
        return;
    }
    setup();
    let out = Container::from_rootfs(rootfs())
        .env("FOO", "bar")
        .workdir("/tmp")
        .hostname("box")
        .command(["sh", "-c", "echo $FOO; pwd; hostname"])
        .run()
        .unwrap();
    assert_eq!(out.stdout, b"bar\n/tmp\nbox\n");
}

#[test]
fn works_from_a_multithreaded_process() {
    if root_mode() {
        return;
    }
    setup();
    let threads: Vec<_> = (0..4)
        .map(|i| {
            std::thread::spawn(move || {
                Container::from_rootfs(rootfs())
                    .command(["sh", "-c", &format!("exit {i}")])
                    .run()
                    .unwrap()
                    .exit_code
            })
        })
        .collect();
    let codes: Vec<i32> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(codes, [0, 1, 2, 3]);
}

#[test]
fn errors_are_typed() {
    if root_mode() {
        return;
    }
    setup();
    let e = Container::from_rootfs(rootfs())
        .command(["/no/such/program"])
        .run()
        .unwrap_err();
    assert_eq!(e.kind, openpod::ErrorKind::CommandNotFound);
    let e = Container::from_rootfs(rootfs()).run().unwrap_err();
    assert_eq!(e.kind, openpod::ErrorKind::InvalidInput);
}

// A host app may hold descriptors without close-on-exec (common with C libraries).
// None of them may reach the container.
#[test]
fn host_fds_do_not_leak_into_the_container() {
    if root_mode() {
        return;
    }
    setup();
    let file = std::fs::File::open("/etc/hostname").unwrap();
    // SAFETY: dup returns a new descriptor without FD_CLOEXEC; it is closed below.
    let leaked = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&file)) };
    assert!(leaked > 2);
    let out = Container::from_rootfs(rootfs())
        .command(["ls", "-1", "/proc/self/fd"])
        .run()
        .unwrap();
    unsafe { libc::close(leaked) };
    // ls itself opens one descriptor (3) to read the directory.
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0\n1\n2\n3\n");
}

#[test]
fn library_start_and_wait() {
    if root_mode() {
        return;
    }
    setup();
    let handle = Container::from_rootfs(rootfs())
        .name("libbox")
        .command(["sh", "-c", "echo hi; exit 4"])
        .start()
        .unwrap();
    assert_eq!(handle.wait().unwrap(), 4);
    let info = handle.info().unwrap();
    assert_eq!(info.name, "libbox");
    let logs: Vec<String> = handle.logs().unwrap().into_iter().map(|l| l.log).collect();
    assert_eq!(logs, ["hi\n"]);
    handle.remove(false).unwrap();
    assert!(openpod::ContainerHandle::get("libbox").is_err());
}

#[test]
fn library_exec_captures_output() {
    if root_mode() {
        return;
    }
    setup();
    let handle = Container::from_rootfs(rootfs())
        .name("libexec")
        .command(["sleep", "30"])
        .start()
        .unwrap();
    let out = handle
        .exec(
            &["sh", "-c", "echo in; echo err >&2; exit 2"],
            &openpod::ExecOptions::default(),
        )
        .unwrap();
    assert_eq!(
        (out.exit_code, out.stdout.as_slice(), out.stderr.as_slice()),
        (2, &b"in\n"[..], &b"err\n"[..])
    );
    handle.remove(true).unwrap();
}

#[test]
fn library_runs_as_root() {
    if !root_mode() {
        return;
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        // Run this test again, as root.
        let out = std::process::Command::new("sudo")
            .args(["-n", "env"])
            .arg(format!("OPENPOD_TEST_ROOTFS={}", rootfs()))
            .arg("OPENPOD_TEST_ROOT=1")
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "library_runs_as_root", "--nocapture"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("1 passed"), "{text}");
        return;
    }
    setup();
    let out = Container::from_rootfs(rootfs())
        .command(["cat", "/proc/self/uid_map"])
        .run()
        .unwrap();
    // Root mode has no user namespace: the map is the whole identity map.
    let map = String::from_utf8_lossy(&out.stdout);
    assert!(map.contains("4294967295"), "{map}");
}
