#![cfg(target_os = "linux")]
//! What the store keeps right about image files: their times, crontabs with one link,
//! and objects a crash damaged, found and repaired. Root mode checks composefs and
//! hard-link trees; rootless mode has hard-link trees only.

mod common;
use common::registry::{FakeRegistry, TarEntry, TestImage, alpine_layer, layer, sha256};
use common::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(out));
    stdout(out)
}

fn podbag(sb: &Sandbox, args: &[&str]) -> Output {
    sb.podbag().args(args).output().unwrap()
}

/// Alpine and then `layers`, pushed as `repo:v1`. Returns the name to pull.
fn push(reg: &FakeRegistry, repo: &str, layers: &[&[TarEntry]]) -> String {
    let mut all = vec![alpine_layer()];
    all.extend(layers.iter().map(|l| layer(l)));
    reg.push(
        repo,
        "v1",
        &TestImage {
            layers: all,
            config: serde_json::json!({"Env": [PATH], "Cmd": ["true"]}),
        },
    );
    format!("{}/{repo}:v1", reg.host)
}

fn timed(path: &str, data: &[u8], mtime: u64) -> TarEntry {
    TarEntry::Timed {
        path: path.into(),
        data: data.to_vec(),
        mtime,
    }
}

/// The store's mount methods: composefs (as root) or hard-link trees (rootless), and
/// hard-link trees as root too.
fn sandboxes() -> Vec<(&'static str, Sandbox)> {
    let mut out = vec![("default", Sandbox::new())];
    if root_mode() {
        out.push((
            "hardlink",
            Sandbox::with_env("PODBAG_IMAGE_MOUNT", "hardlink"),
        ));
    }
    out
}

fn run(sb: &Sandbox, image: &str, script: &str) -> String {
    ok(&podbag(
        sb,
        &[
            "run",
            "--rm",
            "--network",
            "none",
            image,
            "sh",
            "-c",
            script,
        ],
    ))
}

/// A command run as the owner of the store: root in root mode.
fn as_owner(program: &str) -> Command {
    if root_mode() {
        let mut c = Command::new("sudo");
        c.args(["-n", program]);
        c
    } else {
        Command::new(program)
    }
}

fn succeeds(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{cmd:?}: {}", stderr(&out));
}

/// The object of a root-owned 0644 file with these contents.
fn object_of(sb: &Sandbox, contents: &[u8]) -> PathBuf {
    let hex = sha256(contents)
        .strip_prefix("sha256:")
        .unwrap()
        .to_string();
    let key = format!("{hex}.0.0.644");
    sb.data().join("store/objects").join(&key[..2]).join(key)
}

fn read_as_owner(path: &Path) -> Vec<u8> {
    let out = as_owner("cat").arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "cat {}: {}",
        path.display(),
        stderr(&out)
    );
    out.stdout
}

fn write_as_owner(path: &Path, data: &[u8]) {
    use std::io::Write;
    let mut tee = as_owner("tee")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    tee.stdin.take().unwrap().write_all(data).unwrap();
    assert!(tee.wait().unwrap().success());
}

#[test]
fn files_keep_the_times_of_their_layer() {
    let reg = FakeRegistry::start(false);
    let image = push(
        &reg,
        "times",
        &[&[
            timed("app/a.py", b"same\n", 1_600_000_000),
            timed("app/b.py", b"same\n", 1_700_000_000),
            timed("app/c.py", b"same\n", 1_600_000_000),
        ]],
    );
    for (method, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &image]));
        let seen = run(&sb, &image, "cd /app && stat -c '%n %Y %i' a.py b.py c.py");
        let rows: Vec<Vec<&str>> = seen.lines().map(|l| l.split(' ').collect()).collect();
        let times: Vec<(&str, &str)> = rows.iter().map(|r| (r[0], r[1])).collect();
        assert_eq!(
            times,
            [
                ("a.py", "1600000000"),
                ("b.py", "1700000000"),
                ("c.py", "1600000000")
            ],
            "{method}"
        );
        // An inode has one time: b.py cannot share the inode of a.py and c.py.
        assert_eq!(rows[0][2], rows[2][2], "{method}");
        assert_ne!(rows[0][2], rows[1][2], "{method}");
    }
}

/// cron skips a crontab that has more than one link.
#[test]
fn crontabs_have_one_link() {
    let reg = FakeRegistry::start(false);
    let job = b"* * * * * root true\n";
    let image = push(
        &reg,
        "cron",
        &[&[
            TarEntry::file("etc/crontab", job),
            TarEntry::file("etc/cron.d/job", job),
            TarEntry::file("var/spool/cron/crontabs/root", job),
            TarEntry::file("usr/share/doc/job", job),
        ]],
    );
    for (method, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &image]));
        let links = run(
            &sb,
            &image,
            "stat -c %h /etc/crontab /etc/cron.d/job /var/spool/cron/crontabs/root",
        );
        assert_eq!(links, "1\n1\n1", "{method}");
    }
}

