#![cfg(target_os = "linux")]
//! Networking that needs no internet: modes, addresses, generated files, names, ports.

mod common;
use common::*;
use std::process::Output;

fn openpod(sb: &Sandbox, args: &[&str]) -> Output {
    sb.openpod().args(args).output().unwrap()
}

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

#[test]
fn hostname_and_hosts_are_generated() {
    let sb = Sandbox::new();
    let script = "cat /etc/hostname; grep -c 'box' /etc/hosts; grep -c nameserver /etc/resolv.conf";
    let out = ok(&sb.run_with(&["--hostname", "box"], &["sh", "-c", script]));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "box", "{out}");
    assert!(lines[1].parse::<u32>().unwrap() >= 1, "{out}");
    assert!(lines[2].parse::<u32>().unwrap() >= 1, "{out}");
    // The image's own files are not changed.
    let image_hosts = std::fs::read_to_string(std::path::Path::new(&rootfs()).join("etc/hosts"))
        .unwrap_or_default();
    assert!(!image_hosts.contains("box"), "{image_hosts}");
}

#[test]
fn a_user_mount_on_etc_hosts_wins() {
    let sb = Sandbox::new();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("myhosts");
    std::fs::write(&file, "10.1.2.3 mine\n").unwrap();
    let volume = format!("{}:/etc/hosts:ro", file.display());
    let out = ok(&sb.run_with(&["-v", &volume], &["cat", "/etc/hosts"]));
    assert_eq!(out, "10.1.2.3 mine");
}

/// The data directory's record of a network (address table).
fn network_record(sb: &Sandbox, name: &str) -> serde_json::Value {
    let path = sb.data().join("networks").join(format!("{name}.json"));
    let text = if root_mode() {
        let out = std::process::Command::new("sudo")
            .args(["-n", "cat"])
            .arg(&path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        std::fs::read_to_string(&path).unwrap_or_default()
    };
    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
}

/// The container's IPv4 address on eth0 (from inside it).
fn eth0_address(sb: &Sandbox, name: &str) -> String {
    let out = ok(&openpod(
        sb,
        &["exec", name, "sh", "-c", "ip -4 -o addr show eth0"],
    ));
    out.split_whitespace()
        .find(|w| w.contains('/'))
        .and_then(|w| w.split('/').next())
        .unwrap_or_default()
        .to_string()
}

fn start(sb: &Sandbox, name: &str, options: &[&str], command: &[&str]) {
    let rootfs = rootfs();
    let mut args = vec!["run", "-d", "--name", name];
    args.extend_from_slice(options);
    args.extend_from_slice(&["--rootfs", &rootfs]);
    args.extend_from_slice(command);
    ok(&openpod(sb, &args));
    std::thread::sleep(std::time::Duration::from_millis(200));
}

#[test]
fn none_has_only_loopback() {
    let sb = Sandbox::new();
    let out = ok(&sb.run_with(
        &["--network", "none"],
        &[
            "sh",
            "-c",
            "ls /sys/class/net; ip -o link show lo | grep -c UP",
        ],
    ));
    assert_eq!(out, "lo\n1");
}

#[test]
fn host_mode_sees_host_interfaces() {
    let sb = Sandbox::new();
    let out = ok(&sb.run_with(&["--network", "host"], &["ls", "/sys/class/net"]));
    let host: Vec<String> = std::fs::read_dir("/sys/class/net")
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for name in host {
        assert!(out.lines().any(|l| l == name), "{name} missing: {out}");
    }
}

#[test]
fn bridge_container_gets_an_address() {
    let sb = Sandbox::new();
    start(&sb, "a", &[], &["sleep", "30"]);
    let ip = eth0_address(&sb, "a");
    assert!(ip.starts_with("10.7"), "{ip}");
    let record = network_record(&sb, "bridge");
    let listed = record["containers"]
        .as_object()
        .unwrap()
        .values()
        .any(|m| m["ip"] == ip.as_str());
    assert!(listed, "{record}");
    let hosts = ok(&openpod(&sb, &["exec", "a", "cat", "/etc/hosts"]));
    assert!(hosts.contains(&ip), "{hosts}");
    ok(&openpod(&sb, &["rm", "-f", "a"]));
    let record = network_record(&sb, "bridge");
    assert!(
        record["containers"].as_object().unwrap().is_empty(),
        "{record}"
    );
}

#[test]
fn host_reaches_container_and_back() {
    if !root_mode() {
        return;
    }
    let sb = Sandbox::new();
    start(&sb, "srv", &[], &["sh", "-c", "echo hi | nc -l -p 8080"]);
    let ip = eth0_address(&sb, "srv");
    assert!(ip.starts_with("10.7"), "{ip}");
    let conn = std::net::TcpStream::connect((ip.as_str(), 8080)).unwrap();
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    // busybox nc keeps the connection open after it sent its input: read one line.
    let mut got = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(conn), &mut got).unwrap();
    assert_eq!(got, "hi\n");
    ok(&openpod(&sb, &["rm", "-f", "srv"]));
}

