#![cfg(target_os = "linux")]
//! `podbag build`, `commit`, `export` and `diff`, with a base image from a fake registry.
//! Runs rootless (`dev/vm.sh test`) and as root (`dev/vm.sh test-root`). RUN steps use
//! `--network none`, so no test here needs the network turn.

mod common;
use common::registry::{Auth, FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::path::{Path, PathBuf};
use std::process::Output;

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

struct Setup {
    sb: Sandbox,
    _reg: FakeRegistry,
    base: String,
}

impl Drop for Setup {
    fn drop(&mut self) {
        // The images go through podbag: rootless, their files belong to other user IDs,
        // which the sandbox's own removal cannot delete.
        let _ = self.sb.podbag().args(["system", "prune", "-f"]).output();
    }
}

impl Setup {
    fn new() -> Setup {
        Setup::on(FakeRegistry::start(false), Sandbox::new())
    }

    /// The base image in this registry, for podbag in this sandbox.
    fn on(reg: FakeRegistry, sb: Sandbox) -> Setup {
        reg.push(
            "test/alpine",
            "v1",
            &TestImage {
                layers: vec![alpine_layer()],
                config: serde_json::json!({"Env": [PATH], "Cmd": ["/bin/sh"]}),
            },
        );
        let base = format!("{}/test/alpine:v1", reg.host);
        Setup {
            sb,
            _reg: reg,
            base,
        }
    }

    /// A context directory with these files (path, content).
    fn context(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.sb.tmp.path().join(name);
        for (path, content) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content.replace("@BASE@", &self.base)).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        let top = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(self.sb.tmp.path(), top).unwrap();
        open_up(&dir);
        dir
    }

    fn podbag(&self, args: &[&str]) -> Output {
        self.sb.podbag().args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.podbag(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "podbag {args:?}\nstdout: {}\nstderr: {}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    fn build(&self, dir: &Path, extra: &[&str]) -> String {
        let mut args = vec!["build", "--network", "none"];
        args.extend_from_slice(extra);
        let d = dir.display().to_string();
        args.push(&d);
        self.ok(&args)
    }

    /// `podbag run --rm --network none IMAGE sh -c SCRIPT`.
    fn run(&self, image: &str, script: &str) -> String {
        self.ok(&[
            "run",
            "--rm",
            "--network",
            "none",
            image,
            "sh",
            "-c",
            script,
        ])
    }
}

/// Readable by root's podbag in root mode, and by the test user.
fn open_up(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in walk(path) {
        let mode = if entry.is_dir() { 0o755 } else { 0o644 };
        let _ = std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(mode));
    }
}

fn walk(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if path.is_dir() {
        for e in std::fs::read_dir(path).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                out.extend(walk(&e.path()));
            } else {
                out.push(e.path());
            }
        }
    }
    out
}

fn image_id(build_output: &str) -> String {
    build_output
        .lines()
        .find_map(|l| l.strip_prefix("Successfully built "))
        .unwrap_or_else(|| panic!("no image ID in:\n{build_output}"))
        .trim()
        .to_string()
}