/// A crash can leave an object shorter than its file. The next pull of any image with
/// that file writes it again, in place: images that share it heal too.
#[test]
fn a_damaged_object_is_repaired_by_the_next_pull() {
    let reg = FakeRegistry::start(false);
    let contents = b"#!/bin/sh\necho repaired\n";
    let a = push(&reg, "a", &[&[timed("bin/hello", contents, 1)]]);
    let b = push(&reg, "b", &[&[timed("bin/hello2", contents, 1)]]);
    for (method, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &a]));
        ok(&podbag(&sb, &["pull", &b]));
        succeeds(
            as_owner("truncate")
                .args(["-s", "5"])
                .arg(object_of(&sb, contents)),
        );
        let want = "#!/bin/sh\necho repaired";
        let broken = podbag(
            &sb,
            &["run", "--rm", "--network", "none", &a, "cat", "/bin/hello"],
        );
        assert_ne!(stdout(&broken), want, "{method}: the damage did not show");
        ok(&podbag(&sb, &["rmi", &b]));
        ok(&podbag(&sb, &["pull", &b]));
        assert_eq!(run(&sb, &a, "cat /bin/hello"), want, "{method}");
        assert_eq!(run(&sb, &b, "cat /bin/hello2"), want, "{method}");
    }
}

/// Damage that keeps the size passes the check at pull time; `system verify` reads every
/// object, and `--fix` removes the images that use a damaged one.
#[test]
fn verify_finds_damage_and_fix_removes_the_images_that_use_it() {
    let reg = FakeRegistry::start(false);
    let contents = b"shared contents\n";
    let a = push(&reg, "a", &[&[timed("x", contents, 1)]]);
    let b = push(&reg, "b", &[&[timed("y", contents, 1)]]);
    let c = push(&reg, "c", &[&[timed("z", b"other\n", 1)]]);
    for (method, sb) in sandboxes() {
        for image in [&a, &b, &c] {
            ok(&podbag(&sb, &["pull", image]));
        }
        let clean = podbag(&sb, &["system", "verify"]);
        assert_eq!(clean.status.code(), Some(0), "{method}: {}", stderr(&clean));
        succeeds(
            as_owner("dd")
                .args([
                    "if=/dev/zero",
                    "bs=1",
                    "count=3",
                    "conv=notrunc",
                    "status=none",
                ])
                .arg(format!("of={}", object_of(&sb, contents).display())),
        );
        let found = podbag(&sb, &["system", "verify"]);
        assert_eq!(found.status.code(), Some(1), "{method}: {}", stderr(&found));
        let text = stdout(&found);
        assert!(text.contains(": 1 damaged"), "{method}: {text}");
        assert!(text.contains(&a) && text.contains(&b), "{method}: {text}");
        assert!(!text.contains(&c), "{method}: {text}");

        let fixed = ok(&podbag(&sb, &["system", "verify", "--fix"]));
        assert!(fixed.contains("Removed:"), "{method}: {fixed}");
        let images = ok(&podbag(&sb, &["images"]));
        assert!(images.contains(&c), "{method}: {images}");
        assert!(
            !images.contains(&a) && !images.contains(&b),
            "{method}: {images}"
        );
        let after = ok(&podbag(&sb, &["system", "verify"]));
        assert!(after.contains(": 0 damaged"), "{method}: {after}");
    }
}

/// An older podbag stored layers without file times, and objects with the time of the
/// pull. The next pull that needs such a layer reads it again.
#[test]
fn a_layer_stored_by_an_older_podbag_gets_its_times() {
    let reg = FakeRegistry::start(false);
    let old_layer = [timed("app/x", b"x\n", 1_600_000_000)];
    let old = push(&reg, "old", &[&old_layer]);
    let new = push(
        &reg,
        "new",
        &[&old_layer, &[timed("app/y", b"y\n", 1_700_000_000)]],
    );
    let diff_id = sha256(&layer(&old_layer));
    for (method, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &old]));
        let index = sb
            .data()
            .join("store/layers")
            .join(format!("{}.json", diff_id.strip_prefix("sha256:").unwrap()));
        let mut json: serde_json::Value = serde_json::from_slice(&read_as_owner(&index)).unwrap();
        for e in json["entries"].as_array_mut().unwrap() {
            e.as_object_mut().unwrap().remove("mtime");
        }
        write_as_owner(&index, &serde_json::to_vec(&json).unwrap());
        succeeds(as_owner("touch").arg(object_of(&sb, b"x\n")));

        ok(&podbag(&sb, &["pull", &new]));
        let seen = run(&sb, &new, "stat -c %Y /app/x /app/y");
        assert_eq!(seen, "1600000000\n1700000000", "{method}");
    }
}

