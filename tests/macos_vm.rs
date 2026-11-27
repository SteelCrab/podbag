#![cfg(target_os = "macos")]
//! podbag on the Mac: commands run in the Linux VM it starts and stops itself. Needs the
//! guest (`dev/vm.sh run dev/macos/build-guest.sh`) and runs only with
//! PODBAG_TEST_MACOS_VM=1 (it boots a VM). Each test has a home of its own, so its own VM.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn enabled() -> bool {
    let on = std::env::var("PODBAG_TEST_MACOS_VM").as_deref() == Ok("1");
    if !on {
        eprintln!("PODBAG_TEST_MACOS_VM is not 1: skipped");
    }
    on
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The guest files (PODBAG_TEST_GUEST: another build's, to compare with).
fn guest() -> PathBuf {
    std::env::var_os("PODBAG_TEST_GUEST").map_or_else(|| repo().join("target/vm"), PathBuf::from)
}

/// The binary, signed with the virtualization entitlement, once for all tests (codesign
/// replaces the file: two at once trip over each other).
fn podbag_bin() -> PathBuf {
    static SIGNED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SIGNED
        .get_or_init(|| {
            // PODBAG_TEST_BIN: another build, to compare with (an older one fails).
            let bin = std::env::var_os("PODBAG_TEST_BIN").map_or_else(
                || PathBuf::from(env!("CARGO_BIN_EXE_podbag")),
                PathBuf::from,
            );
            let signed = Command::new(repo().join("dev/macos/sign.sh"))
                .arg(&bin)
                .output()
                .unwrap();
            assert!(
                signed.status.success(),
                "{}",
                String::from_utf8_lossy(&signed.stderr)
            );
            bin
        })
        .clone()
}

/// The images the tests use, by the names they use, and where they come from: Google's
/// Docker Hub mirror. Docker Hub allows 100 anonymous pulls an hour, which one run of
/// these tests would use up if each test pulled.
const IMAGES: &[(&str, Option<(&str, &str)>)] = &[
    (
        "mirror.gcr.io/library/alpine:3.22",
        Some(("alpine", "3.22")),
    ),
    (
        "mirror.gcr.io/library/python:3.13-alpine",
        Some(("python", "3.13-alpine")),
    ),
    // BuildKit for buildx, by its mirror name (the builder is told to use it).
    ("mirror.gcr.io/moby/buildkit:buildx-stable-1", None),
];

/// A data disk with [`IMAGES`], made once per run (in `target/macos-test-seed`) and
/// cloned into each test's home: APFS clones share blocks, so this costs no space.
fn seed_disk() -> PathBuf {
    static SEED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SEED.get_or_init(|| {
        let seed = Mac::named(60, "seed");
        for (from, name) in IMAGES {
            seed.ok(&["pull", from]);
            let Some((repo, tag)) = name else {
                continue;
            };
            // `docker tag`, through the Docker API.
            let tagged = Command::new("curl")
                .args(["-s", "-f", "-X", "POST", "--unix-socket"])
                .arg(seed.docker_sock())
                .arg(format!(
                    "http://localhost/images/{from}/tag?repo={repo}&tag={tag}"
                ))
                .output()
                .unwrap();
            assert!(tagged.status.success(), "tag {from}: {tagged:?}");
        }
        seed.ok(&["vm", "stop"]);
        let dir = repo().join("target/macos-test-seed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.img");
        std::fs::rename(seed.home.path().join("disk.img"), &disk).unwrap();
        disk
    })
    .clone()
}

/// A Mac home of its own (`PODBAG_MAC_HOME`): its own VM, disk and sockets. The VM is
/// stopped on drop.
struct Mac {
    home: Home,
    bin: PathBuf,
    idle: u32,
}

/// A home directory, removed on drop.
struct Home(PathBuf);

impl Home {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Mac {
    /// A home whose disk already has the test images (a clone of the seed).
    fn new(idle_secs: u32) -> Mac {
        let mac = Mac::bare(idle_secs);
        let cloned = Command::new("cp")
            .arg("-c")
            .arg(seed_disk())
            .arg(mac.home.path().join("disk.img"))
            .status()
            .unwrap();
        assert!(cloned.success());
        mac
    }

    /// A home with a new, empty disk.
    fn bare(idle_secs: u32) -> Mac {
        Mac::named(idle_secs, std::thread::current().name().unwrap_or("main"))
    }

    /// A home with a new, empty disk, at a path that is the same in every run (from
    /// `name`): its VM keeps its MAC address, so the Mac's DHCP server gives it the
    /// address it had instead of one more of its 253 (leased for an hour). Short: a Unix
    /// socket path is limited to 104 bytes on macOS.
    fn named(idle_secs: u32, name: &str) -> Mac {
        let mut hash: u32 = 0x811c_9dc5;
        for b in name.bytes() {
            hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
        }
        let path = PathBuf::from(format!("/tmp/pbm-{hash:08x}"));
        let bin = podbag_bin();
        if path.exists() {
            // Left by a run that was cut short: its VM would have the same address.
            let _ = Command::new(&bin)
                .args(["vm", "stop"])
                .env("PODBAG_MAC_HOME", &path)
                .output();
            let _ = std::fs::remove_dir_all(&path);
        }
        std::fs::create_dir_all(&path).unwrap();
        Mac {
            home: Home(path),
            bin,
            idle: idle_secs,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(args)
            .env("PODBAG_MAC_HOME", self.home.path())
            .env("PODBAG_VM_GUEST", guest())
            .env("PODBAG_VM_MEMORY", "1024")
            .env("PODBAG_VM_IDLE", self.idle.to_string());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "podbag {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    fn sock(&self) -> PathBuf {
        self.home.path().join("run/vm.sock")
    }

    fn docker_sock(&self) -> PathBuf {
        self.home.path().join("run/docker.sock")
    }

    /// Wait (30 s at most) until `vm status` says the VM is off.
    fn wait_until_stopped(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.ok(&["vm", "status"]) != "stopped" {
            assert!(Instant::now() < deadline, "the VM never stopped");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The process that holds this home's lock (the VM host, or its standby).
    fn host_pid(&self) -> Option<i32> {
        let lsof = Command::new("lsof")
            .arg("-t")
            .arg(self.home.path().join("run/host.lock"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&lsof.stdout).trim().parse().ok()
    }
}

impl Drop for Mac {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // The home goes with the test: show why its VM did what it did.
            for log in ["run/host.log", "console.log"] {
                let text = std::fs::read_to_string(self.home.path().join(log)).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                let tail = &lines[lines.len().saturating_sub(40)..];
                eprintln!("--- {log} (last {} lines)\n{}", tail.len(), tail.join("\n"));
            }
        }
        let _ = self.run(&["vm", "stop"]);
    }
}

#[test]
fn saved_resources_apply_on_the_next_boot() {
    if !enabled() {
        return;
    }
    let mac = Mac::bare(3);
    let check = |cpus: u64, mib: u64| {
        let out = Command::new("curl")
            .args(["-fsS", "--max-time", "45", "--unix-socket"])
            .arg(mac.docker_sock())
            .arg("http://localhost/info")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(info["NCPU"], cpus);
        // The guest reports usable memory after the kernel reserves its share.
        let usable_mib = info["MemTotal"].as_u64().unwrap() >> 20;
        assert!((mib - 128..=mib).contains(&usable_mib), "{usable_mib} MiB");
    };
    mac.ok(&["vm", "resources", "1", "768"]);
    assert_eq!(mac.ok(&["vm", "status"]), "stopped");
    let start = mac
        .cmd(&["vm", "start"])
        .env_remove("PODBAG_VM_CPUS")
        .env_remove("PODBAG_VM_MEMORY")
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    check(1, 768);
    mac.ok(&["vm", "resources", "2", "1280"]);
    check(1, 768);
    mac.wait_until_stopped();
    let saved: serde_json::Value = serde_json::from_str(&mac.ok(&["vm", "resources"])).unwrap();
    assert_eq!(saved["values"]["memoryMib"], 1280);
    assert_eq!(mac.ok(&["vm", "status"]), "stopped");
    check(2, 1280); // The request wakes the standby.
    mac.ok(&["vm", "stop"]);
    // Mac::cmd sets PODBAG_VM_MEMORY to 1024. The saved CPU count still applies.
    mac.ok(&["vm", "start"]);
    check(2, 1024);
}

#[test]
fn commands_run_in_the_vm() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    let started = Instant::now();
    assert_eq!(mac.ok(&["run", "--rm", "alpine:3.22", "echo", "hi"]), "hi");
    eprintln!("first run, VM included: {:?}", started.elapsed());
    // Exit codes and stdin.
    let failed = mac.run(&["run", "--rm", "alpine:3.22", "sh", "-c", "exit 3"]);
    assert_eq!(failed.status.code(), Some(3));
    let mut cat = mac
        .cmd(&["run", "--rm", "-i", "alpine:3.22", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    cat.stdin
        .take()
        .unwrap()
        .write_all(b"through stdin\n")
        .unwrap();
    let out = cat.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "through stdin\n");
    // A Mac directory under /Users, mounted by a relative path.
    let dir = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    std::fs::write(dir.path().join("note"), "from the mac\n").unwrap();
    let read = mac
        .cmd(&[
            "run",
            "--rm",
            "-v",
            "./:/work",
            "alpine:3.22",
            "cat",
            "/work/note",
        ])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&read.stdout),
        "from the mac\n",
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    // Detached, listed, entered, removed.
    mac.ok(&["run", "-d", "--name", "bg", "alpine:3.22", "sleep", "300"]);
    assert!(mac.ok(&["ps"]).contains("bg"));
    assert_eq!(mac.ok(&["exec", "bg", "echo", "inside"]), "inside");
    mac.ok(&["rm", "-f", "bg"]);
}

#[test]
fn two_commands_share_one_vm_and_it_stops_when_idle() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(5);
    // Both start while no VM runs: one host makes it, both use it.
    let a = mac
        .cmd(&["run", "--rm", "alpine:3.22", "true"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let b = mac
        .cmd(&["run", "--rm", "alpine:3.22", "true"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    assert!(a.wait_with_output().unwrap().status.success());
    assert!(b.wait_with_output().unwrap().status.success());
    let hosts = Command::new("pgrep")
        .args(["-f", &format!("{} __vm-host", mac.bin.display())])
        .output()
        .unwrap();
    // pgrep sees hosts of other tests too; this home's socket tells ours apart.
    assert!(!String::from_utf8_lossy(&hosts.stdout).trim().is_empty());
    assert!(mac.sock().exists());
    // Idle for 5 seconds with nothing running: the VM goes.
    mac.wait_until_stopped();
}

#[test]
fn an_unsigned_binary_says_how_to_sign_it() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("podbag");
    std::fs::copy(env!("CARGO_BIN_EXE_podbag"), &copy).unwrap();
    // Strip the signature the linker gave it, then sign without the entitlement.
    let _ = Command::new("codesign")
        .args(["--remove-signature"])
        .arg(&copy)
        .output();
    let _ = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&copy)
        .output();
    let home = tempfile::Builder::new()
        .prefix("opu")
        .tempdir_in("/tmp")
        .unwrap();
    // Two at once: one host fails, the other finds it starting; both say so quickly.
    let started = Instant::now();
    let spawn = || {
        Command::new(&copy)
            .args(["run", "--rm", "alpine:3.22", "true"])
            .env("PODBAG_MAC_HOME", home.path())
            .env("PODBAG_VM_GUEST", guest())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    for out in [a.wait_with_output().unwrap(), b.wait_with_output().unwrap()] {
        let err = String::from_utf8_lossy(&out.stderr);
        assert_ne!(out.status.code(), Some(0));
        assert!(err.contains("sign"), "{err}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "it took {:?}",
        started.elapsed()
    );
    assert!(!Path::new(&home.path().join("run/vm.sock")).exists());
    // Arguments are checked here, without a VM.
    let bad = Command::new(&copy)
        .args(["ps", "--no-such-option"])
        .env("PODBAG_MAC_HOME", home.path())
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
}

/// Run `podbag args` under a new terminal of `rows`×`cols`; returns the master side.
fn under_terminal(
    mac: &Mac,
    args: &[&str],
    rows: u16,
    cols: u16,
) -> (std::fs::File, std::process::Child) {
    use std::os::fd::FromRawFd;
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty fills two descriptors.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    assert_eq!(r, 0);
    // SAFETY: new descriptors from openpty.
    // Only this process holds the master: when it closes, the terminal hangs up.
    // SAFETY: FD_CLOEXEC on a descriptor openpty just made.
    unsafe { libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: new descriptors from openpty.
    let (master, slave) = unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    };
    let mut cmd = mac.cmd(args);
    cmd.stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid and TIOCSCTTY are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    (master, child)
}

/// Read from `master` until `want` shows up (or time runs out); returns what was read.
fn read_until(master: &mut std::fs::File, want: &str, secs: u64) -> String {
    use std::io::Read;
    let fd = std::os::fd::AsRawFd::as_raw_fd(master);
    // SAFETY: non-blocking reads on our own descriptor.
    unsafe {
        libc::fcntl(
            fd,
            libc::F_SETFL,
            libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
        )
    };
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut seen = String::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline && !seen.contains(want) {
        match master.read(&mut buf) {
            Ok(n) if n > 0 => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            _ => std::thread::sleep(Duration::from_millis(30)),
        }
    }
    seen
}

#[test]
fn a_terminal_reaches_the_container() {
    if !enabled() {
        return;
    }
    use std::io::Write;
    let mac = Mac::new(60);
    let (mut master, mut child) =
        under_terminal(&mac, &["run", "--rm", "-it", "alpine:3.22", "sh"], 33, 101);
    assert!(read_until(&mut master, "#", 60).contains('#'), "no prompt");
    // The size of the Mac's terminal.
    master.write_all(b"stty size\r").unwrap();
    let size = read_until(&mut master, "33 101", 10);
    assert!(size.contains("33 101"), "{size:?}");
    // A new size reaches it.
    let size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ on the terminal we made; its foreground group gets SIGWINCH.
    unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(&master),
            libc::TIOCSWINSZ,
            &size,
        )
    };
    std::thread::sleep(Duration::from_millis(500));
    master.write_all(b"stty size\r").unwrap();
    let resized = read_until(&mut master, "40 120", 10);
    assert!(resized.contains("40 120"), "{resized:?}");
    // Ctrl+C ends what runs in the foreground, not the shell.
    master.write_all(b"sleep 100\r").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    master.write_all(&[3]).unwrap();
    master.write_all(b"echo still-$((40+2))\r").unwrap();
    let after = read_until(&mut master, "still-42", 10);
    assert!(after.contains("still-42"), "{after:?}");
    master.write_all(b"exit 5\r").unwrap();
    // Read on while it ends: a terminal's last process waits for its output to be read.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "it did not end");
        let _ = read_until(&mut master, "\0", 1);
    };
    assert_eq!(status.code(), Some(5));
}

#[test]
fn a_running_container_keeps_the_vm() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(3);
    mac.ok(&["run", "-d", "--name", "keep", "alpine:3.22", "sleep", "300"]);
    // Well past the idle time, the container still runs.
    std::thread::sleep(Duration::from_secs(10));
    assert_eq!(mac.ok(&["vm", "status"]), "running");
    assert!(mac.ok(&["ps"]).contains("keep"));
    mac.ok(&["rm", "-f", "keep"]);
    mac.wait_until_stopped();
}

#[test]
fn only_paths_under_users_reach_the_vm() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let out = mac
        .cmd(&["run", "--rm", "-v", "./:/work", "alpine:3.22", "true"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    assert!(err.contains("/Users"), "{err}");
    // Named volumes are the VM's own.
    mac.ok(&["run", "--rm", "-v", "data:/data", "alpine:3.22", "true"]);
}

#[test]
fn containers_cannot_reach_the_agent() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // vsock from a container, to every address that could lead back into the VM: an
    // answer would mean root in the VM and the Mac's /Users.
    let script = r#"import socket
for cid, port in [(c, p) for c in (1, 2, 3) for p in (2, 3, 2375)]:
    try:
        s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        s.settimeout(3)
        s.connect((cid, port))
        s.sendall(b'{"argv":["ps"],"env":[],"cwd":"/","tty":false,"rows":0,"cols":0,"time":[0,0]}\n')
        data = s.recv(64)
        print("REACHED" if data else "closed", cid, port, data)
    except Exception as e:
        print("refused", cid, port, e)
"#;
    for extra in [&[][..], &["--privileged"][..]] {
        let mut args = vec!["run", "--rm"];
        args.extend(extra);
        args.extend(["python:3.13-alpine", "python3", "-c", script]);
        let out = mac.ok(&args);
        assert!(!out.contains("REACHED"), "{extra:?}: {out}");
    }
}

#[test]
fn a_command_ends_when_its_terminal_goes() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // A program that ends on a hangup. (A container's PID 1 without a handler ignores
    // it, here as with a local terminal and with Docker.)
    let (master, mut child) = under_terminal(
        &mac,
        &[
            "run",
            "-it",
            "--name",
            "hup",
            "alpine:3.22",
            "sh",
            "-c",
            "trap 'exit 0' HUP; while :; do sleep 1; done",
        ],
        24,
        80,
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !mac.ok(&["ps"]).contains("hup") {
        assert!(Instant::now() < deadline, "it never ran");
        std::thread::sleep(Duration::from_millis(200));
    }
    // The terminal closes (a closed window): like a local terminal, the command and its
    // container get a hangup.
    drop(master);
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the command outlived its terminal"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    while mac.ok(&["ps"]).contains("hup") {
        assert!(
            Instant::now() < deadline,
            "the container outlived its terminal"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_reader_that_leaves_ends_the_command() {
    if !enabled() {
        return;
    }
    use std::io::BufRead;
    let mac = Mac::new(60);
    // A reader that takes one line and leaves: the command ends too.
    let mut yes = mac
        .cmd(&["run", "--rm", "alpine:3.22", "yes"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    std::io::BufReader::new(yes.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert_eq!(first, "y\n");
    let deadline = Instant::now() + Duration::from_secs(20);
    while yes.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "it kept running for a reader that left"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_slow_reader_gets_everything() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // Readers that wait before they read, for outputs around what the buffers between
    // the VM and the Mac hold: nothing is lost, whatever the size.
    std::thread::scope(|s| {
        for step in 0..17usize {
            let mac = &mac;
            s.spawn(move || {
                use std::io::Read;
                let n = 192_000 + step * 48_000;
                let script = format!("head -c {n} /dev/zero | base64 -w0");
                let mut c = mac
                    .cmd(&["run", "--rm", "alpine:3.22", "sh", "-c", &script])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                std::thread::sleep(Duration::from_secs(5));
                let mut all = Vec::new();
                c.stdout.take().unwrap().read_to_end(&mut all).unwrap();
                assert!(c.wait().unwrap().success());
                assert_eq!(all.len(), n.div_ceil(3) * 4, "{n} bytes in");
            });
        }
    });
}

#[test]
fn input_stays_for_who_reads_it() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // Without -i the command does not take the caller's input: `cat` gets it.
    let script = format!(
        "{{ {} run --rm alpine:3.22 true; cat; }} <<'EOF'\nleft for cat\nEOF",
        mac.bin.display()
    );
    let base = mac.cmd(&[]);
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .envs(base.get_envs().filter_map(|(k, v)| Some((k, v?))))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "left for cat\n");
    // With -i it does.
    let mut cat = mac
        .cmd(&["run", "--rm", "-i", "alpine:3.22", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    cat.stdin
        .take()
        .unwrap()
        .write_all(b"for the container\n")
        .unwrap();
    let out = cat.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "for the container\n");
}

#[test]
fn a_bare_env_name_takes_the_macs_value() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    let token = mac
        .cmd(&[
            "run",
            "--rm",
            "-e",
            "OPM_TOKEN",
            "--env=OPM_OTHER",
            "--env",
            "OPM_THIRD",
            "-e",
            "OPM_UNSET",
            "alpine:3.22",
            "sh",
            "-c",
            "echo $OPM_TOKEN/$OPM_OTHER/$OPM_THIRD/${OPM_UNSET-none} -e OPM_TOKEN",
        ])
        .env("OPM_TOKEN", "abc")
        .env("OPM_OTHER", "d=e")
        .env("OPM_THIRD", "f")
        .env_remove("OPM_UNSET")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&token.stdout).trim(),
        "abc/d=e/f/none -e OPM_TOKEN",
        "{}",
        String::from_utf8_lossy(&token.stderr)
    );
}

#[test]
fn a_restarting_container_keeps_the_vm() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(3);
    // It fails at once and waits longer each time before it starts again.
    mac.ok(&[
        "run",
        "-d",
        "--name",
        "again",
        "--restart",
        "always",
        "alpine:3.22",
        "sh",
        "-c",
        "exit 1",
    ]);
    std::thread::sleep(Duration::from_secs(15));
    assert_eq!(mac.ok(&["vm", "status"]), "running");
    mac.ok(&["rm", "-f", "again"]);
}

#[test]
fn a_containers_own_options_are_not_mounts() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    // `-v` after the image belongs to the container's command.
    let ls = mac
        .cmd(&["run", "--rm", "alpine:3.22", "ls", "-v", "/"])
        .current_dir(tmp.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        ls.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ls.stderr)
    );
}