#[test]
fn containers_on_one_network_reach_each_other_by_ip() {
    let sb = Sandbox::new();
    start(&sb, "srv2", &[], &["sh", "-c", "echo pong | nc -l -p 9000"]);
    let ip = eth0_address(&sb, "srv2");
    assert!(ip.starts_with("10.7"), "{ip}");
    let out = ok(&sb.run_with(&[], &["sh", "-c", &format!("nc -w 2 {ip} 9000 </dev/null")]));
    assert_eq!(out, "pong");
    ok(&openpod(&sb, &["rm", "-f", "srv2"]));
}

#[test]
fn failed_start_releases_the_address() {
    let sb = Sandbox::new();
    let out = sb.run_with(&[], &["nonexistent-command"]);
    assert_eq!(out.status.code(), Some(127));
    let record = network_record(&sb, "bridge");
    assert!(
        record["containers"].as_object().unwrap().is_empty(),
        "{record}"
    );
}

#[test]
fn bridge_container_reaches_the_internet() {
    if std::env::var("OPENPOD_TEST_NETWORK").as_deref() != Ok("1") {
        return;
    }
    let sb = Sandbox::new();
    let out = ok(&sb.run_with(
        &[],
        &[
            "sh",
            "-c",
            "wget -q -O /dev/null http://example.com && echo ok",
        ],
    ));
    assert_eq!(out, "ok");
}

/// The session's holder process for this sandbox (rootless), if it runs.
fn session_pid(sb: &Sandbox) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(sb.runtime().join("session/pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    std::path::Path::new(&format!("/proc/{pid}"))
        .exists()
        .then_some(pid)
}

fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    f()
}

#[test]
fn missing_pasta_is_explained() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::with_env("OPENPOD_PASTA", "/nonexistent");
    let out = sb.run(&["true"]);
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
    assert!(stderr(&out).contains("pasta"), "{}", stderr(&out));
    assert!(stderr(&out).contains("passt"), "{}", stderr(&out));
}

