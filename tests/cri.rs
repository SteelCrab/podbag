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
            "labels": {"tier": "test"},
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
            "labels": {"kind": "worker"},
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
    // A pod's label selects it, including an empty pod; container labels are independent.
    let mut empty_config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pod_json).unwrap()).unwrap();
    empty_config["metadata"]["uid"] = "empty".into();
    empty_config["metadata"]["name"] = "empty".into();
    let empty_file = dir.join("empty.json");
    std::fs::write(&empty_file, empty_config.to_string()).unwrap();
    let empty = ok(&crictl(&socket, &["runp", empty_file.to_str().unwrap()]));
    let list: serde_json::Value = serde_json::from_str(&ok(&crictl(
        &socket,
        &["statsp", "--label", "tier=test", "-o", "json"],
    )))
    .unwrap();
    let pods = list["stats"].as_array().unwrap();
    assert_eq!(pods.len(), 2);
    let populated = pods.iter().find(|p| p["attributes"]["id"] == pod).unwrap();
    let members = populated["linux"]["containers"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["attributes"]["id"], ctr);
    let vacant = pods
        .iter()
        .find(|p| p["attributes"]["id"] == empty)
        .unwrap();
    assert!(
        vacant["linux"]["containers"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    for (label, expected) in [("kind=worker", 1), ("kind=other", 0)] {
        let stats: serde_json::Value = serde_json::from_str(&ok(&crictl(
            &socket,
            &["stats", "--pod", &pod, "--label", label, "-o", "json"],
        )))
        .unwrap();
        assert_eq!(stats["stats"].as_array().map_or(0, Vec::len), expected);
    }
    ok(&crictl(&socket, &["stopp", &empty]));
    ok(&crictl(&socket, &["rmp", &empty]));

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