#[test]
fn mac_paths_outside_users_are_refused_however_written() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    let home = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    let up = "../".repeat(home.path().components().count());
    let dotted = format!("{up}tmp:/x");
    let link = home.path().join("elsewhere");
    std::os::unix::fs::symlink("/tmp", &link).unwrap();
    let linked = format!("{}:/x", link.display());
    for args in [
        &["run", "--rm", "-v", &dotted, "alpine:3.22", "true"][..],
        &["run", "--rm", "-itv", "/tmp:/x", "alpine:3.22", "true"],
        &["run", "--rm", "--volume=/tmp:/x", "alpine:3.22", "true"],
        &["run", "--rm", "-v", &linked, "alpine:3.22", "true"],
    ] {
        let out = mac
            .cmd(args)
            .current_dir(home.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(125), "{args:?}: {err}");
        assert!(err.contains("only /Users"), "{args:?}: {err}");
    }
    // `-v/tmp:/x` is no mount at all: the argument parser takes it for the image, as for a
    // local command.
    let attached = mac
        .cmd(&["run", "--rm", "-v/tmp:/x", "alpine:3.22", "true"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_ne!(attached.status.code(), Some(0));
    // A file the VM would write where the Mac cannot see it.
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    for args in [
        &["save", "-o", "out.tar", "alpine:3.22"][..],
        &["load", "-i", "/tmp/in.tar"],
    ] {
        let out = mac
            .cmd(args)
            .current_dir(tmp.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(125), "{args:?}: {err}");
        assert!(err.contains("only /Users"), "{args:?}: {err}");
    }
    // Under /Users, a file round trip works.
    let saved = mac
        .cmd(&["save", "-o", "out.tar", "alpine:3.22"])
        .current_dir(home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(saved.status.code(), Some(0));
    assert!(home.path().join("out.tar").metadata().unwrap().len() > 1_000_000);
}

#[test]
fn synchronized_share_moves_edits_across_the_vm_restart() {
    if !enabled() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let mac = Mac::bare(60);
    let project = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    let source = project.path().join("original");
    std::fs::write(&source, "from host\n").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o750)).unwrap();
    let name = project.path().canonicalize().unwrap();
    let name = name.to_str().unwrap();
    let status = |args: &[&str]| {
        let text = mac.ok(args);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value["lastError"].is_null(), "{value:#}");
        assert_eq!(
            value["conflicts"],
            serde_json::json!([]),
            "{args:?}: {value:#}"
        );
        value
    };
    let share = status(&["vm", "sync", "add", "project", name]);
    assert_eq!(share["volume"], "podbag-sync-project");
    let alpine = IMAGES[0].0;
    mac.ok(&["pull", alpine]);
    let empty = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    let empty_path = empty.path().canonicalize().unwrap();
    status(&["vm", "sync", "add", "empty", empty_path.to_str().unwrap()]);
    mac.ok(&[
        "run",
        "--rm",
        "-v",
        "podbag-sync-empty:/etc",
        alpine,
        "sh",
        "-ec",
        "test ! -e /etc/alpine-release",
    ]);
    status(&["vm", "sync", "remove", "empty"]);
    let run = |script| {
        mac.ok(&[
            "run",
            "--rm",
            "-v",
            "podbag-sync-project:/work",
            alpine,
            "sh",
            "-ec",
            script,
        ])
    };

    assert_eq!(
        run("cat /work/original; stat -c %a /work/original"),
        "from host\n750"
    );
    run(
        "mv /work/original /work/renamed; echo 'from guest' > /work/renamed; chmod 640 /work/renamed",
    );
    status(&["vm", "sync", "now", "project"]);
    assert!(!source.exists());
    let renamed = project.path().join("renamed");
    assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "from guest\n");
    assert_eq!(
        renamed.metadata().unwrap().permissions().mode() & 0o777,
        0o640
    );
    let old_file = std::fs::read_dir(project.path().join(".podbag-sync-recovery"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("file");
    assert_eq!(std::fs::read_to_string(old_file).unwrap(), "from host\n");

    mac.ok(&["vm", "stop"]);
    std::fs::write(&renamed, "edited while off\n").unwrap();
    status(&["vm", "sync", "now", "project"]);
    assert_eq!(run("cat /work/renamed"), "edited while off");
    std::fs::remove_file(&renamed).unwrap();
    status(&["vm", "sync", "now", "project"]);
    run("test ! -e /work/renamed");
    status(&["vm", "sync", "remove", "project"]);
    assert_eq!(mac.ok(&["vm", "sync", "list"]), "[]");
    assert!(mac.ok(&["volume", "ls"]).contains("podbag-sync-project"));
}

#[test]
fn a_broken_data_disk_is_an_error_not_a_ram_disk() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // A disk with no file system on it.
    let disk = std::fs::File::create(mac.home.path().join("disk.img")).unwrap();
    disk.set_len(1 << 30).unwrap();
    let out = mac.run(&["run", "--rm", "alpine:3.22", "true"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{err}");
    assert!(err.contains("data disk"), "{err}");
}

#[test]
fn a_stopped_host_keeps_what_was_written() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&[
        "run",
        "--rm",
        "-v",
        "keep:/d",
        "alpine:3.22",
        "sh",
        "-c",
        "echo persisted > /d/f",
    ]);
    // Logging out ends the host with SIGTERM. It is the one process holding its lock.
    let pid = mac.host_pid().expect("one host");
    // SAFETY: a signal to the host this test started.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(40);
    while mac.sock().exists() {
        assert!(Instant::now() < deadline, "the host did not stop");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        mac.ok(&["run", "--rm", "-v", "keep:/d", "alpine:3.22", "cat", "/d/f"]),
        "persisted"
    );
    // The VM's files are the user's alone.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(mac.home.path().join("run"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn commands_while_the_vm_stops_all_succeed() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    // An old error in the host's log is not news.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(mac.home.path().join("run/host.log"))
        .unwrap()
        .write_all(b"podbag: an old error\n")
        .unwrap();
    // A container's PID 1 ignores SIGTERM: the VM takes its five seconds to stop.
    mac.ok(&["run", "-d", "alpine:3.22", "sleep", "1000"]);
    // Commands that come while the VM stops (as when its idle time ends) wait for the
    // next VM instead of failing.
    std::thread::scope(|s| {
        s.spawn(|| mac.ok(&["vm", "stop"]));
        std::thread::sleep(Duration::from_millis(1500));
        for i in 0..3 {
            let out = mac.run(&["run", "--rm", "alpine:3.22", "true"]);
            assert_eq!(
                out.status.code(),
                Some(0),
                "run {i}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    });
}

#[test]
fn mac_settings_stay_on_the_mac() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // Paths and programs of the Mac mean nothing in the VM: the VM's store and its
    // podbag are used.
    let ran = mac
        .cmd(&["run", "--rm", "alpine:3.22", "true"])
        .env("PODBAG_DATA_DIR", "/tmp/pbm-elsewhere")
        .env("PODBAG_BIN", "/nonexistent/podbag")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        ran.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
}

#[test]
fn containers_get_generous_limits() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // Like Docker Desktop's VM: many open files, and room for Elasticsearch's maps.
    let out = mac.ok(&[
        "run",
        "--rm",
        "alpine:3.22",
        "sh",
        "-c",
        "ulimit -n; ulimit -Hn; cat /proc/sys/vm/max_map_count",
    ]);
    assert_eq!(out, "1048576\n1048576\n262144");
}

/// Send `argv` to the guest agent as the host's own commands are sent, and wait for the
/// connection to end.
fn agent_request(mac: &Mac, argv: &[&str]) {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(mac.sock()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = podbag::vm::proto::CliRequest {
        argv: argv.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
        cwd: "/".into(),
        tty: false,
        rows: 0,
        cols: 0,
        time: (0, 0),
    };
    let line = format!("PORT 2\n{}\n", serde_json::to_string(&req).unwrap());
    s.write_all(line.as_bytes()).unwrap();
    let mut sink = Vec::new();
    let _ = s.read_to_end(&mut sink);
}

#[test]
fn the_vm_recovers_from_a_lost_agent() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    // Without its agent, the VM still stops when told to.
    agent_request(&mac, &["__vm-agent-exit"]);
    mac.ok(&["vm", "stop"]);
    assert!(!mac.sock().exists());
    // PID 1 starts a lost agent again.
    mac.ok(&["ps"]);
    agent_request(&mac, &["__vm-agent-exit"]);
    std::thread::sleep(Duration::from_millis(200));
    let deadline = Instant::now() + Duration::from_secs(20);
    while mac.run(&["ps"]).status.code() != Some(0) {
        assert!(Instant::now() < deadline, "the agent did not come back");
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn a_signal_is_not_held_up_by_unread_input() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // Endless input that the container never reads, then a signal from another process.
    let mut run = mac
        .cmd(&[
            "run",
            "--rm",
            "-i",
            "alpine:3.22",
            "sh",
            "-c",
            "trap 'exit 3' INT; sleep 1000 & wait",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = run.stdin.take().unwrap();
    std::thread::spawn(move || {
        use std::io::Write;
        let chunk = vec![b'y'; 64 * 1024];
        while input.write_all(&chunk).is_ok() {}
    });
    std::thread::sleep(Duration::from_secs(5));
    // SAFETY: a signal to the command this test started.
    unsafe { libc::kill(run.id() as i32, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = run.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the signal never reached the container"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status.code(), Some(3));
}

#[test]
fn the_sockets_stay_while_the_vm_is_off() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(2);
    mac.ok(&["ps"]);
    mac.wait_until_stopped();
    // The VM is off; its sockets stay for the next command or docker client.
    assert!(mac.sock().exists() && mac.docker_sock().exists());
    // A docker client's first request starts a VM and gets its answer.
    let ping = Command::new("curl")
        .args(["-s", "-m", "180", "--unix-socket"])
        .arg(mac.docker_sock())
        .arg("http://localhost/_ping")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&ping.stdout), "OK");
    assert_eq!(mac.ok(&["vm", "status"]), "running");
    mac.ok(&["ps"]);
}

#[test]
fn standby_uses_little_memory() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(2);
    mac.ok(&["ps"]);
    mac.wait_until_stopped();
    let pid = mac.host_pid().expect("a standby holds the lock");
    let rss = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let kib: u64 = String::from_utf8_lossy(&rss.stdout).trim().parse().unwrap();
    eprintln!("standby RSS: {kib} KiB");
    assert!(kib < 20 * 1024, "standby RSS {kib} KiB");
}

#[test]
fn vm_stop_ends_the_standby() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(2);
    mac.ok(&["ps"]);
    mac.wait_until_stopped();
    assert!(mac.sock().exists(), "no standby");
    mac.ok(&["vm", "stop"]);
    assert!(!mac.sock().exists() && !mac.docker_sock().exists());
    assert_eq!(mac.host_pid(), None);
}

/// Docker Desktop's docker CLI, if there is one.
fn have_docker() -> bool {
    let found = Command::new("docker")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("no docker CLI: skipped");
    }
    found
}

/// A docker config of the tests' own (so nothing is written in ~/.docker), with the
/// compose and buildx plugins of this Mac.
fn docker_config() -> PathBuf {
    static CONFIG: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let dir = repo().join("target/macos-test-docker");
            let _ = std::fs::remove_dir_all(&dir);
            let plugins = dir.join("cli-plugins");
            std::fs::create_dir_all(&plugins).unwrap();
            let home = PathBuf::from(std::env::var("HOME").unwrap());
            for p in ["docker-compose", "docker-buildx"] {
                let from = home.join(".docker/cli-plugins").join(p);
                if from.exists() {
                    std::os::unix::fs::symlink(&from, plugins.join(p)).unwrap();
                }
            }
            dir
        })
        .clone()
}