#[test]
fn parallel_starts_share_one_session() {
    if root_mode() {
        return;
    }
    let sb = std::sync::Arc::new(Sandbox::new());
    let threads: Vec<_> = (0..4)
        .map(|i| {
            let sb = sb.clone();
            std::thread::spawn(move || start(&sb, &format!("p{i}"), &[], &["sleep", "30"]))
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let session = session_pid(&sb).expect("a session");
    let userns = |pid: u64| std::fs::read_link(format!("/proc/{pid}/ns/user")).unwrap();
    for i in 0..4 {
        let info: serde_json::Value =
            serde_json::from_str(&ok(&openpod(&sb, &["inspect", &format!("p{i}")]))).unwrap();
        let pid = info["state"]["pid"].as_u64().unwrap();
        assert_eq!(userns(pid), userns(u64::from(session)), "p{i}");
    }
    let mut ips: Vec<String> = (0..4)
        .map(|i| eth0_address(&sb, &format!("p{i}")))
        .collect();
    ips.sort();
    ips.dedup();
    assert_eq!(ips.len(), 4, "{ips:?}");
    for i in 0..4 {
        ok(&openpod(&sb, &["rm", "-f", &format!("p{i}")]));
    }
}

#[test]
fn session_ends_with_the_last_container() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    start(&sb, "s1", &[], &["sleep", "30"]);
    start(&sb, "s2", &[], &["sleep", "30"]);
    let session = session_pid(&sb).expect("a session");
    ok(&openpod(&sb, &["rm", "-f", "s1"]));
    assert!(std::path::Path::new(&format!("/proc/{session}")).exists());
    ok(&openpod(&sb, &["rm", "-f", "s2"]));
    assert!(
        wait_until(3, || !std::path::Path::new(&format!("/proc/{session}"))
            .exists()),
        "the session outlived its last container"
    );
}

#[test]
fn killed_shim_releases_its_address() {
    let sb = Sandbox::new();
    start(&sb, "k", &[], &["sleep", "30"]);
    let info: serde_json::Value =
        serde_json::from_str(&ok(&openpod(&sb, &["inspect", "k"]))).unwrap();
    let shim = info["state"]["shim_pid"].as_u64().unwrap().to_string();
    let killer: Vec<&str> = if root_mode() {
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
    assert!(wait_until(10, || {
        let ps = ok(&openpod(&sb, &["ps", "-a"]));
        ps.lines()
            .any(|l| l.ends_with(" k") && l.contains("Exited"))
    }));
    let record = network_record(&sb, "bridge");
    assert!(
        record["containers"].as_object().unwrap().is_empty(),
        "{record}"
    );
    if !root_mode() {
        assert!(
            wait_until(3, || session_pid(&sb).is_none()),
            "session still runs"
        );
    }
    ok(&openpod(&sb, &["rm", "k"]));
}

// Rootless containers ask pasta's DNS forwarder; root ones the host's upstream servers.
#[test]
fn bridge_resolv_conf_by_mode() {
    let sb = Sandbox::new();
    let out = ok(&sb.run(&["grep", "^nameserver", "/etc/resolv.conf"]));
    if root_mode() {
        assert!(
            !out.contains("127.0.0.53") && !out.contains("169.254.1.1"),
            "{out}"
        );
    } else {
        assert_eq!(out, "nameserver 169.254.1.1");
    }
}

// A session whose runtime directory is gone (its record lost) does not linger.
#[test]
fn orphaned_session_ends_by_itself() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    start(&sb, "o", &[], &["sleep", "30"]);
    let session = session_pid(&sb).expect("a session");
    std::fs::remove_file(sb.runtime().join("session/pid")).unwrap();
    assert!(
        wait_until(6, || !std::path::Path::new(&format!("/proc/{session}"))
            .exists()),
        "the session lingers without its record"
    );
    let _ = openpod(&sb, &["rm", "-f", "o"]);
}

fn connect_line(port: u16) -> std::io::Result<String> {
    let conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(conn), &mut line)?;
    Ok(line)
}

const TCP_SERVER: &str = "while true; do echo hi | nc -l -p 8080; done";

#[test]
fn published_tcp_port_reaches_the_container() {
    let sb = Sandbox::new();
    start(&sb, "web", &["-p", "18080:8080"], &["sh", "-c", TCP_SERVER]);
    assert_eq!(connect_line(18080).unwrap(), "hi\n");
    // The server (a shell loop around nc) needs a moment to listen again.
    assert!(
        wait_until(3, || connect_line(18080).is_ok_and(|l| l == "hi\n")),
        "a second connection"
    );
    let ps = ok(&openpod(&sb, &["ps"]));
    assert!(ps.contains("0.0.0.0:18080->8080/tcp"), "{ps}");
    ok(&openpod(&sb, &["rm", "-f", "web"]));
}

#[test]
fn published_udp_port() {
    let sb = Sandbox::new();
    let server = "while true; do nc -u -l -p 5353 -e cat; done";
    start(
        &sb,
        "dns",
        &["-p", "127.0.0.1:15353:5353/udp"],
        &["sh", "-c", server],
    );
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    socket.send_to(b"ping\n", "127.0.0.1:15353").unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = socket.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ping\n");
    ok(&openpod(&sb, &["rm", "-f", "dns"]));
}

#[test]
fn automatic_host_port_is_shown() {
    let sb = Sandbox::new();
    start(&sb, "auto", &["-p", "8080"], &["sh", "-c", TCP_SERVER]);
    let info: serde_json::Value =
        serde_json::from_str(&ok(&openpod(&sb, &["inspect", "auto"]))).unwrap();
    let port = info["state"]["ports"][0]["host_port"].as_u64().unwrap() as u16;
    assert_ne!(port, 0, "{info}");
    assert_eq!(connect_line(port).unwrap(), "hi\n");
    ok(&openpod(&sb, &["rm", "-f", "auto"]));
}

