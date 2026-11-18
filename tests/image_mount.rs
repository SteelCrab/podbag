#![cfg(target_os = "linux")]
//! How containers get their image files (design 6.3.3): composefs as root, hard-link trees
//! rootless or with `image_mount = "hardlink"`. Both must show the same tree.

mod common;
use common::registry::{FakeRegistry, TarEntry, TestImage, alpine_layer, layer};
use common::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

fn openpod(sb: &Sandbox, args: &[&str]) -> Output {
    sb.openpod().args(args).output().unwrap()
}

/// `test -e`, as root when openpod runs as root (the store is private to its owner).
fn exists(path: &Path) -> bool {
    let mut cmd = if root_mode() {
        let mut c = Command::new("sudo");
        c.args(["-n", "test", "-e"]);
        c
    } else {
        let mut c = Command::new("test");
        c.arg("-e");
        c
    };
    cmd.arg(path).status().unwrap().success()
}

fn list(dir: &Path) -> Vec<String> {
    let mut cmd = if root_mode() {
        let mut c = Command::new("sudo");
        c.args(["-n", "ls", "-A"]);
        c
    } else {
        let mut c = Command::new("ls");
        c.arg("-A");
        c
    };
    let out = cmd.arg(dir).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(String::from)
        .collect()
}

/// The one image directory in the sandbox's store.
fn image_dir(sb: &Sandbox) -> PathBuf {
    let images = sb.data().join("store/images");
    let names = list(&images);
    assert_eq!(names.len(), 1, "{names:?}");
    images.join(&names[0])
}

fn erofs_mounts(sb: &Sandbox) -> Vec<String> {
    mounts_with_types(&sb.runtime().join("images"))
        .into_iter()
        .filter(|(_, t)| t == "erofs")
        .map(|(p, _)| p)
        .collect()
}

fn push(reg: &FakeRegistry, repo: &str, extra: Vec<Vec<TarEntry>>) -> String {
    let mut layers = vec![alpine_layer()];
    layers.extend(extra.iter().map(|e| layer(e)));
    reg.push(
        repo,
        "v1",
        &TestImage {
            layers,
            config: serde_json::json!({"Env": [PATH], "Cmd": ["true"]}),
        },
    );
    format!("{}/{repo}:v1", reg.host)
}

fn file(path: &str, data: &[u8], mode: u32, uid: u64) -> TarEntry {
    TarEntry::File {
        path: path.into(),
        data: data.to_vec(),
        mode,
        uid,
        gid: uid,
    }
}

/// Capability set v2 with CAP_NET_RAW permitted and effective.
fn net_raw_capability() -> Vec<u8> {
    vec![
        1, 0, 0, 2, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]
}

/// An image with every kind of entry the two methods must agree on.
fn rich_image(reg: &FakeRegistry) -> String {
    let mut extra = vec![
        file("opt/suid", b"#!/bin/sh\necho hi\n", 0o4755, 0),
        file("opt/h1", b"linked\n", 0o644, 0),
        TarEntry::Hardlink {
            path: "opt/h2".into(),
            target: "opt/h1".into(),
        },
        file("opt/same1", b"same\n", 0o644, 0),
        file("opt/same2", b"same\n", 0o644, 0),
        file("opt/e1", b"", 0o644, 0),
        file("opt/e2", b"", 0o644, 0),
        TarEntry::Fifo {
            path: "opt/fifo".into(),
            mode: 0o600,
        },
        TarEntry::Device {
            path: "opt/null".into(),
            major: 1,
            minor: 3,
            mode: 0o666,
        },
        file(
            &format!("opt/{}", "n".repeat(200)),
            b"long name\n",
            0o644,
            0,
        ),
        TarEntry::Symlink {
            path: "opt/longlink".into(),
            target: "a/".repeat(1000),
        },
        TarEntry::Dir {
            path: "home/u".into(),
            mode: 0o700,
            uid: 1000,
            gid: 1000,
        },
        file("home/u/f", b"mine\n", 0o600, 1000),
        file("opt/gone/x", b"x\n", 0o644, 0),
        file("opt/opq/old", b"old\n", 0o644, 0),
        TarEntry::WithXattrs {
            path: "opt/withcap".into(),
            data: b"cap\n".to_vec(),
            mode: 0o755,
            xattrs: vec![
                ("security.capability".into(), net_raw_capability()),
                ("user.note".into(), b"hello".to_vec()),
            ],
        },
    ];
    for i in 0..1500 {
        extra.push(file(
            &format!("opt/many/f{i}"),
            format!("{i}\n").as_bytes(),
            0o644,
            0,
        ));
    }
    let third = vec![
        TarEntry::Whiteout("opt/gone".into()),
        TarEntry::Opaque("opt/opq".into()),
        file("opt/opq/new", b"new\n", 0o644, 0),
    ];
    push(reg, "rich", vec![extra, third])
}

