#![cfg(target_os = "linux")]
//! Pulling images from a fake registry and running containers from them.
//! Runs rootless (`dev/vm.sh test`) and as root (`dev/vm.sh test-root`).

mod common;
use common::registry::{FakeRegistry, TarEntry, TestImage, alpine_layer, layer};
use common::*;
use std::process::{Output, Stdio};

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn alpine_with(extra: Vec<TarEntry>, config: serde_json::Value) -> TestImage {
    TestImage {
        layers: vec![alpine_layer(), layer(&extra)],
        config,
    }
}

fn name(reg: &FakeRegistry, repo: &str) -> String {
    format!("{}/{repo}:v1", reg.host)
}

fn openpod(sb: &Sandbox, args: &[&str]) -> Output {
    sb.openpod().args(args).output().unwrap()
}

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

/// The Objects size from `openpod system df`, in bytes.
fn object_bytes(sb: &Sandbox) -> f64 {
    let out = ok(&openpod(sb, &["system", "df"]));
    let line = out
        .lines()
        .find(|l| l.starts_with("Objects"))
        .unwrap()
        .to_string();
    let size = line.split_whitespace().last().unwrap();
    let split = size.find(|c: char| c.is_ascii_alphabetic()).unwrap();
    let (n, unit) = size.split_at(split);
    let mult = match unit {
        "B" => 1.0,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        other => panic!("unit {other}"),
    };
    n.parse::<f64>().unwrap() * mult
}

/// 1 MiB of bytes that gzip cannot shrink, the same for the same seed.
fn noise(seed: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..1 << 20)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn pull_then_run_uses_image_config() {
    let reg = FakeRegistry::start(false);
    let config = serde_json::json!({
        "Entrypoint": ["/bin/sh", "-c"],
        "Cmd": ["echo $GREETING from $(pwd) as $(id -u)"],
        "Env": [PATH, "GREETING=hi"],
        "WorkingDir": "/app"
    });
    reg.push(
        "test/app",
        "v1",
        &alpine_with(
            vec![TarEntry::Dir {
                path: "app".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
            }],
            config,
        ),
    );
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "test/app")]));
    assert_eq!(
        ok(&openpod(&sb, &["run", &name(&reg, "test/app")])),
        "hi from /app as 0"
    );
}

#[test]
fn command_after_the_image_replaces_cmd() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "t",
        "v1",
        &alpine_with(
            vec![],
            serde_json::json!({"Cmd": ["cat", "/etc/alpine-release"], "Env": [PATH]}),
        ),
    );
    let sb = Sandbox::new();
    assert_eq!(
        ok(&openpod(&sb, &["run", &name(&reg, "t"), "echo", "x"])),
        "x"
    );
}

#[test]
fn entrypoint_flag_replaces_entrypoint_and_cmd() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "t",
        "v1",
        &alpine_with(
            vec![],
            serde_json::json!({"Entrypoint": ["/bin/false"], "Cmd": ["z"], "Env": [PATH]}),
        ),
    );
    let sb = Sandbox::new();
    assert_eq!(
        ok(&openpod(
            &sb,
            &["run", "--entrypoint", "/bin/echo", &name(&reg, "t"), "y"]
        )),
        "y"
    );
}