#[test]
fn published_port_closes_with_the_container() {
    let sb = Sandbox::new();
    start(&sb, "brief", &["-p", "18082:8080"], &["sleep", "1"]);
    assert!(
        wait_until(8, || std::net::TcpStream::connect(("127.0.0.1", 18082))
            .is_err()),
        "the port stayed open"
    );
    let _ = openpod(&sb, &["rm", "brief"]);
}

#[test]
fn port_in_use_is_an_error() {
    let sb = Sandbox::new();
    let _taken = std::net::TcpListener::bind("0.0.0.0:18083").unwrap();
    let out = sb.run_with(&["-p", "18083:80"], &["true"]);
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
    assert!(stderr(&out).contains("18083"), "{}", stderr(&out));
}

#[test]
fn containers_reach_each_other_by_name() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["network", "create", "app"]));
    start(&sb, "db", &["--network", "app"], &["sleep", "30"]);
    start(&sb, "web", &["--network", "app"], &["sleep", "30"]);
    ok(&openpod(&sb, &["exec", "web", "ping", "-c1", "-W2", "db"]));
    // db started first: its hosts file learned about web afterwards.
    ok(&openpod(&sb, &["exec", "db", "ping", "-c1", "-W2", "web"]));
    ok(&openpod(&sb, &["rm", "-f", "db"]));
    let hosts = ok(&openpod(&sb, &["exec", "web", "cat", "/etc/hosts"]));
    assert!(
        !hosts.contains("\tdb\n") && !hosts.ends_with("\tdb"),
        "{hosts}"
    );
    ok(&openpod(&sb, &["rm", "-f", "web"]));
    ok(&openpod(&sb, &["network", "rm", "app"]));
}

#[test]
fn default_network_has_no_names() {
    let sb = Sandbox::new();
    start(&sb, "n1", &[], &["sleep", "30"]);
    start(&sb, "n2", &[], &["sleep", "30"]);
    let hosts = ok(&openpod(&sb, &["exec", "n2", "cat", "/etc/hosts"]));
    assert!(!hosts.contains("n1"), "{hosts}");
    ok(&openpod(&sb, &["rm", "-f", "n1", "n2"]));
}

#[test]
fn network_commands() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    let id = ok(&openpod(&sb, &["network", "create", "back"]));
    assert_eq!(id.len(), 12, "{id}");
    ok(&openpod(
        &sb,
        &["network", "create", "--subnet", "172.31.7.0/24", "fixed"],
    ));
    let table = ok(&openpod(&sb, &["network", "ls"]));
    assert!(table.starts_with("NETWORK ID"), "{table}");
    for name in ["back", "bridge", "fixed", "host", "none"] {
        assert!(
            table
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name)),
            "{name}: {table}"
        );
    }
    let info: serde_json::Value =
        serde_json::from_str(&ok(&openpod(&sb, &["network", "inspect", "fixed"]))).unwrap();
    assert_eq!(info["subnet"], "172.31.7.0/24");
    assert_eq!(info["gateway"], "172.31.7.1");
    let out = openpod(
        &sb,
        &["network", "create", "--subnet", "172.31.7.128/25", "clash"],
    );
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
    start(&sb, "user", &["--network", "back"], &["sleep", "30"]);
    let out = openpod(&sb, &["network", "rm", "back"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("user"), "{}", stderr(&out));
    assert_eq!(
        openpod(&sb, &["network", "rm", "bridge"]).status.code(),
        Some(125)
    );
    ok(&openpod(&sb, &["rm", "-f", "user"]));
    assert_eq!(
        ok(&openpod(&sb, &["network", "rm", "back", "fixed"])),
        "back\nfixed"
    );
    let out = openpod(
        &sb,
        &[
            "run",
            "--rm",
            "--network",
            "gone",
            "--rootfs",
            &rootfs(),
            "true",
        ],
    );
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("gone"), "{}", stderr(&out));
}

#[test]
fn exec_into_host_network_container() {
    let sb = Sandbox::new();
    start(&sb, "h", &["--network", "host"], &["sleep", "30"]);
    ok(&openpod(&sb, &["exec", "h", "true"]));
    ok(&openpod(&sb, &["rm", "-f", "h"]));
}