impl Mac {
    /// The docker CLI on this home's docker.sock: never the user's own Docker.
    fn docker(&self, args: &[&str]) -> Command {
        let mut c = Command::new("docker");
        c.arg("-H")
            .arg(format!("unix://{}", self.docker_sock().display()))
            .args(args)
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .env("DOCKER_CONFIG", docker_config())
            .stdin(Stdio::null());
        c
    }

    fn docker_ok(&self, args: &[&str]) -> String {
        let out = self.docker(args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "docker {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }
}

#[test]
fn docker_runs_containers() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    assert!(
        mac.docker_ok(&["version", "--format", "{{.Server.Version}}"])
            .len()
            > 2
    );
    assert_eq!(
        mac.docker_ok(&["run", "--rm", "alpine:3.22", "echo", "hi"]),
        "hi"
    );
    mac.docker_ok(&["run", "-d", "--name", "web", "alpine:3.22", "sleep", "100"]);
    assert!(mac.docker_ok(&["ps"]).contains("web"));
    assert_eq!(mac.docker_ok(&["exec", "web", "echo", "inside"]), "inside");
    mac.docker_ok(&["rm", "-f", "web"]);
}

#[test]
fn docker_binds_follow_the_mac_rule() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    let dir = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    std::fs::write(dir.path().join("note"), "from the mac\n").unwrap();
    let bind = format!("{}:/d", dir.path().display());
    assert_eq!(
        mac.docker_ok(&["run", "--rm", "-v", &bind, "alpine:3.22", "cat", "/d/note"]),
        "from the mac"
    );
    // A link under /Users that leads out of it is out too.
    std::os::unix::fs::symlink("/tmp", dir.path().join("out")).unwrap();
    let linked = format!("{}/out:/x", dir.path().display());
    for args in [
        &["run", "--rm", "-v", "/tmp:/x", "alpine:3.22", "true"][..],
        &[
            "run",
            "--rm",
            "--mount",
            "type=bind,source=/opt,target=/x",
            "alpine:3.22",
            "true",
        ],
        &["run", "--rm", "-v", &linked, "alpine:3.22", "true"],
    ] {
        let out = mac.docker(args).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_ne!(out.status.code(), Some(0), "{args:?}");
        assert!(err.contains("mounts denied"), "{args:?}: {err}");
    }
    // Named volumes are the VM's own.
    mac.docker_ok(&["run", "--rm", "-v", "data:/d", "alpine:3.22", "true"]);
}