/// A root-owned file with this mode (setuid bits too).
fn with_mode(path: &str, data: &[u8], mode: u32) -> TarEntry {
    TarEntry::File {
        path: path.into(),
        data: data.to_vec(),
        mode,
        uid: 0,
        gid: 0,
    }
}

fn hex_sha256(data: &[u8]) -> String {
    sha256(data).strip_prefix("sha256:").unwrap().to_string()
}

/// True when the store owner sees `path`.
fn exists_as_owner(path: &Path) -> bool {
    as_owner("test")
        .arg("-e")
        .arg(path)
        .status()
        .unwrap()
        .success()
}

/// The compressed copies in the store's objects folder.
fn cold_objects(sb: &Sandbox) -> String {
    let out = as_owner("find")
        .arg(sb.data().join("store/objects"))
        .args(["-name", "*.zst"])
        .output()
        .unwrap();
    assert!(out.status.success(), "find: {}", stderr(&out));
    stdout(&out)
}

/// An image whose files compress well: a big one with an old time, and a setuid one.
fn push_cold(reg: &FakeRegistry, big: &[u8]) -> String {
    push(
        reg,
        "cold",
        &[&[
            timed("srv/big", big, 1_600_000_000),
            with_mode("bin/tool", b"#!/bin/sh\n", 0o4755),
        ]],
    )
}

#[test]
fn compressed_images_run_with_the_same_files() {
    let reg = FakeRegistry::start(false);
    let big = b"podbag ".repeat(10_000);
    let image = push_cold(&reg, &big);
    for (mode, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &image]));
        let out = ok(&podbag(&sb, &["system", "compress", &image]));
        assert!(out.contains("Compressed 1 images"), "{mode}: {out}");
        let zst = PathBuf::from(format!("{}.zst", object_of(&sb, &big).display()));
        assert!(exists_as_owner(&zst), "{mode}: no {}", zst.display());
        let df = ok(&podbag(&sb, &["system", "df"]));
        assert!(df.contains("Compressed"), "{mode}: {df}");
        let seen = run(
            &sb,
            &image,
            "sha256sum /srv/big; stat -c '%Y %a' /srv/big /bin/tool",
        );
        assert!(seen.contains("1600000000"), "{mode}: {seen}");
        assert!(seen.contains("4755"), "{mode}: {seen}");
        assert!(seen.contains(&hex_sha256(&big)), "{mode}: {seen}");
    }
}

#[test]
fn save_of_a_compressed_image_is_unchanged() {
    let reg = FakeRegistry::start(false);
    let big = b"podbag ".repeat(10_000);
    let image = push_cold(&reg, &big);
    for (mode, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &image]));
        let a = sb.tmp.path().join("a.tar");
        let b = sb.tmp.path().join("b.tar");
        ok(&podbag(&sb, &["save", "-o", a.to_str().unwrap(), &image]));
        ok(&podbag(&sb, &["system", "compress", &image]));
        assert!(!cold_objects(&sb).is_empty(), "{mode}: nothing compressed");
        ok(&podbag(&sb, &["save", "-o", b.to_str().unwrap(), &image]));
        let (a, b) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert!(a == b, "{mode}: the archives differ");
    }
}

#[test]
fn decompress_makes_the_image_plain_again() {
    let reg = FakeRegistry::start(false);
    let big = b"podbag ".repeat(10_000);
    let image = push_cold(&reg, &big);
    for (mode, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &image]));
        ok(&podbag(&sb, &["system", "compress", &image]));
        let object = object_of(&sb, &big);
        assert!(!exists_as_owner(&object), "{mode}: still plain");
        let out = ok(&podbag(&sb, &["system", "decompress", &image]));
        assert!(out.contains("Decompressed 1 images"), "{mode}: {out}");
        assert!(exists_as_owner(&object), "{mode}: not plain");
        assert_eq!(cold_objects(&sb), "", "{mode}");
    }
}

/// An image a container uses stays as it is, and the user is told why.
#[test]
fn compress_tells_which_images_a_container_uses() {
    let reg = FakeRegistry::start(false);
    let image = push_cold(&reg, &b"podbag ".repeat(10_000));
    for (mode, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &image]));
        ok(&podbag(&sb, &["create", "--network", "none", &image]));
        let out = podbag(&sb, &["system", "compress", &image]);
        assert!(
            stdout(&out).contains("Compressed 0 images"),
            "{mode}: {}",
            stdout(&out)
        );
        let want = format!("podbag: {image} is not compressed: a container uses it");
        assert!(stderr(&out).contains(&want), "{mode}: {}", stderr(&out));
        assert_eq!(cold_objects(&sb), "", "{mode}");
    }
}