// Design chapter 9's scenario, with a root filesystem instead of images.
#[test]
fn scenario_without_images() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    let html = tempfile::tempdir().unwrap();
    std::fs::write(html.path().join("index.html"), "hello from web\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(html.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    ok(&openpod(&sb, &["network", "create", "app"]));
    start(
        &sb,
        "db",
        &["--network", "app", "-v", "pgdata:/srv"],
        &["sleep", "30"],
    );
    // On /opt, which the shared root filesystem has: a new mount point would be made in it.
    let web_volume = format!("{}:/opt:ro", html.path().display());
    let server = "while true; do nc -l -p 80 < /opt/index.html; done";
    start(
        &sb,
        "web",
        &["--network", "app", "-p", "18090:80", "-v", &web_volume],
        &["sh", "-c", server],
    );
    ok(&openpod(&sb, &["exec", "web", "ping", "-c1", "-W2", "db"]));
    assert!(wait_until(3, || connect_line(18090)
        .is_ok_and(|l| l == "hello from web\n")));
    ok(&openpod(&sb, &["stop", "-t", "1", "web", "db"]));
    ok(&openpod(&sb, &["rm", "web", "db"]));
    ok(&openpod(&sb, &["network", "rm", "app"]));
}

// su, sudo -u and CI shells often have systemd-run but no user manager: a rootless bridge
// container still runs (with a warning), as before networks existed.
#[test]
fn rootless_bridge_without_a_user_manager() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let fake = sb.tmp.path().join("xdg");
    std::fs::create_dir_all(&fake).unwrap();
    let out = sb
        .openpod()
        .env("XDG_RUNTIME_DIR", &fake)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .args(["run", "--rm", "--rootfs", &rootfs(), "echo", "hi"])
        .output()
        .unwrap();
    assert_eq!(ok(&out), "hi");
    assert!(stderr(&out).contains("warning"), "{}", stderr(&out));
}

// A service the host keeps on 127.0.0.1 is not for containers (pasta must not map the
// gateway address to the host's loopback).
#[test]
fn host_loopback_is_not_reachable() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let _ = std::io::Write::write_all(&mut &conn, b"secret\n");
        }
    });
    // pasta maps 169.254.1.3 to the host's loopback (instead of the gateway address, its
    // default); the network's forward chain drops it for containers. (The gateway address
    // itself cannot be probed here: in the lima VM, 192.168.5.2 is host.lima.internal,
    // which leads to the VM's loopback ports by lima's own forwarding.)
    let probe = format!("nc -w 2 169.254.1.3 {port} </dev/null; true");
    let out = sb.run(&["sh", "-c", &probe]);
    assert!(
        !stdout(&out).contains("secret"),
        "reached the host's loopback through pasta's mapping"
    );
}

// Like Docker: containers on different networks do not reach each other.
#[test]
fn networks_are_isolated() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["network", "create", "aa"]));
    ok(&openpod(&sb, &["network", "create", "bb"]));
    start(&sb, "inb", &["--network", "bb"], &["sh", "-c", TCP_SERVER]);
    let ip = eth0_address(&sb, "inb");
    let probe = format!("nc -w 2 {ip} 8080 </dev/null; true");
    let out = ok(&sb.run_with(&["--network", "aa"], &["sh", "-c", &probe]));
    assert!(!out.contains("hi"), "network aa reached network bb: {out}");
    let out = ok(&sb.run_with(&[], &["sh", "-c", &probe]));
    assert!(
        !out.contains("hi"),
        "the default network reached network bb: {out}"
    );
    let out = ok(&sb.run_with(&["--network", "bb"], &["sh", "-c", &probe]));
    assert!(
        out.contains("hi"),
        "the same network must still work: {out}"
    );
    ok(&openpod(&sb, &["rm", "-f", "inb"]));
}

// A stale pasta.pid (pasta ended with its holder) may name another process of the user by
// now: starting a new session must not signal it.
#[test]
fn stale_pid_files_do_not_signal_other_processes() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let mut bystander = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let session = sb.runtime().join("session");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("pasta.pid"), format!("{}\n", bystander.id())).unwrap();
    std::fs::write(session.join("pid"), format!("{}\n", bystander.id())).unwrap();
    ok(&sb.run(&["true"]));
    assert!(
        bystander.try_wait().unwrap().is_none(),
        "a bystander process was killed"
    );
    let _ = bystander.kill();
}