#[test]
fn docker_waits_for_the_vm() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(2);
    mac.ok(&["ps"]);
    mac.wait_until_stopped();
    // The VM is off: the docker client's request starts it.
    assert_eq!(
        mac.docker_ok(&["run", "--rm", "alpine:3.22", "echo", "again"]),
        "again"
    );
}

#[test]
fn docker_clock_follows_the_mac() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    // The VM's clock goes wrong (as after the Mac slept)...
    mac.docker_ok(&[
        "run",
        "--rm",
        "--privileged",
        "alpine:3.22",
        "date",
        "-s",
        "@1000000000",
    ]);
    std::thread::sleep(Duration::from_secs(11));
    // ...and a docker client's next request finds it right again.
    let vm: u64 = mac
        .docker_ok(&["run", "--rm", "alpine:3.22", "date", "+%s"])
        .parse()
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(now.abs_diff(vm) < 10, "VM {vm}, Mac {now}");
}

#[test]
fn compose_up_and_down() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    let project = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    std::fs::create_dir(project.path().join("data")).unwrap();
    std::fs::write(project.path().join("data/index"), "served from the mac\n").unwrap();
    std::fs::write(
        project.path().join("compose.yaml"),
        r#"services:
  web:
    image: python:3.13-alpine
    command: ["python3", "-m", "http.server", "8000", "--directory", "/data"]
    volumes:
      - ./data:/data
  client:
    image: alpine:3.22
    command: ["sleep", "300"]
"#,
    )
    .unwrap();
    let compose = |args: &[&str]| {
        let mut all = vec!["compose", "-p", "mac2t"];
        all.extend(args);
        let mut c = mac.docker(&all);
        c.current_dir(project.path());
        c.output().unwrap()
    };
    let up = compose(&["up", "-d"]);
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );
    // In podbag's VM (not some other Docker).
    assert!(mac.ok(&["ps"]).contains("mac2t"));
    // The client finds the web service by its name, on the project's network; the page
    // comes from the Mac's directory.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let got = compose(&[
            "exec",
            "-T",
            "client",
            "wget",
            "-qO-",
            "http://web:8000/index",
        ]);
        if String::from_utf8_lossy(&got.stdout) == "served from the mac\n" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{}",
            String::from_utf8_lossy(&got.stderr)
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    // What a container writes there is on the Mac.
    let wrote = compose(&["exec", "-T", "web", "sh", "-c", "echo back > /data/back"]);
    assert!(wrote.status.success());
    assert_eq!(
        std::fs::read_to_string(project.path().join("data/back")).unwrap(),
        "back\n"
    );
    let down = compose(&["down", "-v"]);
    assert!(
        down.status.success(),
        "{}",
        String::from_utf8_lossy(&down.stderr)
    );
    assert!(!mac.docker_ok(&["ps", "-a"]).contains("mac2t"));
}

