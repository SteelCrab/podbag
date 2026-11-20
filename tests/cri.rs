#![cfg(target_os = "linux")]
//! `podbag cri serve` with crictl: an image from a fake registry, a pod on the host's
//! network, a container in it, exec (direct and streamed), logs, stop and removal. As root
//! only (`dev/vm.sh test-root`), with crictl installed; skipped otherwise.

mod common;
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::process::{Child, Command, Output, Stdio};

struct Serve {
    child: Child,
    socket: String,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = crictl(&self.socket, &["rmp", "-fa"]);
        // `sudo` passes the signal on to podbag.
        let _ = Command::new("sudo")
            .args(["-n", "pkill", "-TERM", "-f"])
            .arg(format!("cri serve --socket {}", self.socket))
            .status();
        let _ = self.child.wait();
    }
}

fn crictl(socket: &str, args: &[&str]) -> Output {
    Command::new("sudo")
        .args(["-n", "crictl", "--config", "/dev/null", "--timeout", "60s"])
        .arg(format!("--runtime-endpoint=unix://{socket}"))
        .arg(format!("--image-endpoint=unix://{socket}"))
        .args(args)
        .output()
        .unwrap()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
    stdout(out)
}

fn ready() -> bool {
    if !root_mode() {
        eprintln!("skipped: the CRI runs as root (dev/vm.sh test-root)");
        return false;
    }
    let found = Command::new("crictl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !found {
        eprintln!("skipped: crictl is not installed");
    }
    found
}

#[test]
fn crictl_runs_a_pod_and_its_container() {
    if !ready() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = format!("{}/test/cri:v1", reg.host);
    reg.push(
        "test/cri",
        "v1",
        &TestImage {
            layers: vec![alpine_layer()],
            config: serde_json::json!({
                "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
                "Cmd": ["sh"]
            }),
        },
    );
    let sb = Sandbox::new();
    let dir = sb.tmp.path().join("cri");
    std::fs::create_dir_all(dir.join("net.d")).unwrap();
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    let socket = dir.join("cri.sock").display().to_string();
    let child = sb
        .podbag()
        .args(["cri", "serve", "--socket", &socket, "--cni-conf-dir"])
        .arg(dir.join("net.d"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let serve = Serve { child, socket };
    let socket = serve.socket.clone();
    for _ in 0..100 {
        if crictl(&socket, &["version"]).status.success() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    ok(&crictl(&socket, &["pull", &image]));

    let pod_json = dir.join("pod.json");
    std::fs::write(
        &pod_json,
        serde_json::json!({
            "metadata": {"name": "t", "namespace": "default", "uid": "t-1"},
            "log_directory": dir.join("logs"),
            "linux": {"security_context": {"namespace_options": {"network": 2, "pid": 1}}}
        })
        .to_string(),
    )
    .unwrap();
    let ctr_json = dir.join("ctr.json");
    std::fs::write(
        &ctr_json,
        serde_json::json!({
            "metadata": {"name": "c"},
            "image": {"image": image},
            "command": ["sh", "-c", "echo started; exec sleep 300"],
            "envs": [{"key": "GREETING", "value": "hello"}],
            "log_path": "c/0.log"
        })
        .to_string(),
    )
    .unwrap();
    let pod = ok(&crictl(&socket, &["runp", pod_json.to_str().unwrap()]));
    let ctr = ok(&crictl(
        &socket,
        &[
            "create",
            &pod,
            ctr_json.to_str().unwrap(),
            pod_json.to_str().unwrap(),
        ],
    ));
    ok(&crictl(&socket, &["start", &ctr]));
    assert_eq!(
        ok(&crictl(
            &socket,
            &["exec", "-s", &ctr, "sh", "-c", "echo $GREETING"]
        )),
        "hello"
    );
    // Streamed: through the SPDY server.
    let streamed = crictl(&socket, &["exec", &ctr, "sh", "-c", "echo out; exit 3"]);
    assert_eq!(stdout(&streamed), "out");
    // crictl ends with 1 and names the code.
    assert!(
        stderr(&streamed).contains("command terminated with exit code 3"),
        "{}",
        stderr(&streamed)
    );
    let mut logs = String::new();
    for _ in 0..50 {
        logs = stdout(&crictl(&socket, &["logs", &ctr]));
        if logs.contains("started") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(logs, "started");
    let file = std::fs::read_to_string(dir.join("logs/c/0.log")).unwrap();
    assert!(file.contains(" stdout F started\n"), "{file}");
    assert!(ok(&crictl(&socket, &["inspect", &ctr])).contains("CONTAINER_RUNNING"));
    ok(&crictl(&socket, &["stop", "-t", "1", &ctr]));
    let inspect = ok(&crictl(&socket, &["inspect", &ctr]));
    assert!(inspect.contains("CONTAINER_EXITED"), "{inspect}");
    ok(&crictl(&socket, &["rm", &ctr]));
    ok(&crictl(&socket, &["stopp", &pod]));
    ok(&crictl(&socket, &["rmp", &pod]));
    assert_eq!(ok(&crictl(&socket, &["pods", "-q"])), "");
    // Stopping what is gone succeeds.
    ok(&crictl(&socket, &["stopp", &pod]));
    drop(serve);
}
