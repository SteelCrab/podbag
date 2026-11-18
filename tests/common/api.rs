//! Helpers for tests of the Docker Engine API service.

use super::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A running service on a socket in the sandbox; stopped on drop.
pub struct Service {
    child: Child,
    pub socket: PathBuf,
}

impl Service {
    pub fn start(sb: &Sandbox, extra: &[&str]) -> Service {
        let socket = sb.tmp.path().join("docker.sock");
        let child = sb
            .openpod()
            .args(["system", "service", "--socket"])
            .arg(&socket)
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let service = Service { child, socket };
        assert!(
            wait_for(Duration::from_secs(10), || curl(
                &service.socket,
                &["/_ping"]
            )
            .0 == "OK"),
            "the service did not answer"
        );
        service
    }

    pub fn exited_within(&mut self, limit: Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status.code().unwrap_or(-1));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // SIGTERM: in root mode the child is sudo, which passes it on.
        // SAFETY: a signal to our own child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.exited_within(Duration::from_secs(5));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn wait_for(limit: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// `curl` on the socket (as root in root mode: the socket is root's). Returns (body, headers).
pub fn curl(socket: &Path, args: &[&str]) -> (String, String) {
    let mut cmd = if root_mode() {
        let mut c = Command::new("sudo");
        c.args(["-n", "curl"]);
        c
    } else {
        Command::new("curl")
    };
    // Headers on stdout too: root may not open a file the test user made in /tmp
    // (fs.protected_regular).
    let out = cmd
        .args(["-s", "--max-time", "5", "-D", "-", "--unix-socket"])
        .arg(socket)
        .args(args.iter().map(|a| {
            if a.starts_with('/') {
                format!("http://localhost{a}")
            } else {
                a.to_string()
            }
        }))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    match text.split_once("\r\n\r\n") {
        Some((head, body)) => (body.to_string(), head.to_string()),
        None => (String::new(), text),
    }
}

/// The docker CLI against the service (as root in root mode), with its own config dir.
pub fn docker(sb: &Sandbox, service: &Service, args: &[&str]) -> Command {
    let config = sb.tmp.path().join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    let host = format!("unix://{}", service.socket.display());
    let mut cmd = if root_mode() {
        let mut c = Command::new("sudo");
        c.args(["-n", "env"])
            .arg(format!("DOCKER_CONFIG={}", config.display()))
            .arg("docker");
        c
    } else {
        let mut c = Command::new("docker");
        c.env("DOCKER_CONFIG", &config);
        c
    };
    cmd.args(["-H", &host]).args(args);
    cmd
}

/// Run docker and collect its output.
pub fn docker_out(sb: &Sandbox, service: &Service, args: &[&str]) -> Output {
    docker(sb, service, args).output().unwrap()
}

/// The docker CLI killed with SIGKILL after `secs`: it cannot close anything itself.
/// Exit code 137 means it was still running then.
pub fn docker_killed_after(sb: &Sandbox, service: &Service, secs: &str, args: &[&str]) -> Output {
    let config = sb.tmp.path().join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    let host = format!("unix://{}", service.socket.display());
    let mut cmd = if root_mode() {
        let mut c = Command::new("sudo");
        c.args(["-n", "env"])
            .arg(format!("DOCKER_CONFIG={}", config.display()));
        c.args(["timeout", "-s", "KILL", secs, "docker"]);
        c
    } else {
        let mut c = Command::new("timeout");
        c.args(["-s", "KILL", secs, "docker"]);
        c.env("DOCKER_CONFIG", &config);
        c
    };
    cmd.args(["-H", &host])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

impl Service {
    /// The service in a process group of its own (rootless only: no sudo in between),
    /// so that a test can signal the group as a terminal would.
    pub fn start_in_own_group(sb: &Sandbox, extra: &[&str]) -> Service {
        use std::os::unix::process::CommandExt;
        let socket = sb.tmp.path().join("docker.sock");
        let child = sb
            .openpod()
            .args(["system", "service", "--socket"])
            .arg(&socket)
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let service = Service { child, socket };
        assert!(
            wait_for(Duration::from_secs(10), || curl(
                &service.socket,
                &["/_ping"]
            )
            .0 == "OK"),
            "the service did not answer"
        );
        service
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}