#[test]
fn buildx_builds_and_loads() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    let context = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    std::fs::write(
        context.path().join("Dockerfile"),
        "FROM mirror.gcr.io/library/alpine:3.22\nRUN echo built > /f\nCMD [\"cat\", \"/f\"]\n",
    )
    .unwrap();
    mac.docker_ok(&[
        "buildx",
        "create",
        "--name",
        "mac2b",
        "--driver",
        "docker-container",
        "--driver-opt",
        "image=mirror.gcr.io/moby/buildkit:buildx-stable-1",
    ]);
    let built = mac
        .docker(&[
            "buildx",
            "build",
            "--builder",
            "mac2b",
            "--load",
            "-t",
            "mac2:test",
            ".",
        ])
        .current_dir(context.path())
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert_eq!(mac.docker_ok(&["run", "--rm", "mac2:test"]), "built");
    mac.docker_ok(&["buildx", "rm", "mac2b"]);
}

#[test]
fn a_home_keeps_its_network_address() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    let address = || {
        mac.ok(&[
            "run",
            "--rm",
            "--network",
            "host",
            "alpine:3.22",
            "cat",
            "/sys/class/net/eth0/address",
        ])
    };
    let first = address();
    mac.ok(&["vm", "stop"]);
    // The next VM of this home has the same MAC address, so the Mac's DHCP server gives
    // it its address again instead of one more from its pool (253 addresses, leased for
    // an hour).
    assert_eq!(address(), first);
}

impl Mac {
    /// How many lines of this home's host log contain `what`.
    fn log_count(&self, what: &str) -> usize {
        std::fs::read_to_string(self.home.path().join("run/host.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(what))
            .count()
    }

    /// Wait (60 s at most) until the host log has `n` lines with `what`.
    fn wait_for_log(&self, what: &str, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.log_count(what) < n {
            assert!(Instant::now() < deadline, "no {what:?} {n} in the host log");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn vm_stop_at_the_standby_switch_ends_everything() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(1);
    for cycle in 1..=6 {
        mac.ok(&["ps"]);
        // Its VM is off: the host now starts the standby and ends.
        mac.wait_for_log("]: stopped", cycle);
        mac.ok(&["vm", "stop"]);
        assert_eq!(
            mac.host_pid(),
            None,
            "cycle {cycle}: something holds the lock"
        );
        assert!(!mac.sock().exists(), "cycle {cycle}: the socket stayed");
    }
}

#[test]
fn a_vm_that_cannot_start_says_so_to_docker_clients() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(2);
    mac.ok(&["ps"]);
    mac.wait_until_stopped();
    // The disk breaks while the VM is off: its file system's superblock goes.
    use std::os::unix::fs::FileExt;
    std::fs::OpenOptions::new()
        .write(true)
        .open(mac.home.path().join("disk.img"))
        .unwrap()
        .write_all_at(&[0u8; 4096], 1024)
        .unwrap();
    for attempt in 1..=2 {
        let out = mac.docker(&["ps"]).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_ne!(out.status.code(), Some(0), "attempt {attempt}");
        assert!(err.contains("data disk"), "attempt {attempt}: {err}");
    }
    let cli = mac.run(&["ps"]);
    let err = String::from_utf8_lossy(&cli.stderr);
    assert_eq!(cli.status.code(), Some(125), "{err}");
    assert!(err.contains("data disk"), "{err}");
}

#[test]
fn clients_during_the_idle_stop_are_served() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(2);
    mac.ok(&["ps"]);
    // The moment the host decides to stop: these wait in the sockets' queues for the
    // standby to start the next VM.
    mac.wait_for_log("stopping (idle)", 1);
    let ping = Command::new("curl")
        .args(["-s", "-m", "180", "--unix-socket"])
        .arg(mac.docker_sock())
        .arg("http://localhost/_ping")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&ping.stdout), "OK");
    mac.docker_ok(&["ps"]);
    mac.ok(&["ps"]);
}

#[test]
fn the_vm_host_keeps_no_directory_busy() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // Started from a directory the user may want to remove or eject.
    let dir = tempfile::tempdir().unwrap();
    let out = mac
        .cmd(&["ps"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    let pid = mac.host_pid().expect("a host");
    let cwd = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let cwd = String::from_utf8_lossy(&cwd.stdout);
    assert!(cwd.lines().any(|l| l == "n/"), "{cwd}");
}

#[test]
fn ctrl_c_ends_a_command_without_a_handler() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&[
        "run",
        "-d",
        "--name",
        "talker",
        "alpine:3.22",
        "sleep",
        "300",
    ]);
    // `logs -f` sets no signal handler: a local one ends on SIGINT, and so must the VM's.
    let mut logs = mac
        .cmd(&["logs", "-f", "talker"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    // SAFETY: a signal to the command this test started.
    unsafe { libc::kill(logs.id() as i32, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = logs.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "SIGINT did not end it");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status.code(), Some(130));
    // The VM powers off at once: nothing in it holds SIGTERM back for 5 seconds.
    mac.ok(&["rm", "-f", "talker"]);
    let before = mac.log_count("]: stopped");
    let started = Instant::now();
    mac.ok(&["vm", "stop"]);
    assert_eq!(mac.log_count("]: stopped"), before + 1);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "it took {:?}",
        started.elapsed()
    );
}