// When a container's address goes back to the network, its published ports must already
// be closed: the address may be handed to another container at once. (--rm with many
// files keeps the shim busy after it gives the address back.)
#[test]
fn published_port_is_closed_before_the_address_is_released() {
    let sb = Sandbox::new();
    // The files go to a tmpfs on /mnt: the shared root filesystem must not keep them (a
    // second run's `mkdir` failed on the first run's directory, and made no files).
    let work =
        "sleep 1; mkdir /mnt/d && i=0 && while [ $i -lt 3000 ]; do : > /mnt/d/$i; i=$((i+1)); done";
    let rootfs = rootfs();
    let args = [
        "run",
        "-d",
        "--rm",
        "--name",
        "gone",
        "-p",
        "18084:8080",
        "--tmpfs",
        "/mnt",
        "--rootfs",
        &rootfs,
        "sh",
        "-c",
        work,
    ];
    let id = ok(&openpod(&sb, &args));
    let listed = |sb: &Sandbox| {
        network_record(sb, "bridge")["containers"]
            .get(&id)
            .is_some()
    };
    assert!(wait_until(5, || listed(&sb)), "never got an address");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while listed(&sb) {
        assert!(
            std::time::Instant::now() < deadline,
            "the address was never released"
        );
    }
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", 18084)).is_err(),
        "the port still answers after the address was released"
    );
}

// Review focus: a relayed connection that is open when the container ends gets an end
// (not a hang), and the shim finishes.
#[test]
fn open_connection_ends_with_the_container() {
    let sb = Sandbox::new();
    let server = "(sleep 600 | nc -l -p 8080) & sleep 3";
    start(&sb, "holder", &["-p", "18085:8080"], &["sh", "-c", server]);
    let conn = std::net::TcpStream::connect(("127.0.0.1", 18085)).unwrap();
    conn.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut buf = [0u8; 16];
    let got = std::io::Read::read(&mut &conn, &mut buf);
    assert!(
        matches!(&got, Ok(0))
            || matches!(&got, Err(e) if e.kind() != std::io::ErrorKind::WouldBlock && e.kind() != std::io::ErrorKind::TimedOut),
        "the connection hung: {got:?}"
    );
    assert!(wait_until(10, || ok(&openpod(&sb, &["ps", "-a"]))
        .lines()
        .any(|l| l.ends_with(" holder") && l.contains("Exited"))));
    let _ = openpod(&sb, &["rm", "holder"]);
}

#[test]
fn labels_from_the_command_line() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(
        &sb,
        &["network", "create", "--label", "a=b", "--label", "c", "lab"],
    ));
    let net: serde_json::Value =
        serde_json::from_str(&ok(&openpod(&sb, &["network", "inspect", "lab"]))).unwrap();
    assert_eq!(net["labels"], serde_json::json!({"a": "b", "c": ""}));
    let vol = ok(&openpod(
        &sb,
        &["volume", "create", "--label", "eq=x=y", "labvol"],
    ));
    assert_eq!(vol.trim(), "labvol");
    let vol: serde_json::Value =
        serde_json::from_str(&ok(&openpod(&sb, &["volume", "inspect", "labvol"]))).unwrap();
    let vol = if vol.is_array() { vol[0].clone() } else { vol };
    assert_eq!(vol["labels"], serde_json::json!({"eq": "x=y"}));
    // A label without a key is refused, and nothing is made.
    let bad = openpod(&sb, &["network", "create", "--label", "=v", "nolabel"]);
    assert_eq!(bad.status.code(), Some(125), "{}", stderr(&bad));
    assert!(stderr(&bad).contains("label"), "{}", stderr(&bad));
    assert_ne!(
        openpod(&sb, &["network", "inspect", "nolabel"])
            .status
            .code(),
        Some(0)
    );
    ok(&openpod(&sb, &["network", "rm", "lab"]));
    ok(&openpod(&sb, &["volume", "rm", "labvol"]));
}

