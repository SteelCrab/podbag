#![cfg(target_os = "linux")]
//! Volumes: host paths, named and anonymous volumes, tmpfs, and the volume commands.

mod common;
use common::*;
use std::path::Path;
use std::process::Output;

fn podbag(sb: &Sandbox, args: &[&str]) -> Output {
    sb.podbag().args(args).output().unwrap()
}

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

/// `run --rm --rootfs <alpine> <options...> <command...>`
fn run(sb: &Sandbox, options: &[&str], command: &[&str]) -> Output {
    sb.run_with(options, command)
}

/// A host directory the container (root or mapped root) may write.
fn shared_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    dir
}

#[test]
fn bind_mount_shares_files() {
    let sb = Sandbox::new();
    let host = shared_dir();
    std::fs::write(host.path().join("in.txt"), "hello").unwrap();
    let volume = format!("{}:/mnt", host.path().display());
    let out = run(
        &sb,
        &["-v", &volume],
        &["sh", "-c", "cat /mnt/in.txt; echo out > /mnt/out.txt"],
    );
    assert_eq!(ok(&out), "hello");
    assert_eq!(
        std::fs::read_to_string(host.path().join("out.txt")).unwrap(),
        "out\n"
    );
}

#[test]
fn read_only_bind_refuses_writes() {
    let sb = Sandbox::new();
    let host = shared_dir();
    let volume = format!("{}:/mnt:ro", host.path().display());
    let out = run(&sb, &["-v", &volume], &["touch", "/mnt/x"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn relative_bind_and_missing_source() {
    let sb = Sandbox::new();
    let host = shared_dir();
    let out = sb
        .podbag()
        .current_dir(host.path())
        .args(["run", "--rm", "--rootfs", &rootfs(), "-v", "./sub:/srv"])
        .args(["sh", "-c", "echo made > /srv/f"])
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(
        std::fs::read_to_string(host.path().join("sub/f")).unwrap(),
        "made\n"
    );
}

#[test]
fn named_volume_persists_and_copies_image_content() {
    let sb = Sandbox::new();
    let listing = ok(&run(&sb, &["-v", "cfg:/etc/apk"], &["ls", "/etc/apk"]));
    assert!(listing.contains("repositories"), "{listing}");
    let copied = ok(&run(
        &sb,
        &["-v", "cfg:/mnt"],
        &["cat", "/mnt/repositories"],
    ));
    let original =
        std::fs::read_to_string(Path::new(&rootfs()).join("etc/apk/repositories")).unwrap();
    assert_eq!(copied, original.trim());
    ok(&run(
        &sb,
        &["-v", "cfg:/mnt"],
        &["sh", "-c", "echo kept > /mnt/k"],
    ));
    assert_eq!(
        ok(&run(&sb, &["-v", "cfg:/media"], &["cat", "/media/k"])),
        "kept"
    );
}

#[test]
fn nocopy_leaves_the_volume_empty() {
    let sb = Sandbox::new();
    let out = run(
        &sb,
        &["-v", "empty:/etc/apk:nocopy"],
        &["ls", "-A", "/etc/apk"],
    );
    assert_eq!(ok(&out), "");
}

#[test]
fn tmpfs_mount_has_its_size() {
    let sb = Sandbox::new();
    let script = "grep ' /mnt ' /proc/mounts; dd if=/dev/zero of=/mnt/f bs=1M count=2";
    let out = run(&sb, &["--tmpfs", "/mnt:size=1m"], &["sh", "-c", script]);
    let mounts = stdout(&out);
    assert!(mounts.starts_with("tmpfs /mnt tmpfs"), "{mounts}");
    assert!(mounts.contains("size=1024k"), "{mounts}");
    assert!(mounts.contains("noexec"), "{mounts}");
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("No space left"), "{}", stderr(&out));
}

#[test]
fn symlinked_target_stays_inside_the_rootfs() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("rootfs");
    assert!(
        std::process::Command::new("cp")
            .arg("-a")
            .arg(rootfs())
            .arg(&root)
            .status()
            .unwrap()
            .success()
    );
    // An image whose /data points far up and out: the mount must land inside the rootfs.
    let escape = tmp.path().join("escape");
    let link = format!("/../../../../../..{}", escape.display());
    std::os::unix::fs::symlink(&link, root.join("data")).unwrap();
    let out = sb
        .podbag()
        .args(["run", "--rm", "--rootfs"])
        .arg(&root)
        .args(["-v", "vol:/data/inner", "true"])
        .output()
        .unwrap();
    ok(&out);
    assert!(
        !escape.exists(),
        "a directory was created outside the rootfs"
    );
    let inside = root.join(escape.strip_prefix("/").unwrap()).join("inner");
    assert!(inside.is_dir(), "{} is missing", inside.display());
    // Root mode created directories in the copy as root.
    if root_mode() {
        let _ = std::process::Command::new("sudo")
            .args(["-n", "rm", "-rf"])
            .arg(&root)
            .status();
    }
}

#[test]
fn concurrent_first_use_copies_once_or_twice_safely() {
    let sb = std::sync::Arc::new(Sandbox::new());
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let sb = sb.clone();
            std::thread::spawn(move || {
                let out = run(
                    &sb,
                    &["-v", "shared:/etc/apk"],
                    &["ls", "/etc/apk/repositories"],
                );
                ok(&out);
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let copied = ok(&run(
        &sb,
        &["-v", "shared:/mnt"],
        &["cat", "/mnt/repositories"],
    ));
    let original =
        std::fs::read_to_string(Path::new(&rootfs()).join("etc/apk/repositories")).unwrap();
    assert_eq!(copied, original.trim());
}

fn volume_names(sb: &Sandbox) -> Vec<String> {
    ok(&podbag(sb, &["volume", "ls", "-q"]))
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn volume_commands() {
    let sb = Sandbox::new();
    assert_eq!(ok(&podbag(&sb, &["volume", "create", "data"])), "data");
    let table = ok(&podbag(&sb, &["volume", "ls"]));
    assert!(table.starts_with("DRIVER    VOLUME NAME"), "{table}");
    assert!(table.lines().any(|l| l == "local     data"), "{table}");
    assert_eq!(volume_names(&sb), ["data"]);
    let info: serde_json::Value =
        serde_json::from_str(&ok(&podbag(&sb, &["volume", "inspect", "data"]))).unwrap();
    assert_eq!(info["name"], "data");
    assert!(
        info["mountpoint"]
            .as_str()
            .unwrap()
            .ends_with("volumes/data/data"),
        "{info}"
    );
    assert_eq!(ok(&podbag(&sb, &["volume", "rm", "data"])), "data");
    assert_eq!(
        podbag(&sb, &["volume", "inspect", "data"]).status.code(),
        Some(125)
    );
    assert!(volume_names(&sb).is_empty());
}

#[test]
fn volume_in_use_cannot_be_removed() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    let start = [
        "run",
        "-d",
        "--name",
        "u",
        "-v",
        "busy:/mnt",
        "--rootfs",
        &rootfs,
        "sleep",
        "30",
    ];
    ok(&podbag(&sb, &start));
    let out = podbag(&sb, &["volume", "rm", "busy"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(stderr(&out).contains("u"), "{}", stderr(&out));
    ok(&podbag(&sb, &["rm", "-f", "u"]));
    ok(&podbag(&sb, &["volume", "rm", "busy"]));
}

#[test]
fn anonymous_volume_follows_rm() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    ok(&run(&sb, &["-v", "/mnt"], &["true"]));
    assert!(volume_names(&sb).is_empty(), "--rm kept its volume");
    let keep = [
        "run", "--name", "keep", "-v", "/mnt", "--rootfs", &rootfs, "true",
    ];
    ok(&podbag(&sb, &keep));
    let names = volume_names(&sb);
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].len(), 64);
    ok(&podbag(&sb, &["rm", "keep"]));
    assert_eq!(volume_names(&sb), names, "plain rm removed the volume");
    let gone = [
        "run", "--name", "gone", "-v", "/mnt", "--rootfs", &rootfs, "true",
    ];
    ok(&podbag(&sb, &gone));
    assert_eq!(volume_names(&sb).len(), 2);
    ok(&podbag(&sb, &["rm", "-v", "gone"]));
    assert_eq!(volume_names(&sb), names, "rm -v kept the volume");
}

#[test]
fn failed_start_leaves_no_anonymous_volume() {
    let sb = Sandbox::new();
    let out = run(&sb, &["-v", "/mnt"], &["nonexistent-command"]);
    assert_eq!(out.status.code(), Some(127), "{}", stderr(&out));
    assert!(volume_names(&sb).is_empty());
}

#[test]
fn volume_rm_removes_files_of_other_users() {
    let sb = Sandbox::new();
    // Not empty afterwards: an empty volume takes the image directory's owner and mode
    // again at the next mount (Docker's first-use copy).
    let open = "chmod 777 /mnt && touch /mnt/keep";
    ok(&run(&sb, &["-v", "owned:/mnt"], &["sh", "-c", open]));
    let script = "mkdir /mnt/d && touch /mnt/d/f";
    ok(&run(
        &sb,
        &["-u", "405", "-v", "owned:/mnt"],
        &["sh", "-c", script],
    ));
    ok(&podbag(&sb, &["volume", "rm", "owned"]));
    assert!(volume_names(&sb).is_empty());
    assert!(!sb.data().join("volumes/owned").exists());
}

#[test]
fn inspect_shows_mounts() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    let start = [
        "run",
        "-d",
        "--name",
        "m",
        "-v",
        "named:/mnt:ro",
        "--tmpfs",
        "/srv",
        "--rootfs",
        &rootfs,
        "sleep",
        "30",
    ];
    ok(&podbag(&sb, &start));
    let info: serde_json::Value =
        serde_json::from_str(&ok(&podbag(&sb, &["inspect", "m"]))).unwrap();
    let mounts = info["mounts"].as_array().expect("mounts");
    assert_eq!(mounts.len(), 2, "{info}");
    assert_eq!(mounts[0]["kind"], "volume");
    assert_eq!(mounts[0]["source"], "named");
    assert_eq!(mounts[0]["target"], "/mnt");
    assert_eq!(mounts[0]["read_only"], true);
    assert_eq!(mounts[1]["kind"], "tmpfs");
    assert_eq!(mounts[1]["target"], "/srv");
    ok(&podbag(&sb, &["rm", "-f", "m"]));
}

/// A copy of the fixture rootfs whose `/data` is a link to `to`.
fn rootfs_with_data_link(tmp: &Path, to: &str) -> std::path::PathBuf {
    let root = tmp.join("rootfs");
    assert!(
        std::process::Command::new("cp")
            .arg("-a")
            .arg(rootfs())
            .arg(&root)
            .status()
            .unwrap()
            .success()
    );
    std::os::unix::fs::symlink(to, root.join("data")).unwrap();
    root
}

// The image may point a volume path at the kernel's views or at its own root.
#[test]
fn links_to_proc_sys_or_root_are_refused() {
    let sb = Sandbox::new();
    for to in ["/proc", "/sys/kernel", "/", "/proc/../"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = rootfs_with_data_link(tmp.path(), to);
        let out = sb
            .podbag()
            .args(["run", "--rm", "--rootfs"])
            .arg(&root)
            .args(["-v", "vol:/data:nocopy", "ls", "/proc"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(125), "{to}: {}", stderr(&out));
        assert!(stderr(&out).contains("/data"), "{to}: {}", stderr(&out));
    }
}

// An anonymous volume can be named by another container (`-v <hex>:/x`); removing its
// first container must not take it away from the second.
#[test]
fn anonymous_volume_in_use_is_kept_by_rm_v() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    ok(&podbag(
        &sb,
        &[
            "run", "--name", "a", "-v", "/mnt", "--rootfs", &rootfs, "true",
        ],
    ));
    let names = volume_names(&sb);
    let shared = format!("{}:/mnt", names[0]);
    let b = [
        "run", "-d", "--name", "b", "-v", &shared, "--rootfs", &rootfs, "sleep", "30",
    ];
    ok(&podbag(&sb, &b));
    ok(&podbag(&sb, &["rm", "-v", "a"]));
    assert_eq!(volume_names(&sb), names, "rm -v removed a volume in use");
    let out = podbag(&sb, &["volume", "rm", &names[0]]);
    assert_eq!(out.status.code(), Some(125));
    ok(&podbag(&sb, &["rm", "-f", "b"]));
}

// A start that fails before the shim (here: a name in use) removes its new anonymous
// volume, and does not wait on its own volume lock.
#[test]
fn early_failure_with_an_anonymous_volume_returns_promptly() {
    let sb = Sandbox::new();
    let rootfs = rootfs();
    ok(&podbag(
        &sb,
        &[
            "run", "-d", "--name", "dup", "--rootfs", &rootfs, "sleep", "30",
        ],
    ));
    let mut child = sb
        .podbag()
        .args([
            "run", "--name", "dup", "-v", "/mnt", "--rootfs", &rootfs, "true",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("the failed run hung");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(125));
    assert!(volume_names(&sb).is_empty(), "{:?}", volume_names(&sb));
    ok(&podbag(&sb, &["rm", "-f", "dup"]));
}
