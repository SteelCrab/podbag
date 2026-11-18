#![cfg(target_os = "linux")]
//! The keeper (design 6.15): one process per runtime directory starts containers for the
//! CLI and watches the detached ones.

mod common;
use common::*;
use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A sandbox whose containers go through the keeper, which leaves `linger_ms` after its
/// last container.
fn kept(linger_ms: u32) -> Sandbox {
    let mut sb = Sandbox::with_env("OPENPOD_KEEPER", "1");
    sb.env
        .push(("OPENPOD_KEEPER_LINGER_MS".into(), linger_ms.to_string()));
    sb
}

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

fn wait_for(limit: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// (keepers, shims) of this sandbox: processes named `openpod` that run `__keeper` for its
/// runtime directory, and `openpod-shim` or `openpod __shim` processes of its containers.
fn processes(sb: &Sandbox) -> (usize, usize) {
    let out = Command::new("ps")
        .args(["-eo", "comm=,args="])
        .output()
        .unwrap();
    let runtime = sb.runtime().display().to_string();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mine: Vec<&str> = text.lines().filter(|l| l.contains(&runtime)).collect();
    let keepers = mine
        .iter()
        .filter(|l| l.split_whitespace().next() == Some("openpod") && l.contains("__keeper"))
        .count();
    let shims = mine
        .iter()
        .filter(|l| l.split_whitespace().next() == Some("openpod-shim"))
        .count();
    (keepers, shims)
}

#[test]
fn a_foreground_run_goes_through_the_keeper() {
    let sb = kept(300);
    let out = sb.run(&["sh", "-c", "echo out; echo err >&2; exit 3"]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "out");
    assert_eq!(stderr(&out).trim(), "err");
    // While a foreground container runs: one keeper, and its starter serves as the shim.
    let mut long = sb
        .openpod()
        .args(["run", "--rm", "--rootfs", &rootfs(), "sleep", "300"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        wait_for(Duration::from_secs(10), || processes(&sb) == (1, 1)),
        "keepers and shims: {:?}",
        processes(&sb)
    );
    // The starter shows before the container counts as running.
    let running = || stdout(&sb.openpod().args(["ps", "-q"]).output().unwrap());
    assert!(wait_for(Duration::from_secs(10), || !running().is_empty()));
    // It removes itself when it ends (--rm): end it.
    ok(&sb
        .openpod()
        .args(["kill", running().trim()])
        .output()
        .unwrap());
    let _ = long.wait();
}

#[test]
fn stdin_reaches_a_kept_foreground_container() {
    let sb = kept(300);
    let mut child = sb
        .openpod()
        .args(["run", "-i", "--rm", "--rootfs", &rootfs(), "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hi\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(ok(&out), "hi");
}

#[test]
fn a_terminal_works_through_the_keeper() {
    let sb = kept(300);
    let openpod = if root_mode() {
        format!(
            "sudo -n env OPENPOD_KEEPER=1 OPENPOD_KEEPER_LINGER_MS=300 OPENPOD_DATA_DIR={} OPENPOD_RUNTIME_DIR={} {}",
            sb.data().display(),
            sb.runtime().display(),
            BIN
        )
    } else {
        format!(
            "env OPENPOD_KEEPER=1 OPENPOD_KEEPER_LINGER_MS=300 OPENPOD_DATA_DIR={} OPENPOD_RUNTIME_DIR={} {}",
            sb.data().display(),
            sb.runtime().display(),
            BIN
        )
    };
    let line = format!(
        "{openpod} run -it --rm --rootfs {} sh -c 'stty size; echo tty-ok'",
        rootfs()
    );
    let mut child = Command::new("script")
        .args(["-qec", &line, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let mut text = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let status = child.wait().unwrap();
    drop(stdin);
    assert!(text.contains("tty-ok"), "{text}");
    assert_eq!(status.code(), Some(0), "{text}");
}

#[test]
fn signals_reach_a_kept_foreground_container() {
    let sb = kept(300);
    // A shell with a handler: as PID 1 it gets only the signals it handles.
    let script = "trap 'exit 0' TERM; echo ready; while true; do sleep 0.1; done";
    let mut child = sb
        .openpod()
        .args(["run", "--rm", "--rootfs", &rootfs(), "sh", "-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut ready = [0u8; 6];
    out.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready\n");
    // In root mode the child is sudo, which passes the signal on to openpod.
    // SAFETY: a signal to our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn the_keeper_leaves_after_its_linger() {
    let sb = kept(300);
    ok(&sb.run(&["true"]));
    assert!(
        wait_for(Duration::from_secs(5), || processes(&sb) == (0, 0)),
        "left behind: {:?}",
        processes(&sb)
    );
    assert!(
        !sb.runtime().join("keeper.sock").exists(),
        "the socket stayed"
    );
    // And the next run starts a new one.
    ok(&sb.run(&["true"]));
}

#[test]
fn parallel_first_starts_share_one_keeper() {
    let sb = kept(2000);
    std::thread::scope(|scope| {
        let runs: Vec<_> = (0..8)
            .map(|n| {
                let sb = &sb;
                scope.spawn(move || sb.run(&["sh", "-c", &format!("echo {n}")]))
            })
            .collect();
        for (n, run) in runs.into_iter().enumerate() {
            let out = run.join().unwrap();
            assert_eq!(ok(&out), n.to_string());
        }
    });
    assert_eq!(processes(&sb).0, 1, "one keeper serves them all");
}

#[test]
fn a_start_that_meets_a_leaving_keeper_still_runs() {
    // No linger: the keeper leaves as soon as each container is gone.
    let sb = kept(0);
    let begun = Instant::now();
    for round in 0..20 {
        let out = sb.run(&["sh", "-c", "echo again"]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "round {round}: {}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), "again", "round {round}");
    }
    // A client that waits out a keeper that never answers takes seconds per run.
    assert!(
        begun.elapsed() < Duration::from_secs(20),
        "20 runs took {:?}",
        begun.elapsed()
    );
}

#[test]
fn the_keeper_is_used_when_nothing_says_otherwise() {
    if std::env::var("OPENPOD_TEST_KEEPER").as_deref() == Ok("0") {
        return; // this run of the suite turns the keeper off
    }
    let sb = Sandbox::new();
    op(
        &sb,
        &[
            "run",
            "-d",
            "--name",
            "plain",
            "--rootfs",
            &rootfs(),
            "sleep",
            "300",
        ],
    );
    assert_eq!(processes(&sb), (1, 0), "one keeper, no shim");
    op(&sb, &["rm", "-f", "plain"]);
}

#[test]
fn without_the_switch_there_is_no_keeper() {
    let sb = Sandbox::with_env("OPENPOD_KEEPER", "0");
    let mut long = sb
        .openpod()
        .args(["run", "--rm", "--rootfs", &rootfs(), "sleep", "300"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_for(Duration::from_secs(10), || {
        !stdout(&sb.openpod().args(["ps", "-q"]).output().unwrap()).is_empty()
    }));
    assert_eq!(processes(&sb).0, 0, "a keeper appeared");
    let ids = ok(&sb.openpod().args(["ps", "-q"]).output().unwrap());
    ok(&sb.openpod().args(["kill", ids.trim()]).output().unwrap());
    let _ = long.wait();
}

// ---- Detached containers: watched by the keeper, no process of their own (Task 2) ----

/// `openpod inspect <name>` as JSON.
fn inspect(sb: &Sandbox, name: &str) -> serde_json::Value {
    let out = sb.openpod().args(["inspect", name]).output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

fn status_of(sb: &Sandbox, name: &str) -> String {
    inspect(sb, name)["state"]["status"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn exited_with(sb: &Sandbox, name: &str, limit: Duration) -> Option<i64> {
    wait_for(limit, || status_of(sb, name) == "exited").then(|| {
        inspect(sb, name)["state"]["exit_code"]
            .as_i64()
            .unwrap_or(-1)
    })
}

fn op(sb: &Sandbox, args: &[&str]) -> String {
    ok(&sb.openpod().args(args).output().unwrap())
}

fn detached(sb: &Sandbox, name: &str, script: &str) {
    op(
        sb,
        &[
            "run",
            "-d",
            "--name",
            name,
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            script,
        ],
    );
}

fn logs(sb: &Sandbox, name: &str) -> (String, String) {
    let out = sb.openpod().args(["logs", name]).output().unwrap();
    (stdout(&out), stderr(&out))
}

#[test]
fn a_detached_container_has_no_process_of_its_own() {
    let sb = kept(300);
    for name in ["a", "b", "c"] {
        detached(&sb, name, "sleep 300");
    }
    assert_eq!(op(&sb, &["ps", "-q"]).lines().count(), 3);
    assert_eq!(processes(&sb), (1, 0), "one keeper, no shim");
    op(&sb, &["rm", "-f", "a", "b", "c"]);
    assert!(
        wait_for(Duration::from_secs(5), || processes(&sb) == (0, 0)),
        "the keeper stayed: {:?}",
        processes(&sb)
    );
}

#[test]
fn logs_stop_kill_work_on_kept_containers() {
    let sb = kept(300);
    let script = "echo one; echo two >&2; trap 'echo got-usr1' USR1; trap 'exit 5' TERM; \
                  while true; do sleep 0.1; done";
    detached(&sb, "lg", script);
    assert!(wait_for(Duration::from_secs(5), || {
        let (out, err) = logs(&sb, "lg");
        out.contains("one") && err.contains("two")
    }));
    op(&sb, &["kill", "-s", "USR1", "lg"]);
    assert!(wait_for(Duration::from_secs(5), || logs(&sb, "lg")
        .0
        .contains("got-usr1")));
    std::thread::scope(|scope| {
        let follower = scope.spawn(|| sb.openpod().args(["logs", "-f", "lg"]).output().unwrap());
        std::thread::sleep(Duration::from_millis(300));
        op(&sb, &["stop", "-t", "5", "lg"]);
        let followed = follower.join().unwrap();
        assert!(
            stdout(&followed).contains("got-usr1"),
            "{}",
            stdout(&followed)
        );
    });
    assert_eq!(exited_with(&sb, "lg", Duration::from_secs(5)), Some(5));
    assert_eq!(processes(&sb).1, 0);
}

#[test]
fn a_container_that_exits_at_once_keeps_its_code_and_output() {
    let sb = kept(300);
    for n in 0..10 {
        let name = format!("q{n}");
        detached(&sb, &name, "echo quick; exit 7");
        assert_eq!(
            exited_with(&sb, &name, Duration::from_secs(5)),
            Some(7),
            "{name}"
        );
        assert_eq!(logs(&sb, &name).0, "quick", "{name}");
    }
}

#[test]
fn rm_on_exit_removes_a_kept_container() {
    let sb = kept(300);
    op(
        &sb,
        &[
            "run",
            "-d",
            "--rm",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            "exit 0",
        ],
    );
    assert!(
        wait_for(Duration::from_secs(5), || op(&sb, &["ps", "-aq"])
            .is_empty()),
        "the container stayed"
    );
}

#[test]
fn a_kept_terminal_can_be_resized() {
    if root_mode() {
        return; // the test user cannot reach root's control socket; the code is the same
    }
    let sb = kept(300);
    let id = op(
        &sb,
        &[
            "run",
            "-d",
            "-t",
            "--name",
            "term",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            "while true; do stty size; sleep 0.2; done",
        ],
    );
    let socket = sb
        .runtime()
        .join("containers")
        .join(id.trim())
        .join("shim.sock");
    let mut conn = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    conn.write_all(b"{\"op\":\"resize\",\"rows\":30,\"cols\":100}\n")
        .unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    assert!(answer.contains("\"ok\":true"), "{answer}");
    assert!(
        wait_for(Duration::from_secs(5), || logs(&sb, "term")
            .0
            .contains("30 100")),
        "{}",
        logs(&sb, "term").0
    );
    op(&sb, &["rm", "-f", "term"]);
}

#[test]
fn restart_of_a_kept_container() {
    let sb = kept(300);
    detached(&sb, "r", "echo run; sleep 300");
    op(&sb, &["restart", "-t", "1", "r"]);
    assert_eq!(status_of(&sb, "r"), "running");
    assert!(wait_for(Duration::from_secs(5), || logs(&sb, "r").0 == "run\nrun"));
    assert_eq!(processes(&sb), (1, 0));
    op(&sb, &["rm", "-f", "r"]);
}

#[test]
fn a_grandchild_holding_the_pipe_does_not_block_the_exit() {
    let sb = kept(300);
    detached(&sb, "g", "sleep 30 & exit 4");
    assert_eq!(exited_with(&sb, "g", Duration::from_secs(6)), Some(4));
    op(&sb, &["rm", "-f", "g"]);
}

#[test]
fn output_bursts_rotate_the_log() {
    let sb = kept(300);
    let script = "i=0; while [ $i -lt 90000 ]; do \
                  echo \"line $i 0123456789012345678901234567890123456789\"; i=$((i+1)); done; \
                  echo last-line";
    let id = op(
        &sb,
        &[
            "run",
            "-d",
            "--name",
            "burst",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            script,
        ],
    );
    assert_eq!(exited_with(&sb, "burst", Duration::from_secs(60)), Some(0));
    let (out, _) = logs(&sb, "burst");
    assert_eq!(out.lines().last(), Some("last-line"));
    let rotated = sb
        .data()
        .join("containers")
        .join(id.trim())
        .join("log.json.1");
    let exists = if root_mode() {
        Command::new("sudo")
            .args(["-n", "test", "-e"])
            .arg(&rotated)
            .status()
            .unwrap()
            .success()
    } else {
        rotated.exists()
    };
    assert!(exists, "the log did not rotate");
    // The keeper is still well after the burst.
    detached(&sb, "after", "echo fine");
    assert_eq!(exited_with(&sb, "after", Duration::from_secs(5)), Some(0));
}

#[test]
fn an_attached_container_does_not_keep_others_locks() {
    let sb = kept(300);
    detached(&sb, "b", "sleep 300");
    // A foreground container starts after b was adopted: its starter is a fork of a keeper
    // that holds b's lock.
    let mut front = sb
        .openpod()
        .args([
            "run",
            "--rm",
            "--name",
            "front",
            "--rootfs",
            &rootfs(),
            "sleep",
            "300",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_for(Duration::from_secs(10), || status_of(
        &sb, "front"
    ) == "running"));
    op(&sb, &["stop", "-t", "1", "b"]);
    let begun = Instant::now();
    op(&sb, &["start", "b"]);
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(status_of(&sb, "b"), "running");
    op(&sb, &["rm", "-f", "b", "front"]);
    let _ = front.wait();
}

#[test]
fn the_container_inherits_no_keeper_descriptors() {
    let sb = kept(300);
    detached(&sb, "other", "sleep 300");
    detached(&sb, "fds", "ls /proc/self/fd");
    assert_eq!(exited_with(&sb, "fds", Duration::from_secs(5)), Some(0));
    let (listing, _) = logs(&sb, "fds");
    // 0, 1, 2, and the descriptor `ls` reads the directory with.
    assert_eq!(listing.lines().collect::<Vec<_>>(), ["0", "1", "2", "3"]);
    op(&sb, &["rm", "-f", "other", "fds"]);
}

fn keeper_pid(sb: &Sandbox) -> Option<i32> {
    let out = Command::new("ps")
        .args(["-eo", "pid=,comm=,args="])
        .output()
        .unwrap();
    let runtime = sb.runtime().display().to_string();
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let pid = f.next()?.parse().ok()?;
        (f.next()? == "openpod" && l.contains("__keeper") && l.contains(&runtime)).then_some(pid)
    })
}

fn signal(pid: i32, sig: &str) {
    let mut cmd = if root_mode() {
        let mut c = Command::new("sudo");
        c.args(["-n", "kill"]);
        c
    } else {
        Command::new("kill")
    };
    assert!(
        cmd.args(["-s", sig, &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn a_killed_keeper_ends_its_containers_and_the_next_start_recovers() {
    let sb = kept(300);
    detached(&sb, "k", "sleep 300");
    let container_pid = inspect(&sb, "k")["state"]["pid"].as_i64().unwrap();
    signal(keeper_pid(&sb).unwrap(), "KILL");
    assert!(
        wait_for(Duration::from_secs(5), || !std::path::Path::new(&format!(
            "/proc/{container_pid}"
        ))
        .exists()),
        "the container outlived its keeper"
    );
    assert_eq!(status_of(&sb, "k"), "exited");
    detached(&sb, "k2", "sleep 300");
    assert_eq!(status_of(&sb, "k2"), "running");
    assert_eq!(processes(&sb), (1, 0));
    op(&sb, &["rm", "-f", "k", "k2"]);
}

#[test]
fn the_keeper_stops_its_containers_on_sigterm() {
    let sb = kept(300);
    detached(
        &sb,
        "s",
        "trap 'exit 0' TERM; echo ready; while true; do sleep 0.1; done",
    );
    assert!(wait_for(Duration::from_secs(5), || logs(&sb, "s")
        .0
        .contains("ready")));
    signal(keeper_pid(&sb).unwrap(), "TERM");
    assert_eq!(exited_with(&sb, "s", Duration::from_secs(10)), Some(0));
    assert!(wait_for(Duration::from_secs(5), || processes(&sb) == (0, 0)));
}

#[test]
fn published_ports_keep_their_shim() {
    let sb = kept(300);
    op(
        &sb,
        &[
            "run",
            "-d",
            "-p",
            "80",
            "--name",
            "p",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            "while true; do echo hi | nc -l -p 80; done",
        ],
    );
    assert_eq!(processes(&sb), (1, 1), "its relays live in its shim");
    op(&sb, &["rm", "-f", "p"]);
}

#[test]
fn a_quick_restart_gets_its_network_again() {
    // The kernel takes a container's network namespace down a moment after its last
    // process: a start right after a stop meets the old veth device.
    let sb = kept(300);
    detached(&sb, "n", "sleep 300");
    for round in 0..10 {
        op(&sb, &["stop", "-t", "1", "n"]);
        let out = sb.openpod().args(["start", "n"]).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "round {round}: {}",
            stderr(&out)
        );
    }
    op(&sb, &["rm", "-f", "n"]);
}

// ---- Cgroups below the keeper, and removal without a user namespace (Task 3) ----

fn cgroup_of(sb: &Sandbox, name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(inspect(sb, name)["state"]["cgroup"].as_str().unwrap_or(""))
}

fn read_cgroup(dir: &std::path::Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file))
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[test]
fn limits_apply_under_the_keeper() {
    let sb = kept(300);
    op(
        &sb,
        &[
            "run",
            "-d",
            "--name",
            "lim",
            "-m",
            "32m",
            "--pids-limit",
            "10",
            "--rootfs",
            &rootfs(),
            "sleep",
            "300",
        ],
    );
    let group = cgroup_of(&sb, "lim");
    assert_eq!(read_cgroup(&group, "memory.max"), (32u64 << 20).to_string());
    assert_eq!(read_cgroup(&group, "pids.max"), "10");
    assert_eq!(processes(&sb), (1, 0));
    // A container that goes over its limit is killed, and the keeper's finisher says why.
    op(
        &sb,
        &[
            "run",
            "-d",
            "--name",
            "hungry",
            "-m",
            "32m",
            "--rootfs",
            &rootfs(),
            "dd",
            "if=/dev/zero",
            "of=/dev/null",
            "bs=64M",
            "count=1",
        ],
    );
    assert_eq!(
        exited_with(&sb, "hungry", Duration::from_secs(20)),
        Some(137)
    );
    assert_eq!(inspect(&sb, "hungry")["state"]["oom"], "container_limit");
    op(&sb, &["rm", "-f", "lim", "hungry"]);
}

#[test]
fn no_scope_per_container() {
    if root_mode() {
        return; // root mode never used systemd scopes
    }
    let sb = kept(300);
    detached(&sb, "one", "sleep 300");
    detached(&sb, "two", "sleep 300");
    let (one, two) = (cgroup_of(&sb, "one"), cgroup_of(&sb, "two"));
    // Both sit in one scope: <scope>/c/<id>.
    assert_eq!(one.parent(), two.parent(), "{one:?} {two:?}");
    let parent = one.parent().unwrap();
    assert_eq!(parent.file_name().unwrap(), "c");
    let scope = parent.parent().unwrap();
    assert!(scope.extension().is_some_and(|e| e == "scope"), "{scope:?}");
    assert!(
        !one.to_string_lossy().contains("openpod-"),
        "a scope of its own: {one:?}"
    );
    op(&sb, &["rm", "-f", "one", "two"]);
    // The keeper's scope goes with the keeper.
    assert!(
        wait_for(Duration::from_secs(8), || !scope.exists()),
        "{scope:?} stayed"
    );
}

#[test]
fn the_budget_stays_on_the_slice() {
    if root_mode() {
        return; // as root the budget sits on the parent group, as before (tests/limits.rs)
    }
    // A parent of its own: the budget must not reach other tests' containers.
    let parent = format!("openpodkeptbudget{}", std::process::id());
    let slice_name = format!("{parent}.slice");
    let mut sb = kept(300);
    sb.env
        .push(("OPENPOD_CGROUP_PARENT".into(), parent.clone()));
    sb.env
        .push(("OPENPOD_MEMORY_BUDGET".into(), "64MiB".into()));
    detached(&sb, "b", "sleep 300");
    let group = cgroup_of(&sb, "b");
    let slice = group
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == slice_name.as_str()))
        .unwrap_or_else(|| panic!("no {slice_name} above {group:?}"))
        .to_path_buf();
    // One budget for everything in the slice, as without the keeper.
    assert_eq!(read_cgroup(&slice, "memory.max"), (64u64 << 20).to_string());
    assert_eq!(read_cgroup(group.parent().unwrap(), "memory.max"), "max");
    op(&sb, &["rm", "-f", "b"]);
    let _ = Command::new("systemctl")
        .args(["--user", "stop", &slice_name])
        .status();
}

#[test]
fn a_removed_container_needs_no_user_namespace_when_it_wrote_nothing() {
    if root_mode() {
        return; // root needs no user namespace at all
    }
    // newuidmap and newgidmap wrappers that count their runs.
    let mut sb = kept(300);
    let bin = sb.tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let count = sb.tmp.path().join("idmap-runs");
    for tool in ["newuidmap", "newgidmap"] {
        let script = format!(
            "#!/bin/sh\necho {tool} >> {}\nexec /usr/bin/{tool} \"$@\"\n",
            count.display()
        );
        let path = bin.join(tool);
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    sb.env.push(("PATH".into(), path));
    // Without a network the container gets a user namespace of its own (one pair).
    ok(&sb.run_with(&["--network", "none"], &["true"]));
    let runs = std::fs::read_to_string(&count).unwrap_or_default();
    // The two run side by side: their order is not fixed.
    let mut runs: Vec<&str> = runs.lines().collect();
    runs.sort_unstable();
    assert_eq!(
        runs,
        ["newgidmap", "newuidmap"],
        "one pair for the container, none for its removal"
    );
    // Files of other container users still need the namespace:
    // container_files_owned_by_other_users_are_cleaned_up (tests/image_pull.rs).
    assert!(op(&sb, &["ps", "-aq"]).is_empty());
}

#[test]
fn id_map_helpers_run_side_by_side() {
    if root_mode() {
        return; // root needs no ID maps
    }
    // Slow wrappers that note when they start and end: run one after the other, the first
    // ends before the second starts.
    let mut sb = Sandbox::new();
    let bin = sb.tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let trace = sb.tmp.path().join("idmap-trace");
    for tool in ["newuidmap", "newgidmap"] {
        let script = format!(
            "#!/bin/sh\necho start >> {0}\nsleep 0.3\n/usr/bin/{tool} \"$@\"\ncode=$?\necho end >> {0}\nexit $code\n",
            trace.display()
        );
        let path = bin.join(tool);
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    sb.env.push(("PATH".into(), path));
    sb.env.push(("OPENPOD_KEEPER".into(), "0".into()));
    ok(&sb.run_with(&["--network", "none"], &["true"]));
    let lines = std::fs::read_to_string(&trace).unwrap_or_default();
    let lines: Vec<&str> = lines.lines().collect();
    assert_eq!(lines.get(..2), Some(&["start", "start"][..]), "{lines:?}");
}

// ---- Findings of the final review ----

/// Shim processes that a client started itself for container `id` (not the keeper's).
fn own_shims(id: &str) -> usize {
    let out = Command::new("ps")
        .args(["-eo", "comm=,args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.split_whitespace().next() == Some("openpod"))
        .filter(|l| l.contains("__shim") && l.contains(id))
        .count()
}

/// The full ID of the container `name`.
fn id_of(sb: &Sandbox, name: &str) -> String {
    inspect(sb, name)["id"].as_str().unwrap_or("").to_string()
}

/// Connect to the socket `name` in `dir` (a path too long for a socket address goes
/// through an open descriptor of the directory).
fn connect_in(dir: &std::path::Path, name: &str) -> std::os::unix::net::UnixStream {
    use std::os::fd::AsRawFd;
    let handle = std::fs::File::open(dir).unwrap();
    let short = format!("/proc/self/fd/{}/{name}", handle.as_raw_fd());
    std::os::unix::net::UnixStream::connect(short).unwrap()
}

/// How long `run --rm true` takes in this sandbox.
fn time_a_run(sb: &Sandbox) -> Duration {
    let started = Instant::now();
    ok(&sb.run(&["true"]));
    started.elapsed()
}

#[test]
fn a_killed_keeper_ends_a_foreground_container() {
    let sb = kept(300);
    let mut run = sb
        .openpod()
        .args(["run", "--rm", "--rootfs", &rootfs(), "sleep", "300"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(wait_for(Duration::from_secs(10), || {
        !stdout(&sb.openpod().args(["ps", "-q"]).output().unwrap()).is_empty()
    }));
    let id = op(&sb, &["ps", "-q"]);
    let init = inspect(&sb, id.trim())["state"]["pid"].as_u64().unwrap();
    signal(keeper_pid(&sb).expect("a keeper"), "KILL");
    // The command ends with an error ...
    let mut code = None;
    assert!(
        wait_for(Duration::from_secs(10), || {
            code = run.try_wait().unwrap();
            code.is_some()
        }),
        "the run went on without its keeper"
    );
    assert_ne!(code.unwrap().code(), Some(0));
    let mut err = String::new();
    run.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(err.contains("keeper"), "{err}");
    // ... and nothing of the container goes on: not its process, not its helper.
    assert!(
        wait_for(Duration::from_secs(5), || {
            !std::path::Path::new(&format!("/proc/{init}")).exists()
        }),
        "the container still runs"
    );
    assert!(
        wait_for(Duration::from_secs(5), || processes(&sb) == (0, 0)),
        "{:?}",
        processes(&sb)
    );
}

#[test]
fn another_build_does_not_use_this_keeper() {
    let sb = kept(300);
    detached(&sb, "old", "sleep 300");
    assert!(wait_for(Duration::from_secs(5), || processes(&sb) == (1, 0)));
    // The same program in another file: another build, as far as the keeper can tell.
    // (Below the target directory, where the VM's AppArmor profile allows openpod.)
    let dir = std::path::Path::new(BIN)
        .parent()
        .unwrap()
        .join(format!("keeper-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("openpod");
    std::fs::copy(BIN, &copy).unwrap();
    let out = sb
        .openpod_bin(copy.to_str().unwrap())
        .args([
            "run",
            "-d",
            "--name",
            "new",
            "--rootfs",
            &rootfs(),
            "sleep",
            "300",
        ])
        .output()
        .unwrap();
    let started = out.status.code();
    let err = stderr(&out);
    let new = id_of(&sb, "new");
    // The other build watches its container itself; the keeper keeps the one it has.
    let shims = wait_for(Duration::from_secs(5), || own_shims(&new) == 1);
    op(&sb, &["rm", "-f", "old", "new"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(started, Some(0), "{err}");
    assert!(shims, "the other build's container has no shim of its own");
}

#[test]
fn silent_connections_do_not_stall_the_keeper() {
    if root_mode() {
        return; // the test cannot reach root's sockets
    }
    let sb = kept(300);
    detached(&sb, "a", "sleep 300");
    let a = id_of(&sb, "a");
    // Connections that say nothing: to the keeper, and to a container's control socket.
    let quiet: Vec<_> = (0..3)
        .map(|_| connect_in(&sb.runtime(), "keeper.sock"))
        .chain((0..3).map(|_| connect_in(&sb.runtime().join("containers").join(&a), "shim.sock")))
        .collect();
    // Others are served meanwhile.
    let took = time_a_run(&sb);
    assert!(took < Duration::from_millis(1500), "a start took {took:?}");
    // A request cut in the middle is no better.
    let mut half = connect_in(&sb.runtime().join("containers").join(&a), "shim.sock");
    half.write_all(br#"{"op":"ki"#).unwrap();
    let mut cut = connect_in(&sb.runtime(), "keeper.sock");
    cut.write_all(&[0, 0, 1]).unwrap();
    let took = time_a_run(&sb);
    assert!(took < Duration::from_millis(1500), "a start took {took:?}");
    drop((quiet, half, cut));
    op(&sb, &["rm", "-f", "a"]);
}

#[test]
fn a_request_without_an_end_is_cut_off() {
    if root_mode() {
        return; // the test cannot reach root's sockets
    }
    let sb = kept(300);
    detached(&sb, "a", "sleep 300");
    let a = id_of(&sb, "a");
    let mut conn = connect_in(&sb.runtime().join("containers").join(&a), "shim.sock");
    conn.set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    let mut sent = 0usize;
    // 8 MiB without a line end: the keeper must not take it all in.
    while sent < 8 << 20 {
        match conn.write(&chunk) {
            Ok(n) if n > 0 => sent += n,
            _ => break,
        }
    }
    assert!(
        sent < 1 << 20,
        "the keeper took {sent} bytes of one request"
    );
    // It still serves.
    assert_eq!(status_of(&sb, "a"), "running");
    assert!(time_a_run(&sb) < Duration::from_millis(1500));
    op(&sb, &["rm", "-f", "a"]);
}

#[test]
fn a_runtime_directory_of_others_gets_no_keeper() {
    let sb = kept(300);
    // Made by the test user and writable by everybody: for openpod as root it belongs to
    // another user; without root, others could put their own socket there.
    std::fs::create_dir_all(sb.runtime()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(sb.runtime(), std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    detached(&sb, "a", "sleep 300");
    let a = id_of(&sb, "a");
    let shims = wait_for(Duration::from_secs(5), || own_shims(&a) == 1);
    let keepers = processes(&sb).0;
    op(&sb, &["rm", "-f", "a"]);
    assert_eq!(
        keepers, 0,
        "a keeper serves a directory that is not private"
    );
    assert!(shims, "the container has no shim of its own");
}

#[test]
fn the_keeper_gets_only_the_settings_it_needs() {
    if root_mode() {
        return; // the test cannot read a root process's environment
    }
    let mut sb = kept(300);
    sb.env
        .push(("MY_SECRET_TOKEN".into(), "do-not-pass-on".into()));
    let mut run = sb
        .openpod()
        .args(["run", "--rm", "--rootfs", &rootfs(), "sleep", "300"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_for(Duration::from_secs(10), || processes(&sb) == (1, 1)));
    let runtime = sb.runtime().display().to_string();
    // The starter that serves as this container's helper, and the keeper itself.
    let out = Command::new("ps")
        .args(["-eo", "pid=,comm=,args="])
        .output()
        .unwrap();
    let pids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(&runtime))
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .collect();
    assert!(pids.len() >= 2, "{pids:?}");
    let ids = op(&sb, &["ps", "-q"]);
    // (A helper that has changed its credentials does not show its environment.)
    let mut readable = 0;
    for pid in &pids {
        let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        let environ = String::from_utf8_lossy(&environ).to_string();
        readable += usize::from(environ.contains("OPENPOD_RUNTIME_DIR="));
        assert!(
            !environ.contains("MY_SECRET_TOKEN"),
            "{pid} holds the secret"
        );
    }
    assert!(readable >= 1, "no environment could be read: {pids:?}");
    op(&sb, &["kill", ids.trim()]);
    let _ = run.wait();
}

#[test]
fn the_root_keeper_has_a_group_of_its_own() {
    if !root_mode() {
        return; // without root the keeper has a systemd scope (no_scope_per_container)
    }
    let sb = kept(300);
    detached(&sb, "a", "sleep 300");
    let pid = keeper_pid(&sb).expect("a keeper");
    let group = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    // Not the group of whoever happened to start the first container (this test's
    // session): that one may be stopped while other callers' containers run.
    assert_eq!(group.trim(), "0::/openpod.keeper");
    op(&sb, &["rm", "-f", "a"]);
}

#[test]
fn a_keeper_without_cgroups_leaves_limits_to_a_shim() {
    if root_mode() {
        return; // as root the keeper needs no scope
    }
    // The first client finds no systemd-run: its keeper has no scope to make cgroups in.
    let mut first = kept(10_000);
    first
        .env
        .push(("OPENPOD_SYSTEMD_RUN".into(), "/nonexistent".into()));
    detached(&first, "plain", "sleep 300");
    assert!(wait_for(Duration::from_secs(5), || processes(&first).0 == 1));
    // A second client of the same directories has one, and asks for a limit.
    let mut cmd = first.openpod();
    cmd.env_remove("OPENPOD_SYSTEMD_RUN").args([
        "run",
        "-d",
        "--name",
        "limited",
        "-m",
        "32m",
        "--rootfs",
        &rootfs(),
        "sleep",
        "300",
    ]);
    let out = cmd.output().unwrap();
    let started = out.status.code();
    let err = stderr(&out);
    let group = cgroup_of(&first, "limited");
    let limit = read_cgroup(&group, "memory.max");
    op(&first, &["rm", "-f", "plain", "limited"]);
    assert_eq!(started, Some(0), "{err}");
    assert_eq!(limit, (32u64 << 20).to_string(), "{group:?}");
}

#[test]
fn no_process_between_probes() {
    let sb = kept(300);
    let id = op(
        &sb,
        &[
            "run",
            "-d",
            "--name",
            "h",
            "--health-cmd",
            "true",
            "--health-interval",
            "2s",
            "--rootfs",
            &rootfs(),
            "sleep",
            "300",
        ],
    );
    let id = id.trim().to_string();
    assert!(wait_for(Duration::from_secs(15), || {
        inspect(&sb, "h")["health"]["status"] == "healthy"
    }));
    // Between its checks the container costs no process: a probe lives for a moment.
    let probes = || {
        let out = Command::new("ps").args(["-eo", "args="]).output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("__health") && l.contains(&id))
            .count()
    };
    let samples = 60;
    let mut seen = 0;
    for _ in 0..samples {
        if probes() > 0 {
            seen += 1;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        seen * 4 <= samples,
        "a probe ran in {seen} of {samples} samples"
    );
    assert_eq!(processes(&sb), (1, 0), "one keeper, no shim");
    op(&sb, &["rm", "-f", "h"]);
}