#[test]
fn run_copy_env_and_cmd_make_an_image() {
    let s = Setup::new();
    let dir = s.context(
        "basic",
        &[
            (
                "Dockerfile",
                "FROM @BASE@\nENV GREETING=hello\nRUN echo \"built by $GREETING\" > /built \
                 && rm /etc/motd\nCOPY note.txt /opt/app/\nCMD [\"cat\", \"/built\", \"/opt/app/note.txt\"]\n",
            ),
            ("note.txt", "from the context\n"),
        ],
    );
    let out = s.build(&dir, &["-t", "app:1"]);
    assert!(out.contains("Step 1/5 : FROM"), "{out}");
    assert!(out.contains("Step 3/5 : RUN echo"), "{out}");
    assert!(out.contains("Successfully tagged app:1"), "{out}");
    let id = image_id(&out);
    assert_eq!(
        s.ok(&["run", "--rm", "--network", "none", "app:1"]),
        "built by hello\nfrom the context"
    );
    // The deleted file stays deleted in the image.
    assert_eq!(s.run("app:1", "test -e /etc/motd || echo gone"), "gone");
    assert_eq!(s.run("app:1", "echo $GREETING"), "hello");
    assert!(s.ok(&["images"]).contains(&id[..12]), "listed by its ID");

    // A second build: every step from the cache, the same image.
    let again = s.build(&dir, &["-t", "app:1"]);
    assert_eq!(
        again.matches("Using cache").count(),
        2,
        "RUN and COPY: {again}"
    );
    assert_eq!(image_id(&again), id);
    // A changed file: the steps from COPY on run again.
    std::fs::write(dir.join("note.txt"), "changed\n").unwrap();
    let changed = s.build(&dir, &["-t", "app:2"]);
    assert_eq!(changed.matches("Using cache").count(), 1, "{changed}");
    assert_eq!(s.run("app:2", "cat /opt/app/note.txt"), "changed");
    // --no-cache runs everything.
    let fresh = s.build(&dir, &["--no-cache", "-q"]);
    assert!(!fresh.contains("Using cache"), "{fresh}");
    assert!(
        fresh.trim().starts_with("sha256:"),
        "-q prints only the ID: {fresh}"
    );
}

#[test]
fn multi_stage_builds_only_what_the_target_needs() {
    let s = Setup::new();
    let dir = s.context(
        "multi",
        &[(
            "Dockerfile",
            "ARG BASE_IMAGE=@BASE@\nFROM $BASE_IMAGE AS build\nARG VERSION=1\n\
             RUN mkdir /out && echo \"v$VERSION\" > /out/version\n\
             FROM @BASE@ AS unused\nRUN exit 1\n\
             FROM scratch AS final\nCOPY --from=build /out/version /version\n\
             COPY --from=0 /bin/busybox /bin/busybox\nCOPY --from=0 /lib /lib\nCMD [\"/bin/busybox\", \"cat\", \"/version\"]\n",
        )],
    );
    let out = s.build(&dir, &["-t", "small", "--build-arg", "VERSION=7"]);
    assert!(!out.contains("exit 1"), "the unused stage ran:\n{out}");
    assert_eq!(s.ok(&["run", "--rm", "--network", "none", "small"]), "v7");
    // --target stops at a stage.
    s.build(&dir, &["-t", "builder", "--target", "build"]);
    assert_eq!(s.run("builder", "cat /out/version"), "v1");
    let unused = s.build(&dir, &["--build-arg", "NOPE=1", "--target", "build"]);
    assert!(unused.contains("[NOPE] were not consumed"), "{unused}");
}

#[test]
fn add_unpacks_archives_and_dockerignore_leaves_files_out() {
    let s = Setup::new();
    let dir = s.context(
        "add",
        &[
            (
                "Dockerfile",
                "FROM @BASE@\nADD files.tar.gz /opt/\nCOPY . /ctx/\nRUN ls /ctx | sort | tr '\\n' ' '\n",
            ),
            (".dockerignore", "*.log\nsecret/\n"),
            ("debug.log", "no"),
            ("secret/key", "no"),
            ("keep.txt", "yes"),
        ],
    );
    // files.tar.gz: one file, packed by tar with gzip.
    let src = s.sb.tmp.path().join("src");
    std::fs::create_dir_all(src.join("pkg")).unwrap();
    std::fs::write(src.join("pkg/data"), "unpacked\n").unwrap();
    let status = std::process::Command::new("tar")
        .args(["-czf"])
        .arg(dir.join("files.tar.gz"))
        .args(["-C"])
        .arg(&src)
        .arg("pkg")
        .status()
        .unwrap();
    assert!(status.success());
    open_up(&dir);
    s.build(&dir, &["-t", "added"]);
    assert_eq!(s.run("added", "cat /opt/pkg/data"), "unpacked");
    assert_eq!(
        s.run("added", "ls -A /ctx | sort | tr '\\n' ' '"),
        ".dockerignore Dockerfile files.tar.gz keep.txt"
    );
}