/// A build context in the sandbox with this Dockerfile and a `hello.txt`, readable by all:
/// rootless, the store helper reads it as another user.
fn context(sb: &Sandbox, name: &str, dockerfile: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = sb.tmp.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Dockerfile"), dockerfile).unwrap();
    std::fs::write(dir.join("hello.txt"), "hello\n").unwrap();
    for (path, mode) in [
        (sb.tmp.path().to_path_buf(), 0o755),
        (dir.clone(), 0o755),
        (dir.join("Dockerfile"), 0o644),
        (dir.join("hello.txt"), 0o644),
    ] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    dir
}

/// Builds read files of a compressed base image (`COPY --chown` reads its /etc/passwd) and
/// of a compressed `COPY --from` image.
#[test]
fn builds_read_files_of_a_compressed_image() {
    let reg = FakeRegistry::start(false);
    // Big enough to be compressed: objects under 4096 bytes stay plain.
    let mut passwd = String::from("root:x:0:0:root:/root:/bin/sh\n");
    let mut group = String::from("root:x:0:root\n");
    for i in 0..300 {
        passwd.push_str(&format!("user{i}:x:{}:{}::/:/bin/sh\n", 2000 + i, 2000 + i));
        group.push_str(&format!("group{i}:x:{}:\n", 2000 + i));
    }
    passwd.push_str("app:x:1234:1234::/:/bin/sh\n");
    group.push_str("app:x:1234:\n");
    let big = b"podbag ".repeat(10_000);
    let image = push(
        &reg,
        "coldbase",
        &[&[
            with_mode("etc/passwd", passwd.as_bytes(), 0o644),
            with_mode("etc/group", group.as_bytes(), 0o644),
            with_mode("srv/big", &big, 0o644),
        ]],
    );
    let compress = |mode: &str, sb: &Sandbox| {
        ok(&podbag(sb, &["system", "compress", &image]));
        for data in [passwd.as_bytes(), &big] {
            let zst = PathBuf::from(format!("{}.zst", object_of(sb, data).display()));
            assert!(exists_as_owner(&zst), "{mode}: no {}", zst.display());
        }
    };
    for (mode, sb) in sandboxes() {
        ok(&podbag(&sb, &["pull", &image]));
        compress(mode, &sb);
        let chown = context(
            &sb,
            "chown",
            &format!("FROM {image}\nCOPY --chown=app:app hello.txt /hello.txt\n"),
        );
        let dir = chown.to_str().unwrap();
        ok(&podbag(
            &sb,
            &["build", "--network", "none", "-t", "chown", dir],
        ));
        let seen = run(&sb, "chown", "stat -c '%u %g' /hello.txt; cat /hello.txt");
        assert_eq!(seen, "1234 1234\nhello", "{mode}");
        // The built image keeps the shared objects plain: without it they compress again.
        ok(&podbag(&sb, &["rmi", "chown"]));
        compress(mode, &sb);
        let from = context(
            &sb,
            "from",
            &format!("FROM {image}\nCOPY --from={image} /srv/big /big\n"),
        );
        let dir = from.to_str().unwrap();
        ok(&podbag(
            &sb,
            &["build", "--network", "none", "-t", "from", dir],
        ));
        let seen = run(&sb, "from", "sha256sum /big");
        assert!(seen.contains(&hex_sha256(&big)), "{mode}: {seen}");
    }
}

/// A config.toml that does not parse does not stop `rmi` or `system prune`: they go on
/// with compression off.
#[test]
fn a_malformed_config_does_not_stop_image_removal() {
    use std::os::unix::fs::PermissionsExt;
    let reg = FakeRegistry::start(false);
    let a = push(&reg, "a", &[&[timed("x", b"a\n", 1)]]);
    let b = push(&reg, "b", &[&[timed("y", b"b\n", 1)]]);
    let mut sb = Sandbox::new();
    // Readable by all: rootless, the store helper reads it as another user.
    let config = sb.tmp.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    for dir in [sb.tmp.path(), config.as_path()] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    sb.env.push((
        "PODBAG_CONFIG_DIR".to_string(),
        config.display().to_string(),
    ));
    ok(&podbag(&sb, &["pull", &a]));
    ok(&podbag(&sb, &["pull", &b]));
    let file = config.join("config.toml");
    std::fs::write(&file, "image_compression = \"zstd\"\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    ok(&podbag(&sb, &["rmi", &a]));
    ok(&podbag(&sb, &["system", "prune", "-f"]));
    let images = ok(&podbag(&sb, &["images"]));
    assert!(!images.contains(&a) && !images.contains(&b), "{images}");
}