fn user_image() -> TestImage {
    let root = std::path::PathBuf::from(rootfs());
    let passwd = std::fs::read_to_string(root.join("etc/passwd")).unwrap()
        + "app:x:101:101::/home/app:/bin/sh\n";
    let group = std::fs::read_to_string(root.join("etc/group")).unwrap() + "app:x:101:\n";
    alpine_with(
        vec![
            TarEntry::file("etc/passwd", passwd.as_bytes()),
            TarEntry::file("etc/group", group.as_bytes()),
            TarEntry::Dir {
                path: "data".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
            TarEntry::File {
                path: "data/owned".into(),
                data: b"secret".to_vec(),
                mode: 0o640,
                uid: 101,
                gid: 101,
            },
        ],
        serde_json::json!({"User": "app", "Env": [PATH], "Cmd": ["sh", "-c", "id -u; stat -c '%u:%g %a' /data/owned; echo $HOME"]}),
    )
}

#[test]
fn image_user_and_file_ownership() {
    let reg = FakeRegistry::start(false);
    reg.push("u", "v1", &user_image());
    let sb = Sandbox::new();
    assert_eq!(
        ok(&openpod(&sb, &["run", &name(&reg, "u")])),
        "101\n101:101 640\n/home/app"
    );
}

#[test]
fn user_flag_overrides_the_image_user() {
    let reg = FakeRegistry::start(false);
    reg.push("u", "v1", &user_image());
    let sb = Sandbox::new();
    assert_eq!(
        ok(&openpod(
            &sb,
            &["run", "-u", "0", &name(&reg, "u"), "id", "-u"]
        )),
        "0"
    );
}

#[test]
fn whiteouts_and_opaque_dirs_apply() {
    let reg = FakeRegistry::start(false);
    let image = TestImage {
        layers: vec![
            alpine_layer(),
            layer(&[
                TarEntry::Dir {
                    path: "opt".into(),
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                },
                TarEntry::file("opt/a", b"a"),
            ]),
            layer(&[
                TarEntry::Opaque("opt".into()),
                TarEntry::file("opt/b", b"b"),
                TarEntry::Whiteout("etc/alpine-release".into()),
            ]),
        ],
        config: serde_json::json!({"Env": [PATH], "Cmd": ["sh", "-c", "ls /opt; test -e /etc/alpine-release; echo $?"]}),
    };
    reg.push("w", "v1", &image);
    let sb = Sandbox::new();
    assert_eq!(ok(&openpod(&sb, &["run", &name(&reg, "w")])), "b\n1");
}

#[test]
fn hardlinks_share_an_inode() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "h",
        "v1",
        &alpine_with(
            vec![
                TarEntry::file("h1", b"x"),
                TarEntry::Hardlink {
                    path: "h2".into(),
                    target: "h1".into(),
                },
            ],
            serde_json::json!({"Env": [PATH], "Cmd": ["stat", "-c", "%i", "/h1", "/h2"]}),
        ),
    );
    let sb = Sandbox::new();
    let out = ok(&openpod(&sb, &["run", &name(&reg, "h")]));
    let inodes: Vec<&str> = out.lines().collect();
    assert_eq!(inodes.len(), 2);
    assert_eq!(inodes[0], inodes[1]);
}

#[test]
fn shared_layer_is_downloaded_once() {
    let reg = FakeRegistry::start(false);
    let a = alpine_with(vec![TarEntry::file("a", b"a")], serde_json::json!({}));
    let b = alpine_with(vec![TarEntry::file("b", b"b")], serde_json::json!({}));
    reg.push("a", "v1", &a);
    reg.push("b", "v1", &b);
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "a")]));
    ok(&openpod(&sb, &["pull", &name(&reg, "b")]));
    let alpine_blob = format!("GET /v2/b/blobs/{}", FakeRegistry::layer_digests(&b)[0]);
    let alpine_blob_a = format!("GET /v2/a/blobs/{}", FakeRegistry::layer_digests(&a)[0]);
    let fetched = reg
        .requests()
        .iter()
        .filter(|r| **r == alpine_blob || **r == alpine_blob_a)
        .count();
    assert_eq!(fetched, 1, "{:?}", reg.requests());
}

#[test]
fn identical_files_are_stored_once() {
    let reg = FakeRegistry::start(false);
    let big = noise(1);
    reg.push(
        "a",
        "v1",
        &TestImage {
            layers: vec![layer(&[TarEntry::file("big", &big)])],
            config: serde_json::json!({}),
        },
    );
    reg.push(
        "b",
        "v1",
        &TestImage {
            layers: vec![layer(&[
                TarEntry::file("other/big", &big),
                TarEntry::file("marker", b"b"),
            ])],
            config: serde_json::json!({}),
        },
    );
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "a")]));
    let first = object_bytes(&sb);
    ok(&openpod(&sb, &["pull", &name(&reg, "b")]));
    let growth = object_bytes(&sb) - first;
    assert!(first >= 1e6, "first image stored {first} bytes");
    assert!(growth < 1e5, "the second image added {growth} bytes");
}

#[test]
fn corrupted_layer_is_rejected() {
    let reg = FakeRegistry::start(false);
    let image = alpine_with(vec![TarEntry::file("x", b"x")], serde_json::json!({}));
    reg.push("c", "v1", &image);
    reg.corrupt(&FakeRegistry::layer_digests(&image)[1]);
    let sb = Sandbox::new();
    let out = openpod(&sb, &["pull", &name(&reg, "c")]);
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("sha256 mismatch"), "{}", stderr(&out));
    assert_eq!(
        ok(&openpod(&sb, &["images"])).lines().count(),
        1,
        "only the header line"
    );
}