/// When the Mac's DHCP server ends the lease of the device with MAC address `mac`
/// (seconds since the epoch).
fn lease_end(mac: &str) -> Option<u64> {
    // bootpd writes each octet without its leading zero.
    let octets: Vec<String> = mac
        .trim()
        .split(':')
        .map(|o| format!("{:x}", u8::from_str_radix(o, 16).unwrap()))
        .collect();
    let hw = format!("hw_address=1,{}\n", octets.join(":"));
    let leases = std::fs::read_to_string("/var/db/dhcpd_leases").ok()?;
    leases.split('{').find(|e| e.contains(&hw)).and_then(|e| {
        e.lines()
            .find_map(|l| l.trim().strip_prefix("lease=0x"))
            .and_then(|h| u64::from_str_radix(h, 16).ok())
    })
}

#[test]
fn the_vm_renews_its_address_lease() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    let address = mac.ok(&[
        "run",
        "--rm",
        "--network",
        "host",
        "alpine:3.22",
        "cat",
        "/sys/class/net/eth0/address",
    ]);
    let before = lease_end(&address).expect("the VM's lease");
    std::thread::sleep(Duration::from_secs(3));
    // The kernel took the lease at boot and never renews it: the agent does.
    agent_request(&mac, &["__vm-renew"]);
    let after = lease_end(&address).expect("the VM's lease");
    assert!(after >= before + 3, "lease end {before} → {after}");
}