#[test]
fn user_workdir_and_chown() {
    let s = Setup::new();
    let dir = s.context(
        "user",
        &[
            (
                "Dockerfile",
                "FROM @BASE@\nRUN adduser -D -u 1000 app\nWORKDIR /srv/app\n\
                 COPY --chown=app:app --chmod=640 conf ./\nUSER app\n\
                 RUN id -u > /tmp/uid && pwd > /tmp/pwd && touch mine 2>/dev/null || echo cannot-write > /tmp/w\n",
            ),
            ("conf", "x=1\n"),
        ],
    );
    s.build(&dir, &["-t", "users"]);
    assert_eq!(s.run("users", "cat /tmp/uid /tmp/pwd"), "1000\n/srv/app");
    assert_eq!(s.run("users", "stat -c '%u %g %a' conf"), "1000 1000 640");
    // WORKDIR made the directory as root: user app cannot write there.
    assert_eq!(s.run("users", "cat /tmp/w"), "cannot-write");
    assert_eq!(s.run("users", "id -un; pwd"), "app\n/srv/app");
}

/// Owner changes (metadata-only copies with composefs), renamed and replaced directories.
#[test]
fn metadata_changes_and_moved_directories() {
    let s = Setup::new();
    let dir = s.context(
        "moves",
        &[(
            "Dockerfile",
            "FROM @BASE@\nRUN chown 1000:1000 /etc/profile && chmod 600 /etc/profile\n\
             RUN mv /etc/apk /etc/apk-moved\n\
             RUN rm -rf /usr/share && mkdir -p /usr/share && echo only > /usr/share/one\n",
        )],
    );
    s.build(&dir, &["-t", "moved"]);
    let profile = s.run(
        "moved",
        "stat -c '%u %a' /etc/profile; grep -c PATH /etc/profile",
    );
    assert_eq!(profile, "1000 600\n1");
    assert_eq!(
        s.run(
            "moved",
            "test -e /etc/apk || ls /etc/apk-moved | grep -c repositories"
        ),
        "1"
    );
    assert_eq!(s.run("moved", "ls /usr/share"), "one");
}

/// A file served once over HTTP on localhost; returns its URL.
fn serve_once(body: &'static [u8]) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/files/tool.sh", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
        }
    });
    url
}

#[test]
fn onbuild_triggers_and_add_from_a_url() {
    let s = Setup::new();
    let parent = s.context(
        "parent",
        &[(
            "Dockerfile",
            "FROM @BASE@\nONBUILD RUN echo triggered > /trigger\nHEALTHCHECK NONE\n",
        )],
    );
    s.build(&parent, &["-t", "parent:1"]);
    let url = serve_once(b"#!/bin/sh\necho tool\n");
    let child = s.context(
        "child",
        &[(
            "Dockerfile",
            &format!(
                "FROM parent:1\nADD {url} /usr/local/bin/\nRUN chmod +x /usr/local/bin/tool.sh\n"
            ),
        )],
    );
    let out = s.build(&child, &["-t", "child:1"]);
    assert!(out.contains("Executing 1 build triggers"), "{out}");
    assert_eq!(s.run("child:1", "cat /trigger; tool.sh"), "triggered\ntool");
    // Triggers run once: not in images built on the child.
    let grandchild = s.context(
        "grandchild",
        &[("Dockerfile", "FROM child:1\nRUN rm /trigger\n")],
    );
    let out = s.build(&grandchild, &["-t", "grandchild:1"]);
    assert!(!out.contains("build triggers"), "{out}");
}

#[test]
fn failures_are_clear() {
    let s = Setup::new();
    let dir = s.context(
        "fail",
        &[("Dockerfile", "FROM @BASE@\nRUN echo before; exit 3\n")],
    );
    let out = s.podbag(&["build", "--network", "none", dir.to_str().unwrap()]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("before"), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("returned a non-zero code: 3"),
        "{}",
        stderr(&out)
    );
    let dir = s.context(
        "mount",
        &[(
            "Dockerfile",
            "FROM @BASE@\nRUN --mount=type=cache,target=/c true\n",
        )],
    );
    let out = s.podbag(&["build", dir.to_str().unwrap()]);
    assert!(stderr(&out).contains("buildx"), "{}", stderr(&out));
    // Nothing is left: no containers, no context copies.
    assert_eq!(s.ok(&["ps", "-aq"]), "");
    let builds = s.sb.data().join("builds");
    assert!(
        std::fs::read_dir(&builds).map_or(true, |d| d.count() == 0),
        "left in {}",
        builds.display()
    );
}

