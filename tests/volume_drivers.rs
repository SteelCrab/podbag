#![cfg(target_os = "linux")]

mod common;

use common::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Driver {
    calls: Vec<(String, Value)>,
    drop_create_reply: bool,
    fail_unmount: bool,
    mounted: BTreeSet<String>,
}

fn serve(listener: UnixListener, shared: Arc<Mutex<Driver>>, backend: PathBuf) {
    for connected in listener.incoming() {
        let mut stream = connected.unwrap();
        let mut request = BufReader::new(&mut stream);
        let mut line = String::new();
        request.read_line(&mut line).unwrap();
        let method = line.split_ascii_whitespace().nth(1).unwrap().to_string();
        let mut length = 0;
        let mut plugin_media = false;
        loop {
            line.clear();
            request.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = value.trim().parse::<usize>().unwrap();
            }
            plugin_media |= line.trim() == "Content-Type: application/vnd.docker.plugins.v1.1+json";
        }
        let mut payload = vec![0; length];
        request.read_exact(&mut payload).unwrap();
        let body: Value = serde_json::from_slice(&payload).unwrap();
        let reply = if plugin_media {
            let mut driver = shared.lock().unwrap();
            driver.calls.push((method.clone(), body.clone()));
            let id = body["ID"].as_str().unwrap_or_default();
            match method.as_str() {
                "/Plugin.Activate" => json!({"Implements":["VolumeDriver"]}),
                "/VolumeDriver.Create" if driver.drop_create_reply => {
                    driver.drop_create_reply = false;
                    continue;
                }
                "/VolumeDriver.Create" => json!({"Err":""}),
                "/VolumeDriver.Mount" if driver.mounted.insert(id.to_string()) => {
                    json!({"Err":"", "Mountpoint":backend})
                }
                "/VolumeDriver.Mount" => json!({"Err":"old mount was not released"}),
                "/VolumeDriver.Unmount" if driver.fail_unmount => {
                    driver.fail_unmount = false;
                    json!({"Err":"temporary failure"})
                }
                "/VolumeDriver.Unmount" => {
                    driver.mounted.remove(id);
                    json!({"Err":""})
                }
                "/VolumeDriver.Remove" if driver.mounted.is_empty() => json!({"Err":""}),
                _ => json!({"Err":"unexpected operation"}),
            }
        } else {
            json!({"Err":"missing plugin media type"})
        };
        let response = reply.to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .unwrap();
        if method == "/VolumeDriver.Remove" && reply["Err"] == "" && body["Name"] == "durable" {
            return;
        }
    }
}

fn commands(sb: &Sandbox, args: &[&str]) -> String {
    let output = sb.podbag().args(args).output().unwrap();
    assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    stdout(&output)
}

fn configure(sb: &mut Sandbox) -> (UnixListener, PathBuf) {
    let config = sb.tmp.path().join("config");
    let registry = config.join("volume-drivers");
    std::fs::create_dir_all(&registry).unwrap();
    let listener = UnixListener::bind(registry.join("fixture.sock")).unwrap();
    assert!(
        Command::new("sudo")
            .args(["-n", "chown", "0:0"])
            .arg(&registry)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("sudo")
            .args(["-n", "chmod", "0755"])
            .arg(&registry)
            .status()
            .unwrap()
            .success()
    );
    sb.env
        .push(("PODBAG_CONFIG_DIR".into(), config.display().to_string()));
    let backend = sb.tmp.path().join("backend");
    std::fs::create_dir(&backend).unwrap();
    (listener, backend)
}