#[test]
fn containers_find_each_other_by_alias() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["network", "create", "app"]));
    start(
        &sb,
        "db",
        &[
            "--network",
            "app",
            "--network-alias",
            "database",
            "--network-alias",
            "store",
        ],
        &["sleep", "30"],
    );
    start(&sb, "web", &["--network", "app"], &["sleep", "30"]);
    for name in ["db", "database", "store"] {
        ok(&openpod(&sb, &["exec", "web", "ping", "-c1", "-W2", name]));
    }
    // Its own aliases name itself.
    ok(&openpod(
        &sb,
        &["exec", "db", "ping", "-c1", "-W2", "store"],
    ));
    // The default network has no names, so no aliases either.
    let bad = openpod(
        &sb,
        &[
            "run",
            "--rm",
            "--network-alias",
            "x",
            "--rootfs",
            &rootfs(),
            "true",
        ],
    );
    assert_eq!(bad.status.code(), Some(125), "{}", stderr(&bad));
    assert!(stderr(&bad).contains("alias"), "{}", stderr(&bad));
    ok(&openpod(&sb, &["rm", "-f", "db", "web"]));
    ok(&openpod(&sb, &["network", "rm", "app"]));
}

#[test]
fn a_container_on_two_networks_reaches_both() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["network", "create", "front"]));
    ok(&openpod(&sb, &["network", "create", "back"]));
    start(&sb, "web", &["--network", "front"], &["sleep", "30"]);
    start(&sb, "db", &["--network", "back"], &["sleep", "30"]);
    start(
        &sb,
        "api",
        &["--network", "front", "--network", "back"],
        &["sleep", "30"],
    );
    for name in ["web", "db"] {
        ok(&openpod(&sb, &["exec", "api", "ping", "-c1", "-W2", name]));
    }
    // Each network's containers reach it; the networks stay apart.
    ok(&openpod(&sb, &["exec", "web", "ping", "-c1", "-W2", "api"]));
    ok(&openpod(&sb, &["exec", "db", "ping", "-c1", "-W2", "api"]));
    let hosts = ok(&openpod(&sb, &["exec", "web", "cat", "/etc/hosts"]));
    assert!(!hosts.contains("\tdb"), "{hosts}");
    // An interface on each network, and one default route (through the first).
    let addrs = ok(&openpod(&sb, &["exec", "api", "ip", "-o", "-4", "addr"]));
    assert!(addrs.contains("eth0") && addrs.contains("eth1"), "{addrs}");
    // (busybox's `ip route show` lists every route: pick the default ones.)
    let routes = ok(&openpod(&sb, &["exec", "api", "ip", "route"]));
    let defaults: Vec<&str> = routes
        .lines()
        .filter(|l| l.starts_with("default"))
        .collect();
    assert_eq!(defaults.len(), 1, "{routes}");
    assert!(defaults[0].contains("dev eth0"), "{routes}");
    ok(&openpod(&sb, &["rm", "-f", "web", "db", "api"]));
    ok(&openpod(&sb, &["network", "rm", "front", "back"]));
}

#[test]
fn the_addresses_of_all_networks_come_back() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["network", "create", "front"]));
    ok(&openpod(&sb, &["network", "create", "back"]));
    let members = |network: &str| {
        network_record(&sb, network)["containers"]
            .as_object()
            .map_or(0, |m| m.len())
    };
    let rootfs = rootfs();
    ok(&openpod(
        &sb,
        &[
            "run",
            "--rm",
            "--network",
            "front",
            "--network",
            "back",
            "--rootfs",
            &rootfs,
            "true",
        ],
    ));
    assert_eq!((members("front"), members("back")), (0, 0));
    // A start that fails on its second network gives the first one's address back.
    let failed = openpod(
        &sb,
        &[
            "run",
            "--rm",
            "--network",
            "front",
            "--network",
            "missing",
            "--rootfs",
            &rootfs,
            "true",
        ],
    );
    assert_ne!(failed.status.code(), Some(0));
    assert_eq!(members("front"), 0);
    ok(&openpod(&sb, &["network", "rm", "front", "back"]));
}