/// What a container sees of its root: types, modes, owners, device numbers, sizes,
/// symlink targets, contents, and which paths share an inode (by the group's first path).
fn tree_listing(sb: &Sandbox, image: &str) -> String {
    let script = "cd / && { \
        find . -xdev ! -type d -exec stat -c '%n|%f|%u|%g|%t:%T|%s|%i|%N' {} + ; \
        find . -xdev -type d -exec stat -c '%n|%f|%u|%g' {} + ; \
        find . -xdev -type f -exec md5sum {} + ; }";
    let out = ok(&openpod(
        sb,
        &[
            "run",
            "--rm",
            "--network",
            "none",
            "--hostname",
            "box",
            image,
            "sh",
            "-c",
            script,
        ],
    ));
    let skip = ["./etc/hosts", "./etc/hostname", "./etc/resolv.conf"];
    let mut lines: Vec<String> = out
        .lines()
        .filter(|l| {
            !skip
                .iter()
                .any(|s| l.starts_with(&format!("{s}|")) || l.ends_with(&format!(" {s}")))
        })
        .map(String::from)
        .collect();
    lines.sort();
    // Inode numbers differ between file systems: name each by its first path.
    let mut first: BTreeMap<String, String> = BTreeMap::new();
    for l in &lines {
        let f: Vec<&str> = l.split('|').collect();
        if f.len() == 8 {
            first
                .entry(f[6].to_string())
                .or_insert_with(|| f[0].to_string());
        }
    }
    lines
        .iter()
        .map(|l| {
            let mut f: Vec<String> = l.split('|').map(String::from).collect();
            if f.len() == 8 {
                f[6] = first[&f[6]].clone();
            }
            f.join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The sandbox's one image is stored for composefs.
fn assert_composefs(sb: &Sandbox) {
    assert!(
        exists(&image_dir(sb).join("rootfs.erofs")),
        "the image is not stored for composefs"
    );
}

fn hardlink_sandbox() -> Sandbox {
    Sandbox::with_env("OPENPOD_IMAGE_MOUNT", "hardlink")
}

#[test]
fn root_mode_mounts_images_with_composefs() {
    if !root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = push(
        &reg,
        "c",
        vec![vec![file("hello", b"composefs\n", 0o644, 0)]],
    );
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &image]));
    let dir = image_dir(&sb);
    assert!(exists(&dir.join("rootfs.erofs")));
    assert!(!exists(&dir.join("rootfs")));
    let out = ok(&openpod(
        &sb,
        &[
            "run",
            "--rm",
            &image,
            "sh",
            "-c",
            "cat /hello /proc/self/mountinfo",
        ],
    ));
    assert!(out.starts_with("composefs\n"), "{out}");
    let root = out
        .lines()
        .find(|l| l.split(' ').nth(4) == Some("/"))
        .unwrap_or_else(|| panic!("no root mount: {out}"));
    assert!(
        root.contains(" - overlay ") && root.contains("metacopy=on"),
        "{root}"
    );
    assert_eq!(erofs_mounts(&sb).len(), 1);
}

