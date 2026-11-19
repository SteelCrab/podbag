#![cfg(target_os = "linux")]
//! `podbag system service`: the Docker Engine API socket.

mod common;
use common::api::*;
use common::*;
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn ping_answers_with_api_headers() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let (body, headers) = curl(&service.socket, &["/v1.47/_ping"]);
    assert_eq!(body, "OK");
    assert!(headers.contains("Api-Version: 1.47"), "{headers}");
    let (body, _) = curl(&service.socket, &["/v1.47/nothing/here"]);
    assert!(body.contains("page not found"), "{body}");
}

#[test]
fn the_service_exits_when_idle() {
    let sb = Sandbox::new();
    let mut service = Service::start(&sb, &["--timeout", "1"]);
    assert_eq!(service.exited_within(Duration::from_secs(6)), Some(0));
    assert!(!service.socket.exists(), "the socket is removed");
}

#[test]
fn the_service_waits_for_open_connections_before_exiting() {
    let sb = Sandbox::new();
    let mut service = Service::start(&sb, &["--timeout", "1"]);
    // A client that connects and stays (as root, through sudo: the socket is root's).
    let mut holder = if root_mode() {
        Command::new("sudo")
            .args(["-n", "python3", "-c"])
            .arg(HOLD)
            .arg(&service.socket)
            .spawn()
            .unwrap()
    } else {
        Command::new("python3")
            .arg("-c")
            .arg(HOLD)
            .arg(&service.socket)
            .spawn()
            .unwrap()
    };
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        service.exited_within(Duration::from_millis(500)),
        None,
        "exited with a client"
    );
    holder.wait().unwrap();
    assert_eq!(service.exited_within(Duration::from_secs(6)), Some(0));
}

/// Connect to the socket in argv[1] and keep the connection open for 3 seconds.
const HOLD: &str = "import socket, sys, time\ns = socket.socket(socket.AF_UNIX)\ns.connect(sys.argv[1])\ntime.sleep(3)\n";

#[test]
fn a_stale_socket_is_replaced_and_a_live_one_refused() {
    let sb = Sandbox::new();
    // A socket file nobody listens on.
    let stale = sb.tmp.path().join("docker.sock");
    let _ = std::os::unix::net::UnixListener::bind(&stale).unwrap();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let second = sb
        .podbag()
        .args(["system", "service", "--timeout", "0", "--socket"])
        .arg(&service.socket)
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(125), "{}", stderr(&second));
    assert!(
        stderr(&second).contains("already listens"),
        "{}",
        stderr(&second)
    );
    assert_eq!(curl(&service.socket, &["/_ping"]).0, "OK");
}

#[test]
fn socket_activation_uses_fd_3() {
    if Command::new("systemd-socket-activate")
        .arg("--help")
        .output()
        .is_err()
    {
        eprintln!("systemd-socket-activate is missing: skipped");
        return;
    }
    let sb = Sandbox::new();
    let socket = sb.tmp.path().join("act.sock");
    let mut wrapper = sb.podbag_bin("systemd-socket-activate");
    let mut child = wrapper
        .arg("-l")
        .arg(&socket)
        .args([BIN, "system", "service", "--timeout", "0"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(wait_for(Duration::from_secs(10), || socket.exists()));
    let answered = wait_for(Duration::from_secs(10), || {
        curl(&socket, &["/_ping"]).0 == "OK"
    });
    // SAFETY: a signal to our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    if root_mode() {
        let _ = Command::new("sudo")
            .args(["-n", "pkill", "-f"])
            .arg(socket.to_str().unwrap())
            .status();
    }
    assert!(answered, "no answer through the activated socket");
}

#[test]
fn print_units_names_the_socket() {
    let sb = Sandbox::new();
    let out = sb
        .podbag()
        .args(["system", "service", "--print-units"])
        .output()
        .unwrap();
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let socket = sb.runtime().join("docker.sock");
    assert!(
        text.contains(&format!("ListenStream={}", socket.display())),
        "{text}"
    );
    assert!(text.contains("KillMode=process"), "{text}");
}

#[test]
fn an_abandoned_wait_lets_the_service_end() {
    let sb = Sandbox::new();
    let mut service = Service::start(&sb, &["--timeout", "2"]);
    let out = sb
        .podbag()
        .args(["run", "-d", "--name", "long", "--rootfs"])
        .arg(rootfs())
        .args(["sleep", "300"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let wait = docker_killed_after(&sb, &service, "1", &["wait", "long"]);
    // timeout(1) exits 124 when it had to kill the command.
    assert!(
        matches!(wait.status.code(), Some(124 | 137)),
        "docker wait ended by itself: {}",
        stderr(&wait)
    );
    assert!(
        service.exited_within(Duration::from_secs(10)).is_some(),
        "the service stayed for a client that left"
    );
    let _ = sb.podbag().args(["rm", "-f", "long"]).output();
}

#[test]
fn running_out_of_descriptors_does_not_end_the_service() {
    if root_mode() {
        return; // the test user cannot connect to root's socket; the code is the same
    }
    let sb = Sandbox::new();
    let mut service = Service::start(&sb, &["--timeout", "0"]);
    let limited = Command::new("prlimit")
        .args(["--nofile=24:24", "--pid", &service.pid().to_string()])
        .status()
        .unwrap();
    assert!(limited.success());
    let conns: Vec<std::os::unix::net::UnixStream> = (0..40)
        .filter_map(|_| std::os::unix::net::UnixStream::connect(&service.socket).ok())
        .collect();
    assert!(conns.len() > 20, "{} connections", conns.len());
    std::thread::sleep(Duration::from_millis(600));
    drop(conns);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        service.exited_within(Duration::from_millis(100)),
        None,
        "the service ended"
    );
    assert!(
        wait_for(Duration::from_secs(5), || curl(
            &service.socket,
            &["/_ping"]
        )
        .0 == "OK"),
        "the service does not answer any more"
    );
}
