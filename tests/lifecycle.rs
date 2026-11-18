#![cfg(target_os = "linux")]
//! Container life: background runs, state, logs, names, removal, stop, kill, exec, terminals.

mod common;
use common::*;
use std::time::{Duration, Instant};

fn openpod(sb: &Sandbox, args: &[&str]) -> std::process::Output {
    sb.openpod().args(args).output().unwrap()
}

fn ok(out: &std::process::Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

/// `run -d --rootfs <alpine> <options...> <command...>`; returns the container ID.
fn start(sb: &Sandbox, options: &[&str], command: &[&str]) -> String {
    let mut args = vec!["run", "-d"];
    args.extend_from_slice(options);
    let rootfs = rootfs();
    args.extend_from_slice(&["--rootfs", &rootfs]);
    args.extend_from_slice(command);
    ok(&openpod(sb, &args))
}

fn inspect(sb: &Sandbox, c: &str) -> serde_json::Value {
    serde_json::from_str(&ok(&openpod(sb, &["inspect", c]))).unwrap()
}

fn wait_for_status(sb: &Sandbox, c: &str, status: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let info = inspect(sb, c);
        if info["state"]["status"] == status {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "container never became {status}: {info}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn detached_run_prints_the_id_and_keeps_running() {
    let sb = Sandbox::new();
    let t = Instant::now();
    let id = start(&sb, &[], &["sleep", "3"]);
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "run -d waited for the container"
    );
    assert_eq!(id.len(), 64, "{id}");
    let info = inspect(&sb, &id);
    assert_eq!(info["state"]["status"], "running", "{info}");
    assert_eq!(info["name"], id[..12]);
    let info = wait_for_status(&sb, &id, "exited");
    assert_eq!(info["state"]["exit_code"], 0);
}

#[test]
fn logs_show_detached_output_in_order() {
    let sb = Sandbox::new();
    let id = start(
        &sb,
        &[],
        &[
            "sh",
            "-c",
            "echo one; sleep 0.2; echo two >&2; sleep 0.2; echo three",
        ],
    );
    wait_for_status(&sb, &id, "exited");
    let out = openpod(&sb, &["logs", &id]);
    assert_eq!(stdout(&out), "one\nthree");
    assert_eq!(stderr(&out).trim(), "two");
}

#[test]
fn foreground_output_is_also_logged() {
    let sb = Sandbox::new();
    assert_eq!(
        ok(&openpod(
            &sb,
            &["run", "--name", "fg", "--rootfs", &rootfs(), "echo", "hi"]
        )),
        "hi"
    );
    assert_eq!(ok(&openpod(&sb, &["logs", "fg"])), "hi");
}

#[test]
fn exited_container_stays_without_rm() {
    let sb = Sandbox::new();
    let out = openpod(
        &sb,
        &[
            "run",
            "--name",
            "three",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            "exit 3",
        ],
    );
    assert_eq!(out.status.code(), Some(3));
    let info = inspect(&sb, "three");
    assert_eq!(
        (
            info["state"]["status"].clone(),
            info["state"]["exit_code"].clone()
        ),
        ("exited".into(), 3.into())
    );
}

#[test]
fn rm_flag_removes_the_container() {
    let sb = Sandbox::new();
    ok(&openpod(
        &sb,
        &[
            "run",
            "--rm",
            "--name",
            "gone",
            "--rootfs",
            &rootfs(),
            "true",
        ],
    ));
    let out = openpod(&sb, &["inspect", "gone"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(
        stderr(&out).contains("cannot find container gone"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn ps_lists_running_and_all() {
    let sb = Sandbox::new();
    let running = start(&sb, &["--name", "busy"], &["sleep", "5"]);
    ok(&openpod(
        &sb,
        &["run", "--name", "done", "--rootfs", &rootfs(), "true"],
    ));
    let ps = ok(&openpod(&sb, &["ps"]));
    assert!(
        ps.lines().next().unwrap().starts_with("CONTAINER ID"),
        "{ps}"
    );
    assert!(
        ps.contains("busy") && ps.contains(&running[..12]) && ps.contains("Up "),
        "{ps}"
    );
    assert!(!ps.contains("done"), "{ps}");
    let all = ok(&openpod(&sb, &["ps", "-a"]));
    assert!(all.contains("done") && all.contains("Exited (0)"), "{all}");
    let quiet = ok(&openpod(&sb, &["ps", "-a", "-q"]));
    assert_eq!(quiet.lines().count(), 2, "{quiet}");
    ok(&openpod(&sb, &["rm", "-f", "busy"]));
}

#[test]
fn duplicate_names_are_rejected() {
    let sb = Sandbox::new();
    ok(&openpod(
        &sb,
        &["run", "--name", "web", "--rootfs", &rootfs(), "true"],
    ));
    let out = openpod(
        &sb,
        &["run", "--name", "web", "--rootfs", &rootfs(), "true"],
    );
    assert_eq!(out.status.code(), Some(125));
    assert!(
        stderr(&out).contains("the name web is in use"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn rm_refuses_a_running_container_without_force() {
    let sb = Sandbox::new();
    start(&sb, &["--name", "live"], &["sleep", "10"]);
    let out = openpod(&sb, &["rm", "live"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("is running"), "{}", stderr(&out));
    ok(&openpod(&sb, &["rm", "-f", "live"]));
    assert_eq!(openpod(&sb, &["inspect", "live"]).status.code(), Some(125));
}

#[test]
fn rm_removes_an_exited_container_and_its_files() {
    let sb = Sandbox::new();
    ok(&openpod(
        &sb,
        &["run", "--name", "x", "--rootfs", &rootfs(), "true"],
    ));
    ok(&openpod(&sb, &["rm", "x"]));
    let left: Vec<_> = std::fs::read_dir(sb.data().join("containers"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn logs_follow_ends_when_the_container_exits() {
    let sb = Sandbox::new();
    let id = start(&sb, &[], &["sh", "-c", "echo a; sleep 1; echo b"]);
    let t = Instant::now();
    let out = openpod(&sb, &["logs", "-f", &id]);
    assert_eq!(stdout(&out), "a\nb");
    assert!(t.elapsed() < Duration::from_secs(10));
}

fn exit_code(sb: &Sandbox, c: &str) -> serde_json::Value {
    wait_for_status(sb, c, "exited")["state"]["exit_code"].clone()
}

#[test]
fn stop_ends_a_container_that_handles_sigterm() {
    let sb = Sandbox::new();
    start(
        &sb,
        &["--name", "polite"],
        &[
            "sh",
            "-c",
            "trap 'exit 0' TERM; while true; do sleep 0.1; done",
        ],
    );
    let t = Instant::now();
    ok(&openpod(&sb, &["stop", "-t", "5", "polite"]));
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "stop took {:?}",
        t.elapsed()
    );
    assert_eq!(exit_code(&sb, "polite"), 0);
}

#[test]
fn stop_escalates_to_sigkill_after_the_timeout() {
    let sb = Sandbox::new();
    start(
        &sb,
        &["--name", "stubborn"],
        &["sh", "-c", "trap '' TERM; while true; do sleep 0.1; done"],
    );
    let t = Instant::now();
    ok(&openpod(&sb, &["stop", "-t", "1", "stubborn"]));
    let took = t.elapsed();
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(5),
        "stop took {took:?}"
    );
    assert_eq!(exit_code(&sb, "stubborn"), 137);
}

#[test]
fn kill_sends_the_named_signal() {
    let sb = Sandbox::new();
    start(
        &sb,
        &["--name", "k"],
        &[
            "sh",
            "-c",
            "trap 'exit 42' USR1; while true; do sleep 0.1; done",
        ],
    );
    std::thread::sleep(Duration::from_millis(300));
    ok(&openpod(&sb, &["kill", "-s", "USR1", "k"]));
    assert_eq!(exit_code(&sb, "k"), 42);
}

// Rootless: a container process running as another user (a subordinate ID) cannot be
// signaled from the host user; the shim must do it from inside the user namespace.
#[test]
fn stop_reaches_a_non_root_container_user() {
    let sb = Sandbox::new();
    start(
        &sb,
        &["--name", "guest", "-u", "405"],
        &[
            "sh",
            "-c",
            "trap 'exit 7' TERM; while true; do sleep 0.1; done",
        ],
    );
    std::thread::sleep(Duration::from_millis(300));
    ok(&openpod(&sb, &["stop", "-t", "5", "guest"]));
    assert_eq!(exit_code(&sb, "guest"), 7);
}

#[test]
fn ps_marks_a_container_with_a_dead_shim_as_exited() {
    let sb = Sandbox::new();
    start(&sb, &["--name", "orphan"], &["sleep", "30"]);
    let shim = inspect(&sb, "orphan")["state"]["shim_pid"]
        .as_u64()
        .unwrap()
        .to_string();
    let killer = if root_mode() {
        vec!["sudo", "-n", "kill", "-KILL", &shim]
    } else {
        vec!["kill", "-KILL", &shim]
    };
    assert!(
        std::process::Command::new(killer[0])
            .args(&killer[1..])
            .status()
            .unwrap()
            .success()
    );
    let info = wait_for_status(&sb, "orphan", "exited");
    assert!(info["state"]["exit_code"].is_null(), "{info}");
    assert!(ok(&openpod(&sb, &["ps", "-a"])).contains("Exited (?)"));
    ok(&openpod(&sb, &["rm", "orphan"]));
}

fn running(sb: &Sandbox, name: &str, options: &[&str]) -> String {
    let mut opts = vec!["--name", name];
    opts.extend_from_slice(options);
    let id = start(sb, &opts, &["sleep", "30"]);
    std::thread::sleep(Duration::from_millis(200));
    id
}

#[test]
fn exec_runs_in_the_container_namespaces() {
    let sb = Sandbox::new();
    let id = running(&sb, "ns", &[]);
    assert_eq!(ok(&openpod(&sb, &["exec", "ns", "hostname"])), id[..12]);
    assert_eq!(
        ok(&openpod(
            &sb,
            &["exec", "ns", "sh", "-c", "tr '\\0' ' ' < /proc/1/cmdline"]
        )),
        "sleep 30"
    );
    assert!(ok(&openpod(&sb, &["exec", "ns", "cat", "/etc/alpine-release"])).starts_with('3'));
    ok(&openpod(&sb, &["rm", "-f", "ns"]));
}

#[test]
fn exec_uses_container_env_and_user_flag() {
    let sb = Sandbox::new();
    running(&sb, "env", &["-e", "FOO=bar", "-w", "/tmp"]);
    assert_eq!(
        ok(&openpod(
            &sb,
            &["exec", "env", "sh", "-c", "echo $FOO; id -u; pwd"]
        )),
        "bar\n0\n/tmp"
    );
    assert_eq!(
        ok(&openpod(&sb, &["exec", "-u", "405", "env", "id", "-u"])),
        "405"
    );
    assert_eq!(
        ok(&openpod(
            &sb,
            &[
                "exec",
                "-e",
                "A=1",
                "-w",
                "/etc",
                "env",
                "sh",
                "-c",
                "echo $A; pwd"
            ]
        )),
        "1\n/etc"
    );
    ok(&openpod(&sb, &["rm", "-f", "env"]));
}

#[test]
fn exec_exit_code_propagates() {
    let sb = Sandbox::new();
    running(&sb, "codes", &[]);
    assert_eq!(
        openpod(&sb, &["exec", "codes", "sh", "-c", "exit 5"])
            .status
            .code(),
        Some(5)
    );
    let out = openpod(&sb, &["exec", "codes", "/no/such"]);
    assert_eq!(out.status.code(), Some(127), "{}", stderr(&out));
    ok(&openpod(&sb, &["rm", "-f", "codes"]));
}

#[test]
fn exec_does_not_stop_the_container() {
    let sb = Sandbox::new();
    running(&sb, "stay", &[]);
    ok(&openpod(&sb, &["exec", "stay", "true"]));
    assert_eq!(inspect(&sb, "stay")["state"]["status"], "running");
    ok(&openpod(&sb, &["rm", "-f", "stay"]));
}

#[test]
fn exec_into_a_stopped_container_fails() {
    let sb = Sandbox::new();
    ok(&openpod(
        &sb,
        &["run", "--name", "over", "--rootfs", &rootfs(), "true"],
    ));
    let out = openpod(&sb, &["exec", "over", "true"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("is not running"), "{}", stderr(&out));
}

// Every run option must also apply to background containers.
#[test]
fn detached_run_keeps_every_option() {
    let sb = Sandbox::new();
    start(
        &sb,
        &[
            "--name",
            "opts",
            "-u",
            "405",
            "-e",
            "X=1",
            "-w",
            "/tmp",
            "--hostname",
            "hh",
        ],
        &["sh", "-c", "id -u; echo $X; pwd; hostname"],
    );
    wait_for_status(&sb, "opts", "exited");
    assert_eq!(ok(&openpod(&sb, &["logs", "opts"])), "405\n1\n/tmp\nhh");
}

#[test]
fn tty_gives_the_container_a_terminal() {
    let sb = Sandbox::new();
    let out = openpod(&sb, &["run", "--rm", "-t", "--rootfs", &rootfs(), "tty"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "/dev/pts/0");
}

#[test]
fn exec_tty_gives_a_terminal() {
    let sb = Sandbox::new();
    running(&sb, "termy", &[]);
    let out = openpod(&sb, &["exec", "-t", "termy", "tty"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).starts_with("/dev/pts/"), "{}", stdout(&out));
    ok(&openpod(&sb, &["rm", "-f", "termy"]));
}

#[test]
fn tty_output_is_one_stream_in_logs() {
    let sb = Sandbox::new();
    ok(&openpod(
        &sb,
        &[
            "run",
            "-t",
            "--name",
            "one",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            "echo out; echo err >&2",
        ],
    ));
    let out = openpod(&sb, &["logs", "one"]);
    assert_eq!(stdout(&out).replace('\r', ""), "out\nerr");
    assert_eq!(stderr(&out), "");
}

// Review fixes.

// A normal desktop user's runtime directory (/run/user/1000/openpod) left no room in the
// 108-byte socket address for the container's control socket.
#[test]
fn runs_with_a_long_runtime_dir() {
    let mut sb = Sandbox::new();
    let long = sb.tmp.path().join("r".repeat(80));
    sb.env
        .push(("OPENPOD_RUNTIME_DIR".into(), long.display().to_string()));
    let out = sb.run(&["true"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    start(&sb, &["--name", "far"], &["sleep", "30"]);
    ok(&openpod(&sb, &["kill", "far"]));
    wait_for_status(&sb, "far", "exited");
}

fn kill_as_owner(pid: &str) {
    let mut cmd = if root_mode() {
        let mut c = std::process::Command::new("sudo");
        c.args(["-n", "kill"]);
        c
    } else {
        std::process::Command::new("kill")
    };
    assert!(cmd.args(["-KILL", pid]).status().unwrap().success());
}

// PR_SET_PDEATHSIG is cleared when the container switches to another user, so a killed
// shim used to leave a non-root container running with nothing to stop it.
#[test]
fn killed_shim_takes_a_non_root_container_with_it() {
    let sb = Sandbox::new();
    start(&sb, &["--name", "orphan", "-u", "405"], &["sleep", "30"]);
    let st = inspect(&sb, "orphan")["state"].clone();
    let (shim, pid) = (st["shim_pid"].to_string(), st["pid"].as_u64().unwrap());
    kill_as_owner(&shim);
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        assert!(
            Instant::now() < deadline,
            "container process {pid} outlived its shim"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run openpod with a new terminal as its controlling terminal, like a user's shell does.
fn on_terminal(sb: &Sandbox, args: &[&str]) -> (std::process::Child, std::fs::File) {
    use std::os::unix::process::CommandExt;
    let (master, slave) = openpod::linux::pty::open(None).unwrap();
    let slave = std::fs::File::from(slave);
    let mut cmd = sb.openpod();
    cmd.args(args)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 0);
            Ok(())
        });
    }
    (cmd.spawn().unwrap(), std::fs::File::from(master))
}

/// Read the terminal until `needle` appears. Returns everything read.
fn read_until(term: &mut std::fs::File, needle: &str, timeout: Duration) -> String {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let deadline = Instant::now() + timeout;
    let mut seen = String::new();
    while !seen.contains(needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "no {needle:?} on the terminal; got {seen:?}"
        );
        let mut pfd = libc::pollfd {
            fd: term.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
            continue;
        }
        let mut buf = [0u8; 4096];
        match term.read(&mut buf) {
            Ok(n) if n > 0 => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            _ => panic!("terminal closed before {needle:?}; got {seen:?}"),
        }
    }
    seen
}

fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st.code();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("openpod did not end within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// With -t but no -i the terminal stays in normal mode, so Ctrl+C becomes SIGINT for the
// caller's process group, which the container (in its own session) is not part of.
#[test]
fn tty_without_i_passes_ctrl_c() {
    use std::io::Write;
    let sb = Sandbox::new();
    let script = "trap 'echo got-int; exit 3' INT; echo ready; while :; do sleep 0.1; done";
    let (mut child, mut term) = on_terminal(
        &sb,
        &[
            "run",
            "--rm",
            "-t",
            "--rootfs",
            &rootfs(),
            "sh",
            "-c",
            script,
        ],
    );
    read_until(&mut term, "ready", Duration::from_secs(10));
    term.write_all(b"\x03").unwrap();
    read_until(&mut term, "got-int", Duration::from_secs(5));
    assert_eq!(
        wait_with_timeout(&mut child, Duration::from_secs(5)),
        Some(3)
    );
}

// The caller's terminal must not be the container's controlling terminal: through
// /dev/tty a container could read it, or push keystrokes into the user's shell (TIOCSTI).
#[test]
fn container_cannot_reach_the_callers_terminal() {
    let sb = Sandbox::new();
    let probe = "if (: </dev/tty) 2>/dev/null; then echo tty-open; else echo tty-closed; fi";
    let (mut child, mut term) = on_terminal(
        &sb,
        &["run", "--rm", "--rootfs", &rootfs(), "sh", "-c", probe],
    );
    let seen = read_until(&mut term, "tty-", Duration::from_secs(10));
    wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(!seen.contains("tty-open"), "run: {seen:?}");

    running(&sb, "probe", &[]);
    let (mut child, mut term) = on_terminal(&sb, &["exec", "probe", "sh", "-c", probe]);
    let seen = read_until(&mut term, "tty-", Duration::from_secs(10));
    wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(!seen.contains("tty-open"), "exec: {seen:?}");
    ok(&openpod(&sb, &["rm", "-f", "probe"]));
}

/// Wait until a container started in the background reaches "running".
fn wait_until_running(sb: &Sandbox, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while openpod(sb, &["inspect", name]).status.code() != Some(0) {
        assert!(Instant::now() < deadline, "container {name} never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
    wait_for_status(sb, name, "running");
}

fn timed_stop(sb: &Sandbox, name: &str) -> Duration {
    let begin = Instant::now();
    let out = openpod(sb, &["stop", "-t", "2", name]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    begin.elapsed()
}

// The shim wrote stdin into the terminal with a blocking write. A container that does not
// read its input then froze the shim: no output, no stop, no kill.
#[test]
fn stdin_flood_does_not_block_stop() {
    use std::io::Write;
    use std::process::Stdio;
    let sb = Sandbox::new();
    let mut cli = sb
        .openpod()
        .args([
            "run",
            "--rm",
            "-i",
            "-t",
            "--name",
            "flood",
            "--rootfs",
            &rootfs(),
            "sleep",
            "30",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = cli.stdin.take().unwrap();
    std::thread::spawn(move || {
        let line = [b'x'; 1023]
            .iter()
            .chain(b"\n")
            .copied()
            .collect::<Vec<u8>>();
        for _ in 0..1024 {
            if stdin.write_all(&line).is_err() {
                break;
            }
        }
    });
    wait_until_running(&sb, "flood");
    std::thread::sleep(Duration::from_millis(500));
    let took = timed_stop(&sb, "flood");
    assert!(took < Duration::from_secs(6), "stop took {took:?}");
    wait_with_timeout(&mut cli, Duration::from_secs(10));
}

// A caller that stops reading output (a stalled pipe, or Ctrl+S) must not stop the shim
// from serving stop and kill.
#[test]
fn slow_reader_does_not_block_stop() {
    use std::process::Stdio;
    let sb = Sandbox::new();
    let mut cli = sb
        .openpod()
        .args(["run", "--name", "slow", "--rootfs", &rootfs(), "sh", "-c"])
        .arg("while :; do echo 0123456789012345678901234567890123456789; done")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until_running(&sb, "slow");
    std::thread::sleep(Duration::from_secs(1));
    let took = timed_stop(&sb, "slow");
    assert!(took < Duration::from_secs(6), "stop took {took:?}");
    let _ = cli.kill();
    drop(cli.stdout.take());
    let _ = cli.wait();
    wait_for_status(&sb, "slow", "exited");
    ok(&openpod(&sb, &["rm", "slow"]));
}

/// A free-looking local port for this test process.
fn port(offset: u16) -> String {
    (41000 + (std::process::id() % 2000) as u16 + offset).to_string()
}

#[test]
fn create_then_start_runs_the_container() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    let id = ok(&openpod(
        &sb,
        &[
            "create", "--name", "c", "--rootfs", &rootfs, "sh", "-c", "echo hi",
        ],
    ));
    assert_eq!(id.len(), 64, "{id}");
    let ps = ok(&openpod(&sb, &["ps", "-a"]));
    assert!(
        ps.lines()
            .any(|l| l.contains("Created") && l.ends_with(" c")),
        "{ps}"
    );
    assert!(
        !ok(&openpod(&sb, &["ps"])).contains(" c\n"),
        "created is not running"
    );
    let out = openpod(&sb, &["start", "-a", "c"]);
    assert_eq!(ok(&out), "hi");
    let info = wait_for_status(&sb, "c", "exited");
    assert_eq!(info["state"]["exit_code"], 0);
}

#[test]
fn an_exited_container_starts_again() {
    let sb = Sandbox::new();
    let id = start(
        &sb,
        // /mnt exists in the shared rootfs: a new mount point would be made in it.
        &["--name", "r", "-v", "runs:/mnt"],
        &["sh", "-c", "echo run >> /mnt/runs; cat /mnt/runs | wc -l"],
    );
    wait_for_status(&sb, &id, "exited");
    let out = openpod(&sb, &["start", "-a", "r"]);
    assert_eq!(ok(&out), "2");
    let logs = ok(&openpod(&sb, &["logs", "r"]));
    assert_eq!(logs, "1\n2", "both runs are in the log");
    assert_eq!(inspect(&sb, "r")["id"], id.as_str());
}

#[test]
fn restart_stops_and_starts() {
    let sb = Sandbox::new();
    start(&sb, &["--name", "s"], &["sleep", "60"]);
    let first = inspect(&sb, "s")["state"]["started"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    ok(&openpod(&sb, &["restart", "-t", "1", "s"]));
    let info = inspect(&sb, "s");
    assert_eq!(info["state"]["status"], "running", "{info}");
    assert!(info["state"]["started"].as_u64().unwrap() > first, "{info}");
    ok(&openpod(&sb, &["rm", "-f", "s"]));
}

#[test]
fn a_failed_start_keeps_the_created_container() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    let publish = format!("127.0.0.1:{}:80", port(0));
    for name in ["p1", "p2"] {
        ok(&openpod(
            &sb,
            &[
                "create", "--name", name, "-p", &publish, "--rootfs", &rootfs, "sleep", "30",
            ],
        ));
    }
    ok(&openpod(&sb, &["start", "p1"]));
    let second = openpod(&sb, &["start", "p2"]);
    assert_ne!(second.status.code(), Some(0), "the port is taken");
    let ps = ok(&openpod(&sb, &["ps", "-a"]));
    assert!(
        ps.lines()
            .any(|l| l.contains("Created") && l.ends_with(" p2")),
        "{ps}"
    );
    // `run` still leaves nothing behind when its start fails.
    let before = ps.lines().count();
    let run = openpod(
        &sb,
        &[
            "run", "-d", "-p", &publish, "--rootfs", &rootfs, "sleep", "30",
        ],
    );
    assert_ne!(run.status.code(), Some(0));
    assert_eq!(ok(&openpod(&sb, &["ps", "-a"])).lines().count(), before);
    ok(&openpod(&sb, &["rm", "-f", "p1", "p2"]));
}

#[test]
fn start_waits_for_the_last_shim_to_finish() {
    let sb = Sandbox::new();
    let run = |args: &[&str]| {
        let out = sb.openpod().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        stdout(&out)
    };
    let id = run(&[
        "run",
        "-d",
        "--name",
        "ss",
        "--rootfs",
        &rootfs(),
        "sleep",
        "300",
    ]);
    run(&["stop", "-t", "1", "ss"]);
    // The last run's shim answers `stop` before it lets go of its lock (as root, cgroup
    // cleanup can take a second). A flock plays that shim here.
    let lock = sb.data().join("containers").join(&id).join("shim.lock");
    let marker = sb.tmp.path().join("locked");
    let script = format!("touch {}; sleep 0.8", marker.display());
    let mut holder = if root_mode() {
        let mut c = std::process::Command::new("sudo");
        c.args(["-n", "flock"]);
        c
    } else {
        std::process::Command::new("flock")
    }
    .arg(&lock)
    .args(["-c", &script])
    .spawn()
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(marker.exists(), "flock did not take the lock");
    let out = sb.openpod().args(["start", "ss"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let _ = holder.wait();
    let ps = run(&["ps"]);
    assert!(
        ps.lines()
            .any(|l| l.split_whitespace().last() == Some("ss")),
        "{ps}"
    );
    // A container that really runs is refused at once, without waiting.
    let begun = std::time::Instant::now();
    let out = sb.openpod().args(["start", "ss"]).output().unwrap();
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("already running"), "{}", stderr(&out));
    assert!(begun.elapsed() < std::time::Duration::from_secs(3));
    run(&["rm", "-f", "ss"]);
}