#[test]
fn a_private_base_image_uses_the_login() {
    // Made at run time: no secret in the source.
    let user = format!("builder{}", std::process::id());
    let password = format!("pw-{}", std::process::id());
    let reg = FakeRegistry::start_with(Auth::token(&user, &password));
    let host = reg.host.clone();
    let mut sb = Sandbox::new();
    let config = sb.tmp.path().join("docker-config");
    sb.env
        .push(("DOCKER_CONFIG".to_string(), config.display().to_string()));
    let s = Setup::on(reg, sb);
    let dir = s.context(
        "private",
        &[("Dockerfile", "FROM @BASE@\nRUN echo private > /p\n")],
    );
    let ctx = dir.display().to_string();

    // Without a login: the pull is refused, with a hint to log in.
    let out = s.podbag(&["build", "--network", "none", &ctx]);
    assert_ne!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(
        stderr(&out).contains(&format!("podbag login {host}")),
        "{}",
        stderr(&out)
    );

    let mut child =
        s.sb.podbag()
            .args(["login", &host, "-u", &user, "--password-stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
    use std::io::Write;
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{password}").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let out = s.build(&dir, &["-t", "private:1"]);
    assert!(out.contains(&format!("Pulling {}", s.base)), "{out}");
    assert!(!out.contains(&password), "{out}");
    assert_eq!(s.run("private:1", "cat /p"), "private");
}

#[test]
fn a_dockerfile_on_stdin() {
    let s = Setup::new();
    let mut child =
        s.sb.podbag()
            .args(["build", "-t", "piped", "--network", "none", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("FROM {}\nRUN echo piped > /p\n", s.base).as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(s.run("piped", "cat /p"), "piped");
}

#[test]
fn commit_export_and_diff() {
    let s = Setup::new();
    s.ok(&["pull", &s.base]);
    s.ok(&[
        "run",
        "-d",
        "--name",
        "box",
        "--network",
        "none",
        &s.base,
        "sleep",
        "300",
    ]);
    s.ok(&[
        "exec",
        "box",
        "sh",
        "-c",
        "mkdir -p /data && echo data > /data/new && rm /etc/motd && echo x >> /etc/hosts.allow",
    ]);
    let diff = s.ok(&["diff", "box"]);
    for line in ["A /data", "A /data/new", "D /etc/motd", "C /etc"] {
        assert!(
            diff.lines().any(|l| l == line),
            "{line:?} missing in:\n{diff}"
        );
    }
    // Commit while it runs; then again after it stopped.
    let id = s.ok(&[
        "commit",
        "-c",
        "CMD [\"cat\", \"/data/new\"]",
        "box",
        "snap:1",
    ]);
    assert!(id.starts_with("sha256:"), "{id}");
    assert_eq!(
        s.ok(&["run", "--rm", "--network", "none", "snap:1"]),
        "data"
    );
    assert_eq!(s.run("snap:1", "test -e /etc/motd || echo gone"), "gone");
    s.ok(&["stop", "-t", "1", "box"]);
    s.ok(&["commit", "box", "snap:2"]);
    assert_eq!(s.run("snap:2", "cat /data/new"), "data");

    let tar = s.sb.tmp.path().join("box.tar");
    s.ok(&["export", "-o", tar.to_str().unwrap(), "box"]);
    let list = std::process::Command::new("tar")
        .arg("-tf")
        .arg(&tar)
        .output()
        .unwrap();
    let names = stdout(&list);
    assert!(names.lines().any(|l| l == "data/new"), "{names}");
    assert!(names.lines().any(|l| l == "bin/busybox"), "{names}");
    assert!(!names.lines().any(|l| l == "etc/motd"), "{names}");
    s.ok(&["rm", "-f", "box"]);
}
