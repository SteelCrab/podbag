#![cfg(target_os = "linux")]

mod common;
use common::registry::{FakeRegistry, TestImage};
use common::*;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

fn tunnel_proxy(registry: &str) -> (String, Arc<AtomicUsize>) {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = socket.local_addr().unwrap().to_string();
    let registry = registry.to_string();
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    thread::spawn(move || {
        for client in socket.incoming() {
            let Ok(mut client) = client else { continue };
            let registry = registry.clone();
            let requests = requests.clone();
            thread::spawn(move || {
                let mut request = BufReader::new(&mut client);
                let mut line = String::new();
                request.read_line(&mut line).unwrap();
                assert_eq!(line.trim(), format!("CONNECT {registry} HTTP/1.1"));
                loop {
                    line.clear();
                    if request.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                requests.fetch_add(1, Ordering::Relaxed);
                let mut origin = TcpStream::connect(registry).unwrap();
                client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .unwrap();
                let mut incoming = client.try_clone().unwrap();
                let mut outgoing = origin.try_clone().unwrap();
                thread::scope(|scope| {
                    scope.spawn(move || {
                        let _ = std::io::copy(&mut incoming, &mut outgoing);
                        let _ = outgoing.shutdown(Shutdown::Write);
                    });
                    let _ = std::io::copy(&mut origin, &mut client);
                    let _ = client.shutdown(Shutdown::Write);
                });
            });
        }
    });
    (endpoint, count)
}

fn command(sb: &Sandbox, args: &[&str]) -> Output {
    sb.podbag().args(args).output().unwrap()
}

fn save(sb: &Sandbox, data: serde_json::Value) -> Output {
    let mut child = sb
        .podbag()
        .args(["system", "registry-proxy", "--set"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write!(child.stdin.take().unwrap(), "{data}").unwrap();
    child.wait_with_output().unwrap()
}

fn success(out: &Output) {
    assert!(out.status.success(), "{}", stderr(out));
}

#[test]
fn registry_requests_follow_inherited_direct_manual_and_bypass_modes() {
    let registry = FakeRegistry::start(true);
    let (proxy, requests) = tunnel_proxy(&registry.host);
    let counter = || requests.load(Ordering::Relaxed);
    let mut sb = Sandbox::new();
    sb.env.extend([
        (
            "PODBAG_CONFIG_DIR".into(),
            sb.tmp.path().join("config").to_string_lossy().into_owned(),
        ),
        (
            "ALL_PROXY".into(),
            format!("http://fixture-user:fixture-password@{proxy}"),
        ),
        ("NO_PROXY".into(), String::new()),
    ]);
    let search = || {
        let query = format!("{}/probe", registry.host);
        let out = command(&sb, &["search", &query, "--limit", "1"]);
        success(&out);
        assert!(stdout(&out).contains("probe-0"), "{}", stdout(&out));
    };
    let initial = command(&sb, &["system", "registry-proxy"]);
    success(&initial);
    let settings: serde_json::Value = serde_json::from_slice(&initial.stdout).unwrap();
    assert_eq!(
        settings,
        serde_json::json!({"mode": "inherited", "url": "", "bypass": ""})
    );
    search();
    assert_eq!(counter(), 1);

    success(&save(&sb, serde_json::json!({"mode": "direct"})));
    search();
    assert_eq!(counter(), 1, "direct mode ignores the inherited proxy");

    let bad = save(
        &sb,
        serde_json::json!({
            "mode": "manual",
            "url": format!("http://fixture-user:fixture-password@{proxy}"),
        }),
    );
    assert!(!bad.status.success());
    assert!(!stderr(&bad).contains("fixture-password"));
    assert!(!stderr(&bad).contains("fixture-user"));
    search();
    assert_eq!(
        counter(),
        1,
        "invalid settings leave the saved mode in place"
    );

    success(&save(
        &sb,
        serde_json::json!({"mode": "manual", "url": format!("http://{proxy}")}),
    ));
    search();
    assert_eq!(counter(), 2);

    registry.push(
        "sample",
        "v1",
        &TestImage {
            layers: vec![],
            config: serde_json::json!({"Cmd": ["true"]}),
        },
    );
    let image = format!("{}/sample:v1", registry.host);
    success(&command(&sb, &["pull", &image]));
    assert!(counter() > 2, "pull uses the same CONNECT proxy");
    assert!(
        registry
            .requests()
            .iter()
            .any(|request| request.starts_with("GET /token")),
        "the registry issued a bearer token over the connection"
    );

    let uploaded = format!("{}/sample:copied", registry.host);
    success(&command(&sb, &["tag", &image, &uploaded]));
    let before_push = counter();
    success(&command(&sb, &["push", &uploaded]));
    assert!(counter() > before_push, "push uses the same CONNECT proxy");

    let before_bypass = counter();
    success(&save(
        &sb,
        serde_json::json!({
            "mode": "manual",
            "url": format!("http://{proxy}"),
            "bypass": "  localhost, 127.0.0.1   ",
        }),
    ));
    search();
    assert_eq!(
        counter(),
        before_bypass,
        "rules bypass the configured proxy"
    );
    let configured = command(&sb, &["system", "registry-proxy"]);
    success(&configured);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&configured.stdout).unwrap()["bypass"],
        "localhost, 127.0.0.1"
    );
}