#[test]
fn connect_and_disconnect_a_stopped_container() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["network", "create", "front"]));
    ok(&openpod(&sb, &["network", "create", "back"]));
    start(&sb, "web", &["--network", "front"], &["sleep", "300"]);
    // A container that runs keeps its networks until it stops.
    let busy = openpod(&sb, &["network", "connect", "back", "web"]);
    assert_eq!(busy.status.code(), Some(125), "{}", stderr(&busy));
    assert!(stderr(&busy).contains("stop"), "{}", stderr(&busy));
    ok(&openpod(&sb, &["stop", "-t", "0", "web"]));
    ok(&openpod(
        &sb,
        &["network", "connect", "--alias", "w", "back", "web"],
    ));
    // Twice is a conflict.
    let twice = openpod(&sb, &["network", "connect", "back", "web"]);
    assert_eq!(twice.status.code(), Some(125), "{}", stderr(&twice));
    ok(&openpod(&sb, &["start", "web"]));
    let addrs = ok(&openpod(&sb, &["exec", "web", "ip", "-o", "-4", "addr"]));
    assert!(addrs.contains("eth1"), "{addrs}");
    ok(&openpod(&sb, &["exec", "web", "ping", "-c1", "-W2", "w"]));
    ok(&openpod(&sb, &["stop", "-t", "0", "web"]));
    ok(&openpod(&sb, &["network", "disconnect", "back", "web"]));
    // Its primary network stays.
    let primary = openpod(&sb, &["network", "disconnect", "front", "web"]);
    assert_eq!(primary.status.code(), Some(125), "{}", stderr(&primary));
    ok(&openpod(&sb, &["start", "web"]));
    let addrs = ok(&openpod(&sb, &["exec", "web", "ip", "-o", "-4", "addr"]));
    assert!(!addrs.contains("eth1"), "{addrs}");
    ok(&openpod(&sb, &["rm", "-f", "web"]));
    ok(&openpod(&sb, &["network", "rm", "front", "back"]));
}

#[test]
fn a_killed_watcher_gives_back_every_network() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["network", "create", "front"]));
    ok(&openpod(&sb, &["network", "create", "back"]));
    start(
        &sb,
        "k2",
        &["--network", "front", "--network", "back"],
        &["sleep", "30"],
    );
    let members = |network: &str| {
        network_record(&sb, network)["containers"]
            .as_object()
            .map_or(0, |m| m.len())
    };
    assert_eq!((members("front"), members("back")), (1, 1));
    // Its watcher dies without cleaning up (as at a crash or a reboot).
    let info: serde_json::Value =
        serde_json::from_str(&ok(&openpod(&sb, &["inspect", "k2"]))).unwrap();
    let watcher = info["state"]["shim_pid"].as_u64().unwrap().to_string();
    let killer: Vec<&str> = if root_mode() {
        vec!["sudo", "-n", "kill", "-KILL", &watcher]
    } else {
        vec!["kill", "-KILL", &watcher]
    };
    assert!(
        std::process::Command::new(killer[0])
            .args(&killer[1..])
            .status()
            .unwrap()
            .success()
    );
    // The next state check gives back its address on every network.
    assert!(wait_until(10, || {
        let ps = ok(&openpod(&sb, &["ps", "-a"]));
        ps.lines()
            .any(|l| l.ends_with(" k2") && l.contains("Exited"))
    }));
    assert_eq!((members("front"), members("back")), (0, 0));
    ok(&openpod(&sb, &["rm", "-f", "k2"]));
    ok(&openpod(&sb, &["network", "rm", "front", "back"]));
}

#[test]
fn connect_waits_for_the_cleanup_of_the_last_run() {
    let _turn = network_turn();
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["network", "create", "front"]));
    ok(&openpod(&sb, &["network", "create", "back"]));
    start(&sb, "w", &["--network", "front"], &["true"]);
    assert!(wait_until(10, || {
        ok(&openpod(&sb, &["ps", "-a"]))
            .lines()
            .any(|l| l.ends_with(" w") && l.contains("Exited"))
    }));
    let info: serde_json::Value =
        serde_json::from_str(&ok(&openpod(&sb, &["inspect", "w"]))).unwrap();
    let id = info["id"].as_str().unwrap().to_string();
    // The cleanup of its last run holds the lock for a moment after a stop returns.
    let mut holder = hold_shim_lock(&sb, &id, "1");
    ok(&openpod(&sb, &["network", "connect", "back", "w"]));
    let _ = holder.wait();
    ok(&openpod(&sb, &["rm", "w"]));
    ok(&openpod(&sb, &["network", "rm", "front", "back"]));
}
