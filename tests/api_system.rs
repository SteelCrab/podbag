#![cfg(target_os = "linux")]
//! System endpoints of the Docker Engine API, through the docker CLI.

mod common;
use common::api::*;
use common::*;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{Duration, Instant};

#[test]
fn docker_version_and_info() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let out = docker_out(&sb, &service, &["version"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Server:"), "{text}");
    assert!(
        text.contains("API version:") && text.contains("1.47"),
        "{text}"
    );
    let out = docker_out(&sb, &service, &["info"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Operating System:"), "{text}");
    assert!(text.contains("Server Version:"), "{text}");
}

#[test]
fn docker_system_df_runs() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let out = docker_out(&sb, &service, &["system", "df"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("Images"), "{}", stdout(&out));
}

#[test]
fn docker_events_reports_a_container_lifecycle() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let mut events = docker(
        &sb,
        &service,
        &["events", "--format", "{{.Type}} {{.Action}}"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let run = sb.run(&["sh", "-c", "sleep 1"]);
    assert_eq!(run.status.code(), Some(0), "{}", stderr(&run));
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = lines.clone();
    let out = events.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            seen.lock().unwrap().push(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !lines.lock().unwrap().iter().any(|l| l.ends_with("destroy"))
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY: a signal to our own child.
    unsafe { libc::kill(events.id() as i32, libc::SIGTERM) };
    let _ = events.wait();
    let got = lines.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![
            "container create",
            "container start",
            "container die",
            "container destroy"
        ],
        "{got:?}"
    );
}

#[test]
fn unknown_endpoints_get_404_and_unsupported_get_501() {
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let out = docker_out(&sb, &service, &["top", "abc"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("not supported by openpod"),
        "{}",
        stderr(&out)
    );
    let (body, head) = curl(&service.socket, &["/v1.47/nothing"]);
    assert!(head.contains(" 404 "), "{head}");
    assert!(body.contains("page not found"), "{body}");
}