#[test]
fn token_auth_flow_works() {
    let reg = FakeRegistry::start(true);
    reg.push("p", "v1", &alpine_with(vec![], serde_json::json!({})));
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "p")]));
}

#[test]
fn rmi_frees_unshared_objects() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "a",
        "v1",
        &TestImage {
            layers: vec![layer(&[TarEntry::file("big", &noise(2))])],
            config: serde_json::json!({}),
        },
    );
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "a")]));
    assert!(object_bytes(&sb) >= 1e6);
    assert_eq!(
        ok(&openpod(&sb, &["rmi", &name(&reg, "a")])),
        format!("Untagged: {}", name(&reg, "a"))
    );
    assert!(object_bytes(&sb) < 1e5);
    assert_eq!(ok(&openpod(&sb, &["images"])).lines().count(), 1);
}

#[test]
fn store_limit_evicts_the_least_recently_used_image() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "a",
        "v1",
        &TestImage {
            layers: vec![layer(&[TarEntry::file(
                "a",
                &[noise(3), noise(4)].concat(),
            )])],
            config: serde_json::json!({}),
        },
    );
    reg.push(
        "b",
        "v1",
        &TestImage {
            layers: vec![layer(&[TarEntry::file(
                "b",
                &[noise(5), noise(6)].concat(),
            )])],
            config: serde_json::json!({}),
        },
    );
    let sb = Sandbox::with_env("OPENPOD_STORE_LIMIT", "3MB");
    ok(&openpod(&sb, &["pull", &name(&reg, "a")]));
    ok(&openpod(&sb, &["pull", &name(&reg, "b")]));
    let images = ok(&openpod(&sb, &["images"]));
    assert!(images.contains(&name(&reg, "b")), "{images}");
    assert!(!images.contains(&name(&reg, "a")), "{images}");
}

#[test]
fn image_used_by_a_running_container_is_kept() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "a",
        "v1",
        &alpine_with(vec![], serde_json::json!({"Env": [PATH]})),
    );
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "a")]));
    let mut running = sb
        .openpod()
        .args(["run", &name(&reg, "a"), "sleep", "5"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let containers = sb.data().join("containers");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::fs::read_dir(&containers).unwrap().count() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the container did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = openpod(&sb, &["rmi", &name(&reg, "a")]);
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("uses it"), "{}", stderr(&out));
    assert_eq!(running.wait().unwrap().code(), Some(0));
}

#[test]
fn run_pulls_a_missing_image() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "a",
        "v1",
        &alpine_with(vec![], serde_json::json!({"Env": [PATH]})),
    );
    let sb = Sandbox::new();
    let out = openpod(&sb, &["run", &name(&reg, "a"), "true"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("Pulling"), "{}", stderr(&out));
}

