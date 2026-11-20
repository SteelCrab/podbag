#![cfg(target_os = "linux")]
//! `stats`, `top`, `pause`, `unpause`, `rename` and `update`: through podbag's CLI, and
//! through the docker CLI against `podbag system service`.

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::process::Output;
use std::time::{Duration, Instant};

fn podbag(sb: &Sandbox, args: &[&str]) -> Output {
    sb.podbag().args(args).output().unwrap()
}

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

/// `run -d --name <name> <options...> --rootfs <alpine> <command...>`.
fn start(sb: &Sandbox, name: &str, options: &[&str], command: &[&str]) {
    let rootfs = rootfs();
    let mut args = vec!["run", "-d", "--name", name];
    args.extend_from_slice(options);
    args.extend_from_slice(&["--rootfs", &rootfs]);
    args.extend_from_slice(command);
    ok(&podbag(sb, &args));
}

fn inspect(sb: &Sandbox, c: &str) -> serde_json::Value {
    serde_json::from_str(&ok(&podbag(sb, &["inspect", c]))).unwrap()
}

/// A file as root reads it (in root mode, the files are root's).
fn read_file(path: &std::path::Path) -> String {
    if root_mode() {
        let out = std::process::Command::new("sudo")
            .args(["-n", "cat"])
            .arg(path)
            .output()
            .unwrap();
        stdout(&out)
    } else {
        std::fs::read_to_string(path).unwrap_or_default()
    }
}

/// The counter a container writes to `<dir>/n` every 50 ms.
fn counter(dir: &std::path::Path) -> u64 {
    read_file(&dir.join("n")).trim().parse().unwrap_or(0)
}

fn wait_until(secs: u64, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The cells of a table line (columns are three or more spaces apart).
fn cells(line: &str) -> Vec<&str> {
    line.split("   ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect()
}

const COUNT: &str = "i=0; while :; do i=$((i+1)); echo $i > /mnt/n; sleep 0.05; done";

#[test]
fn stats_and_top_through_the_cli() {
    let sb = Sandbox::new();
    start(&sb, "st", &[], &["sleep", "300"]);
    let table = ok(&podbag(&sb, &["stats", "--no-stream", "st"]));
    let lines: Vec<&str> = table.lines().collect();
    assert!(
        lines[0].starts_with("CONTAINER ID   NAME") && lines[0].ends_with("PIDS"),
        "{table}"
    );
    assert!(lines[0].contains("MEM USAGE / LIMIT"), "{table}");
    let row = lines.iter().find(|l| l.contains(" st ")).expect(&table);
    // One process; some memory in use, below a limit.
    let row = cells(row);
    assert_eq!(row.last(), Some(&"1"), "{table}");
    assert!(!row[3].starts_with("0B"), "{table}");
    assert!(!row[3].ends_with("/ 0B"), "{table}");
    // Every running container without names; `-a` adds the stopped ones.
    ok(&podbag(
        &sb,
        &["create", "--name", "off", "--rootfs", &rootfs(), "true"],
    ));
    let all = ok(&podbag(&sb, &["stats", "--no-stream", "-a"]));
    assert!(all.contains(" off "), "{all}");
    assert!(!ok(&podbag(&sb, &["stats", "--no-stream"])).contains(" off "));

    let top = ok(&podbag(&sb, &["top", "st"]));
    let lines: Vec<&str> = top.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"],
        "{top}"
    );
    assert_eq!(lines.len(), 2, "{top}");
    assert!(lines[1].ends_with("sleep 300"), "{top}");
    let pid = inspect(&sb, "st")["state"]["pid"].as_u64().unwrap();
    assert_eq!(
        lines[1].split_whitespace().nth(1),
        Some(pid.to_string().as_str())
    );
    // ps options go to the host's ps; only the container's rows stay.
    let top = ok(&podbag(&sb, &["top", "st", "-o", "pid,comm"]));
    let lines: Vec<&str> = top.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["PID", "COMMAND"]
    );
    assert_eq!(lines.len(), 2, "{top}");
    assert!(lines[1].ends_with("sleep"), "{top}");
    let out = podbag(&sb, &["top", "st", "-o", "comm"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("PID"), "{}", stderr(&out));
    let out = podbag(&sb, &["top", "off"]);
    assert_ne!(out.status.code(), Some(0));
    ok(&podbag(&sb, &["rm", "-f", "st", "off"]));
}

#[test]
fn a_paused_container_does_not_run_and_still_stops() {
    let sb = Sandbox::new();
    let out = sb.tmp.path().join("out");
    std::fs::create_dir(&out).unwrap();
    // /mnt is in the shared root filesystem already: a new mount point would be made there.
    let volume = format!("{}:/mnt", out.display());
    start(&sb, "pz", &["-v", &volume], &["sh", "-c", COUNT]);
    wait_until(10, "the counter never moved", || counter(&out) > 2);
    let pz = |args: &[&str]| podbag(&sb, args);
    ok(&pz(&["pause", "pz"]));
    assert_eq!(inspect(&sb, "pz")["state"]["paused"], true);
    let ps = ok(&pz(&["ps"]));
    assert!(ps.contains("(Paused)"), "{ps}");
    let frozen = counter(&out);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(counter(&out), frozen, "a paused container ran");
    // No exec, no second pause, no start.
    let exec = pz(&["exec", "pz", "true"]);
    assert_ne!(exec.status.code(), Some(0));
    assert!(stderr(&exec).contains("paused"), "{}", stderr(&exec));
    assert_ne!(pz(&["pause", "pz"]).status.code(), Some(0));
    assert_ne!(pz(&["start", "pz"]).status.code(), Some(0));
    ok(&pz(&["unpause", "pz"]));
    assert!(inspect(&sb, "pz")["state"].get("paused").is_none());
    wait_until(10, "the counter did not move again", || {
        counter(&out) > frozen + 2
    });
    assert_ne!(pz(&["unpause", "pz"]).status.code(), Some(0));
    // A paused container stops: the signal is handled once it is thawed.
    ok(&pz(&["pause", "pz"]));
    let began = Instant::now();
    ok(&pz(&["stop", "-t", "1", "pz"]));
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "{:?}",
        began.elapsed()
    );
    let st = inspect(&sb, "pz")["state"].clone();
    assert_eq!(st["status"], "exited", "{st}");
    assert!(st.get("paused").is_none(), "{st}");
    // Started again, paused, removed by force.
    ok(&pz(&["start", "pz"]));
    ok(&pz(&["pause", "pz"]));
    ok(&pz(&["rm", "-f", "pz"]));
    assert!(ok(&pz(&["ps", "-aq"])).is_empty());
}

