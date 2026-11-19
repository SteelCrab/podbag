#![cfg(target_os = "linux")]
//! Resource limits: cgroup placement, limit values, the memory budget, and exit reasons.

mod common;
use common::*;
use std::path::Path;
use std::time::{Duration, Instant};

fn podbag(sb: &Sandbox, args: &[&str]) -> std::process::Output {
    sb.podbag().args(args).output().unwrap()
}

fn ok(out: &std::process::Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

/// `run -d --name <name> <options...> --rootfs <alpine> <command...>`.
fn start(sb: &Sandbox, name: &str, options: &[&str], command: &[&str]) {
    let mut args = vec!["run", "-d", "--name", name];
    args.extend_from_slice(options);
    let rootfs = rootfs();
    args.extend_from_slice(&["--rootfs", &rootfs]);
    args.extend_from_slice(command);
    ok(&podbag(sb, &args));
}

fn inspect(sb: &Sandbox, c: &str) -> serde_json::Value {
    serde_json::from_str(&ok(&podbag(sb, &["inspect", c]))).unwrap()
}

/// The cgroup path of a running container (an ended one's state no longer names a cgroup
/// that is gone, so the path is read while it runs).
fn running_cgroup(sb: &Sandbox, c: &str, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let info = inspect(sb, c);
        if info["state"]["status"] == "running" {
            if let Some(path) = info["state"]["cgroup"].as_str() {
                return path.to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "{c} never ran with a cgroup: {info}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait until the container has exited; returns its `inspect` record.
fn wait_exited(sb: &Sandbox, c: &str, secs: u64) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let info = inspect(sb, c);
        if info["state"]["status"] == "exited" {
            return info;
        }
        assert!(Instant::now() < deadline, "{c} never exited: {info}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until a path is gone.
fn wait_gone(path: &str, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Path::new(path).exists() {
        assert!(Instant::now() < deadline, "{path} still exists");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The container's `ps -a` line.
fn ps_line(sb: &Sandbox, name: &str) -> String {
    ok(&podbag(sb, &["ps", "-a"]))
        .lines()
        .find(|l| l.split_whitespace().last() == Some(name))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn container_sees_its_own_cgroup_root() {
    let sb = Sandbox::new();
    let out = sb.run(&["cat", "/proc/self/cgroup"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "0::/\n",
        "{}",
        stderr(&out)
    );
}

/// `run --rm <options> cat <file>`: the file's content.
fn read_in_container(sb: &Sandbox, options: &[&str], file: &str) -> String {
    let out = sb.run_with(options, &["cat", file]);
    ok(&out)
}

#[test]
fn memory_sets_memory_max() {
    let sb = Sandbox::new();
    assert_eq!(
        read_in_container(&sb, &["--memory", "64m"], "/sys/fs/cgroup/memory.max"),
        "67108864"
    );
}

#[test]
fn cpus_sets_cpu_max() {
    let sb = Sandbox::new();
    assert_eq!(
        read_in_container(&sb, &["--cpus", "0.5"], "/sys/fs/cgroup/cpu.max"),
        "50000 100000"
    );
}

#[test]
fn pids_limit_applies() {
    let sb = Sandbox::new();
    assert_eq!(
        read_in_container(&sb, &["--pids-limit", "20"], "/sys/fs/cgroup/pids.max"),
        "20"
    );
}

#[test]
fn memory_limit_kills_with_a_reason() {
    let sb = Sandbox::new();
    start(
        &sb,
        "hungry",
        &["--memory", "32m"],
        &["dd", "if=/dev/zero", "of=/dev/null", "bs=64M", "count=1"],
    );
    let info = wait_exited(&sb, "hungry", 20);
    assert_eq!(info["state"]["exit_code"], 137, "{info}");
    assert_eq!(info["state"]["oom"], "container_limit", "{info}");
    let line = ps_line(&sb, "hungry");
    assert!(
        line.contains("Exited (137, out of memory: container limit)"),
        "{line}"
    );
    ok(&podbag(&sb, &["rm", "hungry"]));
}

#[test]
fn invalid_limits_are_rejected() {
    let sb = Sandbox::new();
    let out = sb.run_with(&["--memory", "4m"], &["true"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("6MiB"), "{}", stderr(&out));
    let out = sb.run_with(&["--cpus", "0"], &["true"]);
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
}

#[test]
fn cgroup_is_removed_after_exit() {
    let sb = Sandbox::new();
    start(&sb, "brief", &["--memory", "64m"], &["sleep", "1"]);
    let path = running_cgroup(&sb, "brief", 10);
    let info = wait_exited(&sb, "brief", 10);
    wait_gone(&path, 3);
    // The state names no cgroup that is gone (a later check would remove the next run's,
    // which gets the same path).
    assert!(info["state"]["cgroup"].is_null(), "{info}");
    ok(&podbag(&sb, &["rm", "brief"]));
}

#[test]
fn killed_shim_leaves_no_cgroup() {
    if !root_mode() {
        return;
    }
    let sb = Sandbox::new();
    start(&sb, "orphan", &["--memory", "64m"], &["sleep", "60"]);
    let info = inspect(&sb, "orphan");
    let path = info["state"]["cgroup"].as_str().expect("a cgroup path");
    let shim = info["state"]["shim_pid"].as_u64().unwrap().to_string();
    assert!(
        std::process::Command::new("sudo")
            .args(["-n", "kill", "-KILL", &shim])
            .status()
            .unwrap()
            .success()
    );
    wait_exited(&sb, "orphan", 10);
    wait_gone(path, 3);
    ok(&podbag(&sb, &["rm", "orphan"]));
}

#[test]
fn limits_without_systemd_are_an_error() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::with_env("PODBAG_SYSTEMD_RUN", "/nonexistent");
    let out = sb.run_with(&["--memory", "32m"], &["true"]);
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
    assert!(stderr(&out).contains("systemd"), "{}", stderr(&out));
}

#[test]
fn runs_without_systemd_with_a_warning() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::with_env("PODBAG_SYSTEMD_RUN", "/nonexistent");
    let out = sb.run(&["echo", "hi"]);
    assert_eq!(ok(&out), "hi");
    assert!(stderr(&out).contains("warning"), "{}", stderr(&out));
}

/// A cgroup parent of its own and a 64 MiB budget, so the small budget does not touch
/// other tests.
fn budget_sandbox(tag: &str) -> (Sandbox, String) {
    let parent = format!("podbag{tag}{}", std::process::id());
    let mut sb = Sandbox::with_env("PODBAG_CGROUP_PARENT", &parent);
    sb.env
        .push(("PODBAG_MEMORY_BUDGET".to_string(), "64MiB".to_string()));
    (sb, parent)
}

/// The parent group of a container's cgroup: the directory named `parent` (root mode)
/// or `<parent>.slice` (rootless).
fn parent_dir(cgroup: &str, parent: &str) -> std::path::PathBuf {
    let slice = format!("{parent}.slice");
    Path::new(cgroup)
        .ancestors()
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n == parent || n == slice.as_str())
        })
        .unwrap_or_else(|| panic!("no {parent} above {cgroup}"))
        .to_path_buf()
}

/// Remove the test's parent group once its containers are gone.
fn remove_parent(dir: &Path, parent: &str) {
    let status = if root_mode() {
        std::process::Command::new("sudo")
            .args(["-n", "rmdir"])
            .arg(dir)
            .status()
    } else {
        std::process::Command::new("systemctl")
            .args(["--user", "stop", &format!("{parent}.slice")])
            .status()
    };
    assert!(status.unwrap().success());
}

fn page_aligned(bytes: u64) -> u64 {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    bytes / page * page
}

#[test]
fn budget_is_written_to_the_parent() {
    let (sb, parent) = budget_sandbox("budgetfile");
    start(&sb, "b", &[], &["sleep", "30"]);
    let info = inspect(&sb, "b");
    let dir = parent_dir(info["state"]["cgroup"].as_str().unwrap(), &parent);
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
    assert_eq!(read("memory.max"), "67108864\n");
    assert_eq!(
        read("memory.high"),
        format!("{}\n", page_aligned((64u64 << 20) * 9 / 10))
    );
    ok(&podbag(&sb, &["rm", "-f", "b"]));
    remove_parent(&dir, &parent);
}

#[test]
fn budget_exceeded_kills_one_container_with_a_reason() {
    let (sb, parent) = budget_sandbox("budgetoom");
    // Each holds 48 MiB: dd keeps its buffer while it waits to write into the pipe.
    let hog = ["sh", "-c", "dd if=/dev/zero bs=48M count=1 | sleep 60"];
    start(&sb, "a", &[], &hog);
    let a_cgroup = running_cgroup(&sb, "a", 10);
    std::thread::sleep(Duration::from_secs(2));
    start(&sb, "b", &[], &hog);
    let deadline = Instant::now() + Duration::from_secs(20);
    let ended = || -> Vec<serde_json::Value> {
        ["a", "b"]
            .iter()
            .map(|c| inspect(&sb, c))
            .filter(|i| i["state"]["status"] == "exited")
            .collect()
    };
    while ended().is_empty() {
        assert!(Instant::now() < deadline, "no container was ended");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The kernel may end the second one too when the first one's memory is not given
    // back yet; every container it ended must carry the reason.
    std::thread::sleep(Duration::from_secs(1));
    let ended = ended();
    for info in &ended {
        assert_eq!(info["state"]["exit_code"], 137, "{info}");
        assert_eq!(info["state"]["oom"], "memory_budget", "{info}");
    }
    let killed = &ended[0];
    let name = killed["name"].as_str().unwrap();
    assert!(
        ps_line(&sb, name).contains("out of memory: memory budget"),
        "{}",
        ps_line(&sb, name)
    );
    let dir = parent_dir(&a_cgroup, &parent);
    ok(&podbag(&sb, &["rm", "-f", "a", "b"]));
    remove_parent(&dir, &parent);
}

#[test]
fn exec_runs_in_the_container_cgroup() {
    let sb = Sandbox::new();
    start(&sb, "x", &["--pids-limit", "50"], &["sleep", "30"]);
    std::thread::sleep(Duration::from_millis(200));
    let exec = |cmd: &[&str]| {
        let mut args = vec!["exec", "x"];
        args.extend_from_slice(cmd);
        ok(&podbag(&sb, &args))
    };
    assert_eq!(exec(&["cat", "/proc/self/cgroup"]), "0::/");
    // sleep, the shell, and cat (the trailing `:` keeps the shell from exec-ing cat).
    let current: u32 = exec(&["sh", "-c", "cat /sys/fs/cgroup/pids.current; :"])
        .parse()
        .unwrap();
    assert!(current >= 3, "pids.current = {current}");
    ok(&podbag(&sb, &["rm", "-f", "x"]));
}

#[test]
fn exec_child_does_not_leave_the_cgroup_behind() {
    let sb = Sandbox::new();
    start(&sb, "y", &["--memory", "64m"], &["sleep", "2"]);
    let path = running_cgroup(&sb, "y", 10);
    let mut exec = sb
        .podbag()
        .args(["exec", "y", "sleep", "30"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_exited(&sb, "y", 10);
    wait_gone(&path, 3);
    let deadline = Instant::now() + Duration::from_secs(5);
    while exec.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "exec outlived its container");
        std::thread::sleep(Duration::from_millis(50));
    }
    ok(&podbag(&sb, &["rm", "y"]));
}

// A cgroup that was still busy when the shim died is removed by a later command
// (The next state check retries the cleanup.)
#[test]
fn busy_cgroup_is_removed_by_a_later_command() {
    if !root_mode() {
        return;
    }
    let sb = Sandbox::new();
    start(&sb, "busy", &["--memory", "64m"], &["sleep", "60"]);
    let info = inspect(&sb, "busy");
    let path = info["state"]["cgroup"].as_str().unwrap().to_string();
    let shim = info["state"]["shim_pid"].as_u64().unwrap().to_string();
    let sudo = |args: &[&str]| {
        assert!(
            std::process::Command::new("sudo")
                .arg("-n")
                .args(args)
                .status()
                .unwrap()
                .success()
        )
    };
    // A host process keeps the group busy.
    let mut holder = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    let procs = format!("{path}/cgroup.procs");
    sudo(&["sh", "-c", &format!("echo {} > {procs}", holder.id())]);
    sudo(&["kill", "-KILL", &shim]);
    wait_exited(&sb, "busy", 10);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(Path::new(&path).exists(), "the group should still be busy");
    holder.kill().unwrap();
    holder.wait().unwrap();
    ok(&podbag(&sb, &["ps", "-a"]));
    assert!(!Path::new(&path).exists(), "{path} was left behind");
    ok(&podbag(&sb, &["rm", "busy"]));
}
