#![cfg(target_os = "linux")]
mod common;
use common::*;
use std::io::{BufRead, BufReader};
use std::process::Stdio;

#[test]
fn env_and_workdir_are_applied() {
    let out = Sandbox::new().run_with(
        &["-e", "FOO=a=b", "-w", "/tmp"],
        &["sh", "-c", "echo $FOO; pwd"],
    );
    assert_eq!(stdout(&out), "a=b\n/tmp", "{}", stderr(&out));
}

#[test]
fn default_environment_matches_docker() {
    let out = Sandbox::new().run_with(
        &["--hostname", "box"],
        &[
            "sh",
            "-c",
            "echo $PATH; echo $HOME; echo $HOSTNAME; hostname",
        ],
    );
    assert_eq!(
        stdout(&out),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n/root\nbox\nbox"
    );
}

#[test]
fn default_hostname_is_the_short_id() {
    let name = stdout(&Sandbox::new().run(&["hostname"]));
    assert_eq!(name.len(), 12);
    assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
}

/// The PID of the only child of `pid`.
fn only_child(pid: u32) -> u32 {
    let text = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap();
    let pids: Vec<u32> = text
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 1, "expected one child of {pid}, found {pids:?}");
    pids[0]
}

/// Wait until the container prints `ready`.
fn wait_ready(child: &mut std::process::Child) {
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready");
}

// PID 1 ignores SIGKILL sent from inside its own namespace, so the test sends it from the host.
// Process chain: openpod (CLI) -> openpod __shim -> sleep (PID 1 in the container).
#[test]
fn killed_by_signal_exits_128_plus_signal() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let mut child = sb
        .openpod()
        .args([
            "run",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            "echo ready; exec sleep 30",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_ready(&mut child);
    let init = only_child(only_child(child.id()));
    unsafe { libc::kill(init as i32, libc::SIGKILL) };
    assert_eq!(child.wait().unwrap().code(), Some(137));
}

#[test]
fn closed_stdin_does_not_hang() {
    let sb = Sandbox::new();
    let out = sb
        .openpod()
        .args(["run", "--rootfs", &rootfs(), "cat"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
}

// In root mode the test cannot signal the sudo process, so this runs rootless only.
#[test]
fn sigterm_reaches_the_container() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let mut child = sb
        .openpod()
        .args([
            "run",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            "trap 'exit 42' TERM; echo ready; sleep 30 & wait",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_ready(&mut child);
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(child.wait().unwrap().code(), Some(42));
}

// The container must not inherit blocked or ignored signals from openpod (Rust ignores SIGPIPE).
#[test]
fn container_starts_with_default_signal_handling() {
    let out = Sandbox::new().run(&["grep", "-E", "^Sig(Blk|Ign):", "/proc/self/status"]);
    assert_eq!(
        stdout(&out),
        "SigBlk:\t0000000000000000\nSigIgn:\t0000000000000000",
        "{}",
        stderr(&out)
    );
}

// `kill -INT <cli>` comes from a process, so it is forwarded like `docker run` does.
// Ctrl+C from a terminal is different: the terminal already sends it to the container
// directly, so openpod must not forward that copy (see signals::forward_to).
#[test]
fn sigint_sent_to_the_cli_reaches_the_container() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let mut child = sb
        .openpod()
        .args([
            "run",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            "trap 'exit 43' INT; echo ready; sleep 30 & wait",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_ready(&mut child);
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(child.wait().unwrap().code(), Some(43));
}

// Every "fix" line must be something the user can do now; `openpod check` does not exist yet.
#[test]
fn failed_setup_gives_advice_that_works() {
    let out = Sandbox::new().run_with(&["-w", "/proc/openpod-test"], &["true"]);
    assert_eq!(out.status.code(), Some(125));
    let err = stderr(&out);
    assert!(
        err.starts_with("openpod: cannot enter the working directory /proc/openpod-test\n"),
        "{err}"
    );
    assert!(err.contains("\n  fix:   "), "{err}");
    assert!(!err.contains("openpod check"), "{err}");
}

fn run_with_stdin(sb: &Sandbox, options: &[&str], input: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = sb
        .openpod()
        .args(["run", "--rm"])
        .args(options)
        .args(["--rootfs", &rootfs(), "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

// Like Docker: without -i the container's stdin is empty.
#[test]
fn stdin_is_not_attached_without_i() {
    let out = run_with_stdin(&Sandbox::new(), &[], "hi\n");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}

#[test]
fn stdin_is_attached_with_i() {
    let out = run_with_stdin(&Sandbox::new(), &["-i"], "hi\n");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "hi");
}