#[test]
fn a_renamed_container_is_found_by_its_new_name() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&podbag(&sb, &["network", "create", "app"]));
    start(&sb, "old", &["--network", "app"], &["sleep", "60"]);
    start(&sb, "peer", &["--network", "app"], &["sleep", "60"]);
    ok(&podbag(&sb, &["rename", "old", "new"]));
    assert_eq!(inspect(&sb, "new")["name"], "new");
    assert_ne!(podbag(&sb, &["inspect", "old"]).status.code(), Some(0));
    let hosts = ok(&podbag(&sb, &["exec", "peer", "cat", "/etc/hosts"]));
    let names: Vec<&str> = hosts
        .lines()
        .flat_map(|l| l.split_whitespace().skip(1))
        .collect();
    assert!(names.contains(&"new"), "{hosts}");
    assert!(!names.contains(&"old"), "{hosts}");
    ok(&podbag(&sb, &["exec", "peer", "ping", "-c1", "-W2", "new"]));
    // A name in use, the same name, an invalid name.
    let taken = podbag(&sb, &["rename", "new", "peer"]);
    assert_eq!(taken.status.code(), Some(125), "{}", stderr(&taken));
    assert!(
        stderr(&taken).contains("already in use"),
        "{}",
        stderr(&taken)
    );
    assert_ne!(
        podbag(&sb, &["rename", "new", "new"]).status.code(),
        Some(0)
    );
    assert_ne!(
        podbag(&sb, &["rename", "new", "../x"]).status.code(),
        Some(0)
    );
    ok(&podbag(&sb, &["rm", "-f", "new", "peer"]));
    ok(&podbag(&sb, &["network", "rm", "app"]));
}