/// What `http://127.0.0.1:<port>/<path>` on the Mac answers (None: no answer).
fn mac_get(port: u16, path: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-s", "-f", "-m", "3"])
        .arg(format!("http://127.0.0.1:{port}/{path}"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Wait (30 s at most) until the Mac's port `port` answers `want` for /index.
fn wait_for_answer(port: u16, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while mac_get(port, "index").as_deref() != Some(want) {
        assert!(
            Instant::now() < deadline,
            "port {port} never answered {want:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A directory under /Users with an `index` file saying `text`, for a web server.
fn site(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    std::fs::write(dir.path().join("index"), text).unwrap();
    dir
}

#[test]
fn published_ports_reach_the_macs_localhost() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    let site = site("forwarded\n");
    let bind = format!("{}:/srv", site.path().display());
    let server = [
        "python3",
        "-m",
        "http.server",
        "8000",
        "--directory",
        "/srv",
    ];
    let mut args = vec![
        "run",
        "-d",
        "--name",
        "web",
        "-p",
        "18080:8000",
        "-v",
        &bind,
    ];
    args.push("python:3.13-alpine");
    args.extend(server);
    mac.docker_ok(&args);
    wait_for_answer(18080, "forwarded\n");
    // The container goes: so does the Mac's port.
    mac.docker_ok(&["rm", "-f", "web"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while mac_get(18080, "index").is_some() {
        assert!(Instant::now() < deadline, "port 18080 stayed open");
        std::thread::sleep(Duration::from_millis(200));
    }
    // podbag's own -p too.
    let mut args = vec![
        "run",
        "-d",
        "--name",
        "web2",
        "-p",
        "18081:8000",
        "-v",
        &bind,
    ];
    args.push("python:3.13-alpine");
    args.extend(server);
    mac.ok(&args);
    wait_for_answer(18081, "forwarded\n");
    mac.ok(&["rm", "-f", "web2"]);
}

#[test]
fn a_taken_mac_port_is_reported_and_others_work() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    // Another program has the Mac's port 18082.
    let taken = std::net::TcpListener::bind("127.0.0.1:18082").unwrap();
    let site = site("still served\n");
    let bind = format!("{}:/srv", site.path().display());
    mac.docker_ok(&[
        "run",
        "-d",
        "-p",
        "18082:8000",
        "-p",
        "18083:8000",
        "-v",
        &bind,
        "python:3.13-alpine",
        "python3",
        "-m",
        "http.server",
        "8000",
        "--directory",
        "/srv",
    ]);
    wait_for_answer(18083, "still served\n");
    let deadline = Instant::now() + Duration::from_secs(10);
    while mac.log_count("18082") == 0 {
        assert!(Instant::now() < deadline, "the taken port is not reported");
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(taken);
}

#[test]
fn many_connections_leak_nothing() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    let site = site("again\n");
    let bind = format!("{}:/srv", site.path().display());
    mac.docker_ok(&[
        "run",
        "-d",
        "-p",
        "18084:8000",
        "-v",
        &bind,
        "python:3.13-alpine",
        "python3",
        "-m",
        "http.server",
        "8000",
        "--directory",
        "/srv",
    ]);
    wait_for_answer(18084, "again\n");
    let open = || {
        let pid = mac.host_pid().expect("a host").to_string();
        let lsof = Command::new("lsof").args(["-p", &pid]).output().unwrap();
        String::from_utf8_lossy(&lsof.stdout).lines().count()
    };
    let before = open();
    for i in 0..300 {
        assert_eq!(
            mac_get(18084, "index").as_deref(),
            Some("again\n"),
            "request {i}"
        );
    }
    std::thread::sleep(Duration::from_secs(1));
    let after = open();
    assert!(
        after <= before + 20,
        "the host's descriptors: {before} → {after}"
    );
}

/// A UDP echo server on port 9000, for `python3 -c`: each datagram comes back after
/// `echo:`.
const UDP_ECHO: &str = "import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(('', 9000))
while True:
    d, a = s.recvfrom(65535)
    s.sendto(b'echo:' + d, a)
";

/// A UDP socket on the Mac that talks to `to` only.
fn udp_client(to: &str) -> std::net::UdpSocket {
    let local = if to.starts_with('[') {
        "[::1]:0"
    } else {
        "127.0.0.1:0"
    };
    let s = std::net::UdpSocket::bind(local).unwrap();
    s.connect(to).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    s
}

/// What a datagram on `s` gets back within a second.
fn udp_ask(s: &std::net::UdpSocket, data: &[u8]) -> std::io::Result<Vec<u8>> {
    s.send(data)?;
    let mut buf = vec![0u8; 65536];
    let n = s.recv(&mut buf)?;
    Ok(buf[..n].to_vec())
}

/// Wait (30 s at most) until `s` gets `echo:<data>` back for `data`.
fn wait_for_echo(s: &std::net::UdpSocket, data: &[u8]) {
    let want = [b"echo:", data].concat();
    let deadline = Instant::now() + Duration::from_secs(30);
    while udp_ask(s, data).ok().as_deref() != Some(&want[..]) {
        assert!(
            Instant::now() < deadline,
            "no echo through {:?}",
            s.peer_addr()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn published_udp_ports_reach_the_macs_localhost() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&[
        "run",
        "-d",
        "--name",
        "echo",
        "-p",
        "18085:9000/udp",
        "python:3.13-alpine",
        "python3",
        "-c",
        UDP_ECHO,
    ]);
    let one = udp_client("127.0.0.1:18085");
    wait_for_echo(&one, b"one");
    // Each client gets its own replies, on ::1 too.
    let two = udp_client("[::1]:18085");
    wait_for_echo(&two, b"two");
    assert_eq!(udp_ask(&one, b"again").unwrap(), b"echo:again");
    // A large datagram comes back whole (macOS sends 9216 bytes at most by default).
    let big = vec![b'x'; 9000];
    assert_eq!(udp_ask(&one, &big).unwrap(), [b"echo:", &big[..]].concat());
    // The container goes: so does the Mac's port (a closed port answers "refused").
    mac.ok(&["rm", "-f", "echo"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match udp_ask(&one, b"anyone?") {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => break,
            got => assert!(Instant::now() < deadline, "port 18085 stayed open: {got:?}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_taken_mac_udp_port_is_reported_and_others_work() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // Another program has the Mac's UDP port 18088.
    let taken = std::net::UdpSocket::bind("127.0.0.1:18088").unwrap();
    mac.ok(&[
        "run",
        "-d",
        "-p",
        "18088:9000/udp",
        "-p",
        "18089:9000/udp",
        "python:3.13-alpine",
        "python3",
        "-c",
        UDP_ECHO,
    ]);
    wait_for_echo(&udp_client("127.0.0.1:18089"), b"still served");
    let deadline = Instant::now() + Duration::from_secs(10);
    while mac.log_count("port 18088/udp") == 0 {
        assert!(Instant::now() < deadline, "the taken port is not reported");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(mac.log_count("another program on this Mac uses it"), 1);
    drop(taken);
}

#[test]
fn a_low_port_reaches_the_mac_and_stays_off_its_network() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    let site = site("low\n");
    let bind = format!("{}:/srv", site.path().display());
    // Below 1024: macOS gives 127.0.0.1:<port> only to root.
    mac.docker_ok(&[
        "run",
        "-d",
        "-p",
        "1007:8000",
        "-v",
        &bind,
        "python:3.13-alpine",
        "python3",
        "-m",
        "http.server",
        "8000",
        "--directory",
        "/srv",
    ]);
    wait_for_answer(1007, "low\n");
    // Not from the Mac's network: a connection to its own network address gets nothing.
    let lan = Command::new("ipconfig")
        .args(["getifaddr", "en0"])
        .output()
        .unwrap();
    let lan = String::from_utf8_lossy(&lan.stdout).trim().to_string();
    if !lan.is_empty() {
        let got = Command::new("curl")
            .args(["-s", "-f", "-m", "3"])
            .arg(format!("http://{lan}:1007/index"))
            .output()
            .unwrap();
        assert!(
            !got.status.success(),
            "{}",
            String::from_utf8_lossy(&got.stdout)
        );
    }
}

#[test]
fn many_connections_at_once_through_a_port() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    // The host starts with macOS's usual limit of 256 open files.
    let mut first = mac.cmd(&["ps"]);
    // SAFETY: getrlimit and setrlimit between fork and exec, on plain data.
    unsafe {
        use std::os::unix::process::CommandExt;
        first.pre_exec(|| {
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
            lim.rlim_cur = 256;
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
            Ok(())
        });
    }
    assert!(
        first
            .stdin(Stdio::null())
            .output()
            .unwrap()
            .status
            .success()
    );
    let site = site("crowd\n");
    let bind = format!("{}:/srv", site.path().display());
    mac.docker_ok(&[
        "run",
        "-d",
        "-p",
        "18086:8000",
        "-v",
        &bind,
        "python:3.13-alpine",
        "python3",
        "-m",
        "http.server",
        "8000",
        "--directory",
        "/srv",
    ]);
    wait_for_answer(18086, "crowd\n");
    // 100 connections that stay open (a client's pool), then 20 that ask for a page.
    use std::io::{Read, Write};
    let pool: Vec<std::net::TcpStream> = (0..100)
        .map(|_| std::net::TcpStream::connect("127.0.0.1:18086").unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    for i in 0..20 {
        let mut c = std::net::TcpStream::connect("127.0.0.1:18086").unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        c.write_all(b"GET /index HTTP/1.0\r\n\r\n").unwrap();
        let mut reply = String::new();
        let _ = c.read_to_string(&mut reply);
        assert!(
            reply.ends_with("crowd\n"),
            "connection {i} beside the pool: {reply:?}"
        );
    }
    drop(pool);
}

#[test]
fn published_ports_answer_on_ipv6_localhost_too() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    let site = site("six\n");
    let bind = format!("{}:/srv", site.path().display());
    mac.docker_ok(&[
        "run",
        "-d",
        "-p",
        "18087:8000",
        "-v",
        &bind,
        "python:3.13-alpine",
        "python3",
        "-m",
        "http.server",
        "8000",
        "--directory",
        "/srv",
    ]);
    wait_for_answer(18087, "six\n");
    // macOS resolves localhost to ::1 first; some clients (Node 18) do not fall back.
    let got = Command::new("curl")
        .args(["-s", "-f", "-m", "3", "-6", "http://[::1]:18087/index"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&got.stdout), "six\n");
}

#[test]
fn disk_space_goes_back_without_a_restart() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    // What the Mac has given the sparse disk file, in MiB.
    let allocated = || {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(mac.home.path().join("disk.img"))
            .unwrap()
            .blocks()
            * 512
            >> 20
    };
    let base = allocated();
    let back_within = |what: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while allocated() > base + 100 {
            assert!(
                Instant::now() < deadline,
                "{what}: {} MiB, was {base}",
                allocated()
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    };
    // A container's 500 MB, gone with it.
    mac.ok(&[
        "run",
        "--rm",
        "alpine:3.22",
        "sh",
        "-c",
        "dd if=/dev/urandom of=/big bs=1M count=500 2>/dev/null && sync",
    ]);
    assert!(allocated() > base + 400, "written: {} MiB", allocated());
    back_within("after the container");
    // A volume's 300 MB, gone with it.
    mac.ok(&[
        "run",
        "--rm",
        "-v",
        "scratch:/s",
        "alpine:3.22",
        "sh",
        "-c",
        "dd if=/dev/urandom of=/s/big bs=1M count=300 2>/dev/null && sync",
    ]);
    mac.ok(&["volume", "rm", "scratch"]);
    back_within("after the volume");
    // An image's layers, gone with it (a seed image: no pull).
    let held = allocated();
    mac.ok(&["rmi", "mirror.gcr.io/moby/buildkit:buildx-stable-1"]);
    let deadline = Instant::now() + Duration::from_secs(20);
    while allocated() + 30 > held {
        assert!(
            Instant::now() < deadline,
            "after rmi: {} MiB, was {held}",
            allocated()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn a_container_bigger_than_the_vm_is_killed() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    // 64 MiB at a time, every page touched, up to 2 GiB: twice the tests' 1 GiB VM.
    let grow = "chunks = []\n\
                for i in range(32):\n    \
                    b = bytearray(64 << 20)\n    \
                    b[::4096] = b'x' * len(b[::4096])\n    \
                    chunks.append(b)\n\
                print('all of it fit')";
    // From podbag's CLI and from the docker CLI (the agent's Docker API).
    for mut command in [
        mac.cmd(&["run", "--rm", "python:3.13-alpine", "python3", "-c", grow]),
        mac.docker(&["run", "--rm", "python:3.13-alpine", "python3", "-c", grow]),
    ] {
        killed_within_a_minute(&mac, &mut command);
    }
}

/// Runs a container that wants more memory than the VM has: it must end with 137 (an OOM
/// kill) within 60 s, and the VM must keep answering.
fn killed_within_a_minute(mac: &Mac, command: &mut Command) {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            // Dropping the Mac stops the VM, busy or not.
            panic!("neither killed nor done within 60 s: the VM thrashes");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        status.code(),
        Some(137),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The VM keeps answering.
    let asked = Instant::now();
    mac.ok(&["ps"]);
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "ps took {:?}",
        asked.elapsed()
    );
}

#[test]
fn a_standby_wakes_without_waiting() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(2);
    mac.ok(&["ps"]);
    mac.wait_until_stopped();
    // The host that started the standby is long gone by the time a client comes.
    std::thread::sleep(Duration::from_secs(1));
    let asked = Instant::now();
    mac.ok(&["ps"]);
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "woke in {:?}",
        asked.elapsed()
    );
}

#[test]
fn a_container_runs_in_milliseconds_through_the_docker_api() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    mac.ok(&["ps"]);
    let best = (0..6)
        .map(|_| api_run_rm(&mac.docker_sock()))
        .skip(1)
        .min()
        .unwrap();
    assert!(best < Duration::from_millis(12), "best of 5: {best:?}");
}

/// `docker run --rm --network none alpine:3.22 true` as the docker CLI does it, without the
/// CLI's own start: create, attach, wait for the removal, start. Returns how long it took.
fn api_run_rm(sock: &Path) -> Duration {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    fn request(sock: &Path, method: &str, path: &str, body: &str) -> String {
        let mut s = UnixStream::connect(sock).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }
    fn head(s: &mut UnixStream) {
        let mut h = Vec::new();
        let mut b = [0u8; 1];
        while !h.ends_with(b"\r\n\r\n") {
            assert_eq!(s.read(&mut b).unwrap(), 1, "{h:?}");
            h.push(b[0]);
        }
    }
    let started = Instant::now();
    let created = request(
        sock,
        "POST",
        "/containers/create",
        r#"{"Image":"alpine:3.22","Cmd":["true"],"HostConfig":{"NetworkMode":"none","AutoRemove":true}}"#,
    );
    let id = created
        .split("\"Id\":\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    let mut attach = UnixStream::connect(sock).unwrap();
    write!(
        attach,
        "POST /containers/{id}/attach?stream=1&stdout=1&stderr=1 HTTP/1.1\r\nHost: d\r\n\
         Upgrade: tcp\r\nConnection: Upgrade\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    head(&mut attach);
    let mut wait = UnixStream::connect(sock).unwrap();
    write!(
        wait,
        "POST /containers/{id}/wait?condition=removed HTTP/1.1\r\nHost: d\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    head(&mut wait);
    let start = request(sock, "POST", &format!("/containers/{id}/start"), "");
    assert!(start.starts_with("HTTP/1.1 204"), "{start}");
    let mut rest = String::new();
    wait.read_to_string(&mut rest).unwrap();
    assert!(rest.contains("\"StatusCode\":0"), "{rest}");
    started.elapsed()
}

#[test]
fn a_big_vm_keeps_no_unused_bounce_buffers() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // The first command starts the VM: 8 GiB, the others' size in docs/benchmarks.md.
    let out = mac
        .cmd(&["run", "--rm", "--privileged", "alpine:3.22", "dmesg"])
        .env("PODBAG_VM_MEMORY", "8192")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = String::from_utf8_lossy(&out.stdout);
    let line = |what: &str| {
        log.lines()
            .find(|l| l.contains(what))
            .unwrap_or_else(|| panic!("no {what:?} in dmesg"))
            .to_string()
    };
    // Bounce buffers nothing uses (virtio here bypasses the DMA API): a few MB, not 64.
    let tlb = line("software IO TLB: mapped");
    let mb: u64 = tlb
        .rsplit('(')
        .next()
        .and_then(|s| s.strip_suffix("MB)"))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("{tlb}"));
    assert!(mb <= 8, "{tlb}");
}

#[test]
fn vm_stop_after_a_failed_start_leaves_nothing_running() {
    if !enabled() || !have_docker() {
        return;
    }
    let mac = Mac::new(2);
    let disk = mac.home.path().join("disk.img");
    for attempt in 1..=4 {
        // A good disk: a VM runs and goes, and its standby waits.
        let _ = std::fs::remove_file(&disk);
        assert!(
            Command::new("cp")
                .arg("-c")
                .arg(seed_disk())
                .arg(&disk)
                .status()
                .unwrap()
                .success()
        );
        mac.ok(&["ps"]);
        mac.wait_until_stopped();
        // The disk breaks: the standby's host fails, answers, and starts another standby.
        use std::os::unix::fs::FileExt;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&disk)
            .unwrap()
            .write_all_at(&[0u8; 4096], 1024)
            .unwrap();
        let out = mac.docker(&["ps"]).output().unwrap();
        assert_ne!(out.status.code(), Some(0), "attempt {attempt}");
        mac.ok(&["vm", "stop"]);
        if let Some(pid) = mac.host_pid() {
            let run = mac.home.path().join("run");
            let read = |f: &str| std::fs::read_to_string(run.join(f)).unwrap_or_default();
            let ps = Command::new("ps")
                .args(["-o", "pid,ppid,lstart,command", "-p", &pid.to_string()])
                .output()
                .unwrap();
            panic!(
                "attempt {attempt}: a host or standby still runs after vm stop:\n{}\
                 pid file: {:?}, state: {:?}\nhost.log:\n{}",
                String::from_utf8_lossy(&ps.stdout),
                read("host.pid"),
                read("state"),
                read("host.log")
            );
        }
    }
}

/// `podbag vm status --json`.
fn status_json(mac: &Mac) -> serde_json::Value {
    serde_json::from_str(&mac.ok(&["vm", "status", "--json"])).unwrap()
}

/// The CPUs and the memory (MiB) a container sees in the VM.
fn guest_size(cmd: impl Fn(&[&str]) -> Command) -> (u32, u64) {
    let out = cmd(&[
        "run",
        "--rm",
        "alpine:3.22",
        "sh",
        "-c",
        "nproc; grep MemTotal /proc/meminfo",
    ])
    .stdin(Stdio::null())
    .output()
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines = text.lines();
    let cpus = lines.next().unwrap().trim().parse().unwrap();
    let kib: u64 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (cpus, kib / 1024)
}

#[test]
fn vm_settings_apply_at_the_next_start_and_at_a_restart() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    // The harness's own settings would win over the saved ones.
    let cmd = |args: &[&str]| {
        let mut c = mac.cmd(args);
        c.env_remove("PODBAG_VM_MEMORY")
            .env_remove("PODBAG_VM_CPUS")
            .env_remove("PODBAG_VM_IDLE");
        c
    };
    let ok = |args: &[&str]| {
        let out = cmd(args).stdin(Stdio::null()).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "podbag {args:?}: {text}");
        text
    };
    let status =
        || -> serde_json::Value { serde_json::from_str(&ok(&["vm", "status", "--json"])).unwrap() };
    // Checked before saving.
    let bad = cmd(&["vm", "set", "--memory", "512M"]).output().unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--memory must be from 1 GiB"));
    let bad = cmd(&["vm", "set", "--cpus", "0"]).output().unwrap();
    assert!(!bad.status.success());
    assert!(!mac.home.path().join("vm.json").exists());
    let said = ok(&[
        "vm", "set", "--cpus", "2", "--memory", "1536M", "--idle", "5m",
    ]);
    assert!(
        said.contains("Saved: 2 CPUs, 1536 MiB memory, power off when idle: after 5m"),
        "{said}"
    );
    assert!(said.contains("next start"), "{said}");
    let (cpus, mib) = guest_size(cmd);
    assert_eq!(cpus, 2);
    assert!((1300..=1536).contains(&mib), "MemTotal {mib} MiB");
    let s = status();
    assert_eq!(s["state"], "running");
    assert_eq!(s["inUse"]["cpus"], 2);
    assert_eq!(s["inUse"]["memoryMiB"], 1536);
    assert_eq!(s["inUse"]["idleSeconds"], 300, "the saved idle time ({s})");
    assert_eq!(s["pending"], false);
    // New values while it runs: pending until a restart.
    let said = ok(&["vm", "set", "--cpus", "1", "--memory", "1G"]);
    assert!(said.contains("podbag vm restart"), "{said}");
    assert_eq!(status()["pending"], true);
    assert_eq!(guest_size(cmd).0, 2);
    ok(&["vm", "set", "--cpus", "1", "--restart", "--yes"]);
    let (cpus, mib) = guest_size(cmd);
    assert_eq!(cpus, 1);
    assert!((800..=1024).contains(&mib), "MemTotal {mib} MiB");
    assert_eq!(status()["pending"], false);
}

#[test]
fn vm_status_json_start_stop_and_restart() {
    if !enabled() {
        return;
    }
    let mac = Mac::new(60);
    let s = status_json(&mac);
    assert_eq!(s["platform"], "macos");
    assert_eq!(s["state"], "stopped");
    assert!(s["inUse"].is_null() && s["uptimeSeconds"].is_null() && s["containers"].is_null());
    assert_eq!(
        s["next"]["memoryMiB"], 1024,
        "the harness's PODBAG_VM_MEMORY"
    );
    assert_eq!(
        s["overriddenBy"],
        serde_json::json!(["PODBAG_VM_MEMORY", "PODBAG_VM_IDLE"])
    );
    assert!(s["limits"]["cpusMax"].as_u64().unwrap() >= 1);
    assert!(s["limits"]["memoryMaxMiB"].as_u64().unwrap() >= 1024);
    // The disk is a sparse file: far less on the Mac than its size.
    let disk = &s["disk"];
    assert_eq!(disk["sizeBytes"], 64u64 << 30);
    assert!(
        disk["allocatedBytes"].as_u64().unwrap() < 8u64 << 30,
        "{disk}"
    );
    assert_eq!(mac.ok(&["vm", "start"]), "running");
    mac.ok(&["run", "-d", "--name", "bg", "alpine:3.22", "sleep", "300"]);
    // The host counts the containers and reads the guest's use every few seconds.
    let deadline = Instant::now() + Duration::from_secs(15);
    let s = loop {
        let s = status_json(&mac);
        if s["containers"] == 1 && s["cpuPercent"].is_number() {
            break s;
        }
        assert!(Instant::now() < deadline, "{s}");
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(s["state"], "running");
    assert!(s["uptimeSeconds"].as_u64().is_some());
    let total = s["memoryTotalMiB"].as_u64().unwrap();
    assert!((800..=1024).contains(&total), "{s}");
    assert!(s["memoryUsedMiB"].as_u64().unwrap() < total);
    // A restart stops the containers: without a terminal, only with --yes.
    let refused = mac.run(&["vm", "restart"]);
    assert_eq!(refused.status.code(), Some(1));
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(
        err.contains("1 container runs") && err.contains("--yes"),
        "{err}"
    );
    assert!(mac.ok(&["ps"]).contains("bg"));
    let before = s["uptimeSeconds"].as_u64().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(mac.ok(&["vm", "restart", "--yes"]), "running");
    let s = status_json(&mac);
    assert!(s["uptimeSeconds"].as_u64().unwrap() <= before, "{s}");
    let ps = mac.ok(&["ps", "-a"]);
    assert!(ps.contains("bg") && !ps.contains("Up "), "{ps}");
    mac.ok(&["vm", "stop"]);
    let s = status_json(&mac);
    assert_eq!(s["state"], "stopped");
    assert!(s["containers"].is_null());
}