fn rpc(driver: &Arc<Mutex<Driver>>, expected: &[&str]) {
    let driver = driver.lock().unwrap();
    assert_eq!(
        driver
            .calls
            .iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn volume_driver_lifecycle_uses_a_real_container_mount() {
    if !root_mode() {
        return;
    }
    let mut sb = Sandbox::new();
    let (listener, backend) = configure(&mut sb);
    let driver = Arc::new(Mutex::new(Driver::default()));
    let server = {
        let shared = Arc::clone(&driver);
        let backend = backend.clone();
        std::thread::spawn(move || serve(listener, shared, backend))
    };
    let api = api::Service::start(&sb, &[]);
    // A lost Create reply for a generated name: the driver may have the volume, so its
    // record stays, to be seen and removed.
    driver.lock().unwrap().drop_create_reply = true;
    let (_, headers) = api::curl(
        &api.socket,
        &[
            "-H",
            "Content-Type: application/json",
            "-d",
            r#"{"Driver":"fixture"}"#,
            "/volumes/create",
        ],
    );
    assert!(headers.contains("500"), "{headers}");
    let generated = commands(&sb, &["volume", "ls", "-q"]);
    let generated = generated.trim();
    assert!(
        !generated.is_empty() && !generated.contains('\n'),
        "{generated}"
    );
    {
        let d = driver.lock().unwrap();
        assert_eq!(d.calls[1].1["Name"], generated);
    }
    commands(&sb, &["volume", "rm", generated]);
    rpc(
        &driver,
        &[
            "/Plugin.Activate",
            "/VolumeDriver.Create",
            "/VolumeDriver.Remove",
        ],
    );
    driver.lock().unwrap().calls.clear();
    let create = || {
        api::curl(
            &api.socket,
            &[
                "-H",
                "Content-Type: application/json",
                "-d",
                r#"{"Name":"durable","Driver":"fixture","DriverOpts":{"pool":"dev"}}"#,
                "/volumes/create",
            ],
        )
    };
    driver.lock().unwrap().drop_create_reply = true;
    let (_, headers) = create();
    assert!(headers.contains("500"), "{headers}");
    rpc(&driver, &["/Plugin.Activate", "/VolumeDriver.Create"]);
    driver.lock().unwrap().calls.clear();
    let (body, headers) = create();
    assert!(headers.contains("201"), "{headers} {body}");
    let details: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(details["Driver"], "fixture");
    assert_eq!(details["Options"]["pool"], "dev");
    let (pruned, headers) = api::curl(
        &api.socket,
        &[
            "-X",
            "POST",
            "/volumes/prune?filters=%7B%22all%22:%7B%22true%22:true%7D%7D",
        ],
    );
    assert!(headers.contains("200"), "{headers} {pruned}");
    let pruned: Value = serde_json::from_str(&pruned).unwrap();
    assert!(pruned["VolumesDeleted"].is_null());
    let rootfs = rootfs();
    commands(
        &sb,
        &[
            "run",
            "-d",
            "--name",
            "writer",
            "--network",
            "none",
            "--rootfs",
            &rootfs,
            "-v",
            "durable:/mnt",
            "-v",
            "durable:/media:ro",
            "sleep",
            "60",
        ],
    );
    commands(
        &sb,
        &[
            "exec",
            "writer",
            "sh",
            "-ec",
            "echo saved > /mnt/data; cat /media/data; ! touch /media/rejected",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(backend.join("data")).unwrap(),
        "saved\n"
    );
    assert!(!Path::new(&backend).join("rejected").exists());
    let busy = sb
        .podbag()
        .args(["volume", "rm", "durable"])
        .output()
        .unwrap();
    assert!(!busy.status.success());
    rpc(
        &driver,
        &[
            "/Plugin.Activate",
            "/VolumeDriver.Create",
            "/VolumeDriver.Mount",
        ],
    );

    driver.lock().unwrap().fail_unmount = true;
    commands(&sb, &["stop", "writer"]);
    commands(&sb, &["start", "writer"]);
    let persisted = commands(&sb, &["exec", "writer", "cat", "/mnt/data"]);
    assert_eq!(persisted, "saved");
    commands(&sb, &["rm", "-f", "writer"]);
    let bad = sb
        .podbag()
        .args([
            "run",
            "--name",
            "bad",
            "--network",
            "none",
            "--rootfs",
            &rootfs,
            "-v",
            "durable:/mnt",
            "missing-command",
        ])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("missing-command"));
    commands(&sb, &["volume", "rm", "durable"]);
    server.join().unwrap();
    assert!(backend.join("data").exists());
    rpc(
        &driver,
        &[
            "/Plugin.Activate",
            "/VolumeDriver.Create",
            "/VolumeDriver.Mount",
            "/VolumeDriver.Unmount",
            "/VolumeDriver.Unmount",
            "/VolumeDriver.Mount",
            "/VolumeDriver.Unmount",
            "/VolumeDriver.Mount",
            "/VolumeDriver.Unmount",
            "/VolumeDriver.Remove",
        ],
    );
    let d = driver.lock().unwrap();
    assert_eq!(
        d.calls[1].1,
        json!({"Name":"durable","Opts":{"pool":"dev"}})
    );
    assert_eq!(d.calls[2].1["ID"], d.calls[5].1["ID"]);
    assert_eq!(d.calls[7].1["ID"], d.calls[8].1["ID"]);
    assert!(d.mounted.is_empty());
    println!(
        "Unix-socket plugin Create retry, duplicate-mount coalescing, real bind access, stop/restart recovery, failed-start Unmount and Remove passed."
    );
}

#[test]
fn a_rootless_engine_refuses_external_drivers() {
    if root_mode() {
        return;
    }
    let sb = Sandbox::new();
    let result = sb
        .podbag()
        .args(["volume", "create", "--driver", "fixture", "durable"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(stderr(&result).contains("external volume drivers require a rootful Linux engine"));
}