#[test]
fn update_changes_the_cgroup_and_later_starts() {
    let sb = Sandbox::new();
    start(&sb, "up", &["-m", "64m"], &["sleep", "300"]);
    let group = inspect(&sb, "up")["state"]["cgroup"]
        .as_str()
        .expect("a cgroup")
        .to_string();
    let max = || read_file(&std::path::Path::new(&group).join("memory.max"));
    assert_eq!(max().trim(), (64u64 << 20).to_string());
    ok(&podbag(
        &sb,
        &[
            "update",
            "-m",
            "128m",
            "--pids-limit",
            "50",
            "--cpus",
            "0.5",
            "up",
        ],
    ));
    assert_eq!(max().trim(), (128u64 << 20).to_string());
    let file = |name: &str| read_file(&std::path::Path::new(&group).join(name));
    assert_eq!(file("pids.max").trim(), "50");
    assert_eq!(file("cpu.max").trim(), "50000 100000");
    ok(&podbag(
        &sb,
        &["update", "--pids-limit", "-1", "--cpu-shares", "512", "up"],
    ));
    assert_eq!(file("pids.max").trim(), "max");
    assert_eq!(file("cpu.weight").trim(), "59");
    // Docker's checks.
    let small = podbag(&sb, &["update", "-m", "4m", "up"]);
    assert_ne!(small.status.code(), Some(0));
    let none = podbag(&sb, &["update", "up"]);
    assert!(
        stderr(&none).contains("one or more flags"),
        "{}",
        stderr(&none)
    );
    let cpus = podbag(&sb, &["update", "--cpus", "1000", "up"]);
    assert!(stderr(&cpus).contains("Range of CPUs"), "{}", stderr(&cpus));
    // The next start has the new limits.
    ok(&podbag(&sb, &["stop", "-t", "0", "up"]));
    ok(&podbag(&sb, &["start", "up"]));
    let group = inspect(&sb, "up")["state"]["cgroup"]
        .as_str()
        .unwrap()
        .to_string();
    let file = |name: &str| read_file(&std::path::Path::new(&group).join(name));
    assert_eq!(file("memory.max").trim(), (128u64 << 20).to_string());
    assert_eq!(file("cpu.max").trim(), "50000 100000");
    ok(&podbag(&sb, &["rm", "-f", "up"]));
}

#[test]
fn a_restart_policy_set_by_update_holds() {
    let sb = Sandbox::new();
    start(&sb, "rp", &[], &["sh", "-c", "sleep 1; exit 3"]);
    ok(&podbag(&sb, &["update", "--restart", "on-failure:2", "rp"]));
    // The run that was going when the policy changed is restarted too.
    wait_until(30, "it was not restarted twice", || {
        inspect(&sb, "rp")["restart_count"].as_u64() == Some(2)
            && inspect(&sb, "rp")["state"]["status"] == "exited"
    });
    ok(&podbag(&sb, &["rm", "-f", "rp"]));
}

struct Api {
    _reg: FakeRegistry,
    sb: Sandbox,
    service: Service,
    image: String,
}

impl Api {
    fn new() -> Api {
        let reg = FakeRegistry::start(false);
        reg.push(
            "img",
            "v1",
            &TestImage {
                layers: vec![alpine_layer()],
                config: serde_json::json!({
                    "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
                    "Cmd": ["sh"]
                }),
            },
        );
        let image = format!("{}/img:v1", reg.host);
        let sb = Sandbox::new();
        let service = Service::start(&sb, &["--timeout", "0"]);
        let api = Api {
            _reg: reg,
            sb,
            service,
            image,
        };
        api.ok(&["pull", &api.image]);
        api
    }