#[test]
fn rootless_mode_keeps_hardlink_trees() {
    if root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "r", vec![vec![file("hello", b"tree\n", 0o644, 0)]]);
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &image]));
    let dir = image_dir(&sb);
    assert!(exists(&dir.join("rootfs/hello")));
    assert!(!exists(&dir.join("rootfs.erofs")));
    assert!(!exists(&sb.data().join("store/mount-method.json")));
    assert_eq!(
        ok(&openpod(&sb, &["run", "--rm", &image, "cat", "/hello"])),
        "tree"
    );
}

#[test]
fn both_mount_methods_show_the_same_tree() {
    if !root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = rich_image(&reg);
    let composefs = Sandbox::new();
    let hardlink = hardlink_sandbox();
    let a = tree_listing(&composefs, &image);
    let b = tree_listing(&hardlink, &image);
    assert!(exists(&image_dir(&composefs).join("rootfs.erofs")));
    assert!(exists(&image_dir(&hardlink).join("rootfs")));
    assert!(
        a.contains("./opt/h1|") && a.contains("./opt/many/f1499|"),
        "{a}"
    );
    assert!(
        !a.contains("./opt/gone") && !a.contains("./opt/opq/old"),
        "{a}"
    );
    if a != b {
        let only = |x: &str, y: &str| -> Vec<String> {
            let ys: std::collections::BTreeSet<&str> = y.lines().collect();
            x.lines()
                .filter(|l| !ys.contains(l))
                .map(String::from)
                .collect()
        };
        panic!(
            "composefs only: {:#?}\nhard links only: {:#?}",
            only(&a, &b),
            only(&b, &a)
        );
    }
}

#[test]
fn file_capabilities_work_with_both_methods() {
    if !root_mode() {
        return;
    }
    let busybox = std::fs::read(Path::new(&rootfs()).join("bin/busybox")).unwrap();
    let reg = FakeRegistry::start(false);
    let image = push(
        &reg,
        "cap",
        vec![vec![TarEntry::WithXattrs {
            // busybox picks its applet from the program's name.
            path: "capdir/cat".into(),
            data: busybox,
            mode: 0o755,
            xattrs: vec![("security.capability".into(), net_raw_capability())],
        }]],
    );
    for sb in [Sandbox::new(), hardlink_sandbox()] {
        let out = ok(&openpod(
            &sb,
            &[
                "run",
                "--rm",
                "--network",
                "none",
                "--user",
                "1000",
                &image,
                "/capdir/cat",
                "/proc/self/status",
            ],
        ));
        let eff = out.lines().find(|l| l.starts_with("CapEff:")).unwrap();
        assert_eq!(eff, "CapEff:\t0000000000002000", "{out}");
        if !sb.env.iter().any(|(k, _)| k == "OPENPOD_IMAGE_MOUNT") {
            assert_composefs(&sb);
            assert_eq!(erofs_mounts(&sb).len(), 1);
        }
    }
}

#[test]
fn writes_copy_contents_up_from_the_object_store() {
    if !root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = push(
        &reg,
        "w",
        vec![vec![
            file("a", b"first\n", 0o644, 0),
            file("b", b"0123456789", 0o644, 0),
            file("c", b"third\n", 0o755, 0),
        ]],
    );
    let sb = Sandbox::new();
    let out = ok(&openpod(
        &sb,
        &[
            "run",
            "--rm",
            &image,
            "sh",
            "-c",
            "echo more >> /a; truncate -s 3 /b; chmod 700 /c; cat /c; echo x >> /c; \
             cat /a /b; echo; cat /c",
        ],
    ));
    assert_eq!(out, "third\nfirst\nmore\n012\nthird\nx");
    assert_composefs(&sb);
}