#[test]
fn container_files_owned_by_other_users_are_cleaned_up() {
    let reg = FakeRegistry::start(false);
    reg.push("u", "v1", &user_image());
    let sb = Sandbox::new();
    ok(&openpod(
        &sb,
        &[
            "run",
            "--rm",
            &name(&reg, "u"),
            "sh",
            "-c",
            "mkdir /tmp/d && touch /tmp/d/x",
        ],
    ));
    let left: Vec<_> = std::fs::read_dir(sb.data().join("containers"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn oversized_blob_is_rejected() {
    let reg = FakeRegistry::start(false);
    let image = alpine_with(vec![TarEntry::file("x", b"x")], serde_json::json!({}));
    reg.push("o", "v1", &image);
    reg.pad(&FakeRegistry::layer_digests(&image)[1], 1 << 20);
    let sb = Sandbox::new();
    let out = openpod(&sb, &["pull", &name(&reg, "o")]);
    assert_eq!(out.status.code(), Some(125));
    assert!(
        stderr(&out).contains("bytes, the manifest says"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn failed_pull_leaves_no_objects() {
    let reg = FakeRegistry::start(false);
    let image = TestImage {
        layers: vec![
            layer(&[TarEntry::file("big", &noise(7))]),
            layer(&[TarEntry::file("x", b"x")]),
        ],
        config: serde_json::json!({}),
    };
    reg.push("f", "v1", &image);
    reg.corrupt(&FakeRegistry::layer_digests(&image)[1]);
    let sb = Sandbox::new();
    assert_eq!(
        openpod(&sb, &["pull", &name(&reg, "f")]).status.code(),
        Some(125)
    );
    assert!(
        object_bytes(&sb) < 1e5,
        "a failed pull left {} bytes",
        object_bytes(&sb)
    );
}

#[test]
fn run_is_not_blocked_by_a_slow_pull() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "a",
        "v1",
        &alpine_with(vec![], serde_json::json!({"Env": [PATH]})),
    );
    let slow = alpine_with(vec![TarEntry::file("s", b"s")], serde_json::json!({}));
    reg.push("slow", "v1", &slow);
    let stalled = FakeRegistry::layer_digests(&slow)[1].clone();
    reg.stall(&stalled);
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "a")]));

    let mut pull = sb
        .openpod()
        .args(["pull", &name(&reg, "slow")])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !reg.requests().iter().any(|r| r.ends_with(&stalled)) {
        assert!(
            std::time::Instant::now() < deadline,
            "the slow pull never reached the stalled layer"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let mut run = sb
        .openpod()
        .args(["run", &name(&reg, "a"), "true"])
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(s) = run.try_wait().unwrap() {
            break Some(s);
        }
        if std::time::Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let _ = run.kill();
    kill_tree(&mut pull);
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "run waited for an unrelated pull"
    );
}

/// Stop a spawned openpod (through sudo in root mode) and every process below it.
/// Only this test's processes: other tests run in parallel.
fn kill_tree(child: &mut std::process::Child) {
    fn descendants(pid: u32, out: &mut Vec<u32>) {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return;
        };
        for t in tasks.flatten() {
            let kids = std::fs::read_to_string(t.path().join("children")).unwrap_or_default();
            for k in kids.split_whitespace().filter_map(|k| k.parse().ok()) {
                out.push(k);
                descendants(k, out);
            }
        }
    }
    let mut pids = Vec::new();
    descendants(child.id(), &mut pids);
    let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
    if !pids.is_empty() {
        let mut kill = if root_mode() {
            let mut c = std::process::Command::new("sudo");
            c.args(["-n", "kill", "-KILL"]);
            c
        } else {
            let mut c = std::process::Command::new("kill");
            c.arg("-KILL");
            c
        };
        let _ = kill.args(&pids).status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn check_image_passes_a_clean_image() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "clean",
        "v1",
        &alpine_with(vec![], serde_json::json!({"Env": [PATH]})),
    );
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "clean")]));
    let out = ok(&openpod(&sb, &["check-image", &name(&reg, "clean")]));
    assert!(out.contains("the image has musl"), "{out}");
    assert!(out.contains("no problems found"), "{out}");
}

#[test]
fn check_image_finds_a_glibc_program_in_a_musl_image() {
    // The VM's own programs are built for glibc.
    let glibc_program = std::fs::read("/usr/bin/true").unwrap();
    let reg = FakeRegistry::start(false);
    let image = alpine_with(
        vec![TarEntry::File {
            path: "usr/local/bin/app".into(),
            data: glibc_program,
            mode: 0o755,
            uid: 0,
            gid: 0,
        }],
        serde_json::json!({"Env": [PATH]}),
    );
    reg.push("mixed", "v1", &image);
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &name(&reg, "mixed")]));
    let out = openpod(&sb, &["check-image", &name(&reg, "mixed")]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    let errors: Vec<&str> = text.lines().filter(|l| l.starts_with("error:")).collect();
    assert_eq!(errors.len(), 1, "{text}");
    assert!(
        errors[0].contains("/usr/local/bin/app") && errors[0].contains("glibc"),
        "{text}"
    );
    assert!(text.contains("fix: ") && text.contains("musl"), "{text}");
}

#[test]
fn check_image_needs_a_pulled_image() {
    let sb = Sandbox::new();
    let out = openpod(&sb, &["check-image", "nothere:v1"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("pull"), "{}", stderr(&out));
}

#[test]
fn restart_keeps_the_writable_layer() {
    let reg = FakeRegistry::start(false);
    reg.push(
        "w",
        "v1",
        &alpine_with(
            vec![],
            serde_json::json!({"Env": [PATH], "Cmd": ["sh", "-c", "echo x >> /state; wc -l < /state"]}),
        ),
    );
    let sb = Sandbox::new();
    ok(&openpod(
        &sb,
        &["run", "-d", "--name", "w", &name(&reg, "w")],
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let info: serde_json::Value =
            serde_json::from_str(&ok(&openpod(&sb, &["inspect", "w"]))).unwrap();
        if info["state"]["status"] == "exited" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{info}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(ok(&openpod(&sb, &["start", "-a", "w"])), "2");
    ok(&openpod(&sb, &["rm", "w"]));
}