    fn docker(&self, args: &[&str]) -> Output {
        docker_out(&self.sb, &self.service, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.docker(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "docker {args:?}: {}",
            stderr(&out)
        );
        stdout(&out)
    }

    fn json(&self, path: &str) -> serde_json::Value {
        let (body, head) = curl(&self.service.socket, &[path]);
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{path}: {e}: {head} {body}"))
    }
}

#[test]
fn docker_stats_and_top() {
    let a = Api::new();
    a.ok(&["run", "-d", "--name", "w", &a.image, "sleep", "300"]);
    let table = a.ok(&["stats", "--no-stream", "w"]);
    assert!(table.contains("MEM USAGE / LIMIT"), "{table}");
    let row = cells(table.lines().find(|l| l.contains(" w ")).expect(&table));
    assert!(!row[3].starts_with("0B"), "{table}");
    // A stream sends a sample a second until the client goes.
    let streamed = docker_killed_after(&a.sb, &a.service, "3", &["stats", "w"]);
    // Still running when it was killed.
    assert!(
        matches!(streamed.status.code(), Some(124 | 137)),
        "{:?} {}",
        streamed.status,
        stderr(&streamed)
    );
    assert!(
        stdout(&streamed).matches("CONTAINER ID").count() >= 2,
        "{}",
        stdout(&streamed)
    );
    // The JSON the CLI reads.
    let one = a.json("/containers/w/stats?stream=false&one-shot=true");
    assert_eq!(one["name"], "/w");
    assert!(one["memory_stats"]["usage"].as_u64() > Some(0), "{one}");
    assert!(one["memory_stats"]["limit"].as_u64() > Some(0), "{one}");
    assert!(
        one["memory_stats"]["stats"]["inactive_file"].is_u64(),
        "{one}"
    );
    assert_eq!(one["pids_stats"]["current"], 1, "{one}");
    assert!(
        one["cpu_stats"]["system_cpu_usage"].as_u64() > Some(0),
        "{one}"
    );
    assert!(one["cpu_stats"]["online_cpus"].as_u64() > Some(0), "{one}");
    assert_eq!(one["preread"], "0001-01-01T00:00:00Z");
    assert!(one["networks"]["eth0"]["rx_bytes"].is_u64(), "{one}");
    let two = a.json("/containers/w/stats?stream=false");
    assert_ne!(two["preread"], "0001-01-01T00:00:00Z", "{two}");
    assert!(
        two["precpu_stats"]["system_cpu_usage"].as_u64() > Some(0),
        "{two}"
    );

    let top = a.ok(&["top", "w"]);
    assert!(top.starts_with("UID"), "{top}");
    assert!(top.contains("sleep 300"), "{top}");
    let top = a.ok(&["top", "w", "-o", "pid,comm"]);
    assert!(top.lines().next().unwrap().contains("COMMAND"), "{top}");
    assert!(top.contains("sleep"), "{top}");
    let bad = a.docker(&["top", "w", "-o", "comm"]);
    assert!(stderr(&bad).contains("PID"), "{}", stderr(&bad));
    a.ok(&["create", "--name", "c", &a.image, "true"]);
    let off = a.docker(&["top", "c"]);
    assert!(stderr(&off).contains("is not running"), "{}", stderr(&off));
    a.ok(&["rm", "-f", "w", "c"]);
}

#[test]
fn docker_pause_rename_and_update() {
    let a = Api::new();
    a.ok(&["run", "-d", "--name", "p", &a.image, "sleep", "300"]);
    // The events, line by line, from a stream that is known to be open: it has shown a
    // volume made after it began.
    let mut events = docker(&a.sb, &a.service, &["events"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = events.stdout.take().unwrap();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(reader)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        a.ok(&["volume", "create", "mark"]);
        a.ok(&["volume", "rm", "mark"]);
        if rx.recv_timeout(Duration::from_millis(500)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "the event stream showed nothing");
    }
    a.ok(&["pause", "p"]);
    let ps = a.ok(&["ps"]);
    assert!(ps.contains("(Paused)"), "{ps}");
    let state = a.ok(&["inspect", "-f", "{{.State.Status}} {{.State.Paused}}", "p"]);
    assert_eq!(state.trim(), "paused true");
    let exec = a.docker(&["exec", "p", "true"]);
    assert!(
        stderr(&exec).contains("is paused, unpause the container before exec"),
        "{}",
        stderr(&exec)
    );
    let again = a.docker(&["pause", "p"]);
    assert!(
        stderr(&again).contains("already paused"),
        "{}",
        stderr(&again)
    );
    let filtered = a.ok(&["ps", "-q", "--filter", "status=paused"]);
    assert_eq!(filtered.lines().count(), 1, "{filtered}");
    a.ok(&["unpause", "p"]);
    let state = a.ok(&["inspect", "-f", "{{.State.Status}} {{.State.Paused}}", "p"]);
    assert_eq!(state.trim(), "running false");
    a.ok(&["rename", "p", "q"]);
    std::thread::sleep(Duration::from_millis(800));
    let names = a.ok(&["ps", "--format", "{{.Names}}"]);
    assert_eq!(names.trim(), "q");
    a.ok(&["run", "-d", "--name", "other", &a.image, "sleep", "300"]);
    let taken = a.docker(&["rename", "q", "other"]);
    assert!(
        stderr(&taken).contains("is already in use"),
        "{}",
        stderr(&taken)
    );
    std::thread::sleep(Duration::from_secs(1));
    // In root mode the child is sudo, which passes the signal on.
    // SAFETY: a signal to our own child.
    unsafe { libc::kill(events.id() as i32, libc::SIGTERM) };
    let _ = events.wait();
    let seen: String = rx.try_iter().map(|l| l + "\n").collect();
    for action in ["pause", "unpause", "rename"] {
        assert!(
            seen.lines()
                .any(|l| l.contains(&format!(" container {action} "))),
            "no {action}: {seen}"
        );
    }

    // Update: live in the cgroup, and in the settings.
    a.ok(&[
        "update",
        "--memory",
        "128m",
        "--memory-swap",
        "256m",
        "--restart",
        "on-failure:3",
        "q",
    ]);
    let host = a.ok(&[
        "inspect",
        "-f",
        "{{.HostConfig.Memory}} {{.HostConfig.MemorySwap}} {{.HostConfig.RestartPolicy.Name}} {{.HostConfig.RestartPolicy.MaximumRetryCount}}",
        "q",
    ]);
    assert_eq!(host.trim(), "134217728 268435456 on-failure 3");
    let info: serde_json::Value =
        serde_json::from_str(&ok(&podbag(&a.sb, &["inspect", "q"]))).unwrap();
    let group = std::path::PathBuf::from(info["state"]["cgroup"].as_str().unwrap());
    assert_eq!(read_file(&group.join("memory.max")).trim(), "134217728");
    // Without swap accounting the kernel has no such file.
    if group.join("memory.swap.max").exists() {
        assert_eq!(
            read_file(&group.join("memory.swap.max")).trim(),
            "134217728"
        );
    }
    let small = a.docker(&["update", "--memory", "1m", "q"]);
    assert!(
        stderr(&small).contains("Minimum memory limit allowed is 6MB"),
        "{}",
        stderr(&small)
    );
    let quota = a.docker(&["update", "--cpu-quota", "500", "q"]);
    assert!(
        stderr(&quota).contains("CPU cfs quota"),
        "{}",
        stderr(&quota)
    );
    // A paused container stops.
    a.ok(&["pause", "q"]);
    a.ok(&["stop", "-t", "1", "q"]);
    let state = a.ok(&["inspect", "-f", "{{.State.Status}} {{.State.Paused}}", "q"]);
    assert_eq!(state.trim(), "exited false");
    // system df -v reads the containers as ps shows them.
    let df = a.ok(&["system", "df", "-v"]);
    assert!(df.contains("img:v1"), "{df}");
    a.ok(&["rm", "-f", "q", "other"]);
}

#[test]
fn without_a_cgroup_what_exists_is_shown_and_the_rest_explained() {
    if root_mode() {
        eprintln!("skipped: root mode always has cgroups");
        return;
    }
    let sb = Sandbox::with_env("PODBAG_SYSTEMD_RUN", "/nonexistent");
    start(&sb, "bare", &[], &["sleep", "300"]);
    assert!(inspect(&sb, "bare")["state"]["cgroup"].is_null());
    let out = podbag(&sb, &["stats", "--no-stream", "bare"]);
    let table = ok(&out);
    assert!(
        stderr(&out).contains("without a cgroup"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("systemd"), "{}", stderr(&out));
    let row = cells(table.lines().find(|l| l.contains(" bare ")).expect(&table));
    assert_eq!(row.last(), Some(&"1"), "{table}");
    // Its processes come from its PID namespace.
    let top = ok(&podbag(&sb, &["top", "bare"]));
    assert!(top.lines().any(|l| l.ends_with("sleep 300")), "{top}");
    let pause = podbag(&sb, &["pause", "bare"]);
    assert_ne!(pause.status.code(), Some(0));
    assert!(
        stderr(&pause).contains("without a cgroup"),
        "{}",
        stderr(&pause)
    );
    let update = podbag(&sb, &["update", "-m", "64m", "bare"]);
    assert_ne!(update.status.code(), Some(0));
    assert!(
        stderr(&update).contains("without a cgroup"),
        "{}",
        stderr(&update)
    );
    // A restart policy needs no cgroup.
    ok(&podbag(&sb, &["update", "--restart", "always", "bare"]));
    ok(&podbag(&sb, &["rm", "-f", "bare"]));
}