#[test]
fn a_missing_artifact_is_built_on_first_use() {
    if !root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "m", vec![vec![file("hello", b"hi\n", 0o644, 0)]]);
    // Pulled for hard links, run with composefs.
    let mut sb = hardlink_sandbox();
    ok(&openpod(&sb, &["pull", &image]));
    assert!(!exists(&image_dir(&sb).join("rootfs.erofs")));
    sb.env.retain(|(k, _)| k != "OPENPOD_IMAGE_MOUNT");
    assert_eq!(
        ok(&openpod(&sb, &["run", "--rm", &image, "cat", "/hello"])),
        "hi"
    );
    assert!(exists(&image_dir(&sb).join("rootfs.erofs")));
    // Pulled for composefs, run with hard links.
    let mut sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &image]));
    assert!(!exists(&image_dir(&sb).join("rootfs")));
    sb.env
        .push(("OPENPOD_IMAGE_MOUNT".to_string(), "hardlink".to_string()));
    assert_eq!(
        ok(&openpod(&sb, &["run", "--rm", &image, "cat", "/hello"])),
        "hi"
    );
    assert!(exists(&image_dir(&sb).join("rootfs/hello")));
}

#[test]
fn concurrent_starts_share_one_mount() {
    if !root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "p", vec![vec![file("hello", b"hi\n", 0o644, 0)]]);
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &image]));
    let children: Vec<_> = (0..6)
        .map(|_| {
            sb.openpod()
                .args(["run", "-d", "--network", "none", &image, "sleep", "60"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        ok(&child.wait_with_output().unwrap());
    }
    assert_eq!(erofs_mounts(&sb).len(), 1, "{:?}", erofs_mounts(&sb));
}

#[test]
fn a_lost_image_mount_is_mounted_again() {
    if !root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "l", vec![vec![file("hello", b"hi\n", 0o644, 0)]]);
    let sb = Sandbox::new();
    assert_eq!(
        ok(&openpod(&sb, &["run", "--rm", &image, "cat", "/hello"])),
        "hi"
    );
    let point = erofs_mounts(&sb).pop().unwrap();
    let sudo = |args: &[&str]| {
        assert!(
            Command::new("sudo")
                .arg("-n")
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    // Unmounted, mount point left behind.
    sudo(&["umount", &point]);
    assert_eq!(
        ok(&openpod(&sb, &["run", "--rm", &image, "cat", "/hello"])),
        "hi"
    );
    assert_eq!(erofs_mounts(&sb), vec![point.clone()]);
    // Everything gone, as after a reboot.
    sudo(&["umount", &point]);
    sudo(&["rm", "-rf", &sb.runtime().join("images").to_string_lossy()]);
    assert_eq!(
        ok(&openpod(&sb, &["run", "--rm", &image, "cat", "/hello"])),
        "hi"
    );
    assert_eq!(erofs_mounts(&sb), vec![point]);
}

#[test]
fn removing_an_image_unmounts_it() {
    if !root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "x", vec![vec![file("hello", b"hi\n", 0o644, 0)]]);
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["run", "--rm", &image, "true"]));
    assert_eq!(erofs_mounts(&sb).len(), 1);
    let dir = image_dir(&sb);
    ok(&openpod(&sb, &["rmi", &image]));
    assert_eq!(erofs_mounts(&sb), Vec::<String>::new());
    assert!(!exists(&dir));
    assert_eq!(list(&sb.runtime().join("images")), Vec::<String>::new());
}

#[test]
fn check_image_leaves_no_mounts() {
    if !root_mode() {
        return;
    }
    let reg = FakeRegistry::start(false);
    let image = push(&reg, "k", vec![vec![file("hello", b"hi\n", 0o644, 0)]]);
    let sb = Sandbox::new();
    ok(&openpod(&sb, &["pull", &image]));
    assert_composefs(&sb);
    let before = mounts_under(sb.tmp.path());
    ok(&openpod(&sb, &["check-image", &image]));
    assert_eq!(mounts_under(sb.tmp.path()), before);
    assert_eq!(list(&sb.data().join("store/tmp")), Vec::<String>::new());
}
