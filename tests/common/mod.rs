#![allow(dead_code)]

pub mod api;
pub mod registry;
use std::process::{Command, Output};

pub const BIN: &str = env!("CARGO_BIN_EXE_podbag");

/// The unpacked Alpine root filesystem. Fails with instructions when it is not set.
pub fn rootfs() -> String {
    std::env::var("PODBAG_TEST_ROOTFS")
        .expect("PODBAG_TEST_ROOTFS is not set; run these tests with `dev/vm.sh test`")
}

/// True when the tests run podbag as root (`dev/vm.sh test-root`).
/// As root, every sandbox's user networks live in the host's one namespace, and separate
/// data directories pick the same automatic subnets: tests that create networks take turns.
pub fn network_turn() -> Option<std::fs::File> {
    if !root_mode() {
        return None;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open("/tmp/podbag-test-networks.lock")
        .unwrap();
    lock.lock().unwrap();
    Some(lock)
}

/// Hold container `c`'s shim lock for `secs`, as a start or a cleanup does; returns the
/// holder once it holds the lock.
pub fn hold_shim_lock(sb: &Sandbox, c: &str, secs: &str) -> std::process::Child {
    let lock = sb.data().join("containers").join(c).join("shim.lock");
    let mut cmd = if root_mode() {
        let mut cmd = std::process::Command::new("sudo");
        cmd.args(["-n", "flock"]);
        cmd
    } else {
        std::process::Command::new("flock")
    };
    let holder = cmd.arg(&lock).args(["sleep", secs]).spawn().unwrap();
    // flock(1) has the lock once a try from here fails.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mut probe = if root_mode() {
            let mut p = std::process::Command::new("sudo");
            p.args(["-n", "flock", "-n"]);
            p
        } else {
            let mut p = std::process::Command::new("flock");
            p.arg("-n");
            p
        };
        let free = probe.arg(&lock).arg("true").status().unwrap().success();
        if !free {
            return holder;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the lock was never taken"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Write the mark that the user stopped container `c`.
pub fn mark_stopped(sb: &Sandbox, c: &str) {
    let mark = sb.data().join("containers").join(c).join("stopped");
    if root_mode() {
        assert!(
            std::process::Command::new("sudo")
                .args(["-n", "touch"])
                .arg(&mark)
                .status()
                .unwrap()
                .success()
        );
    } else {
        std::fs::write(mark, "").unwrap();
    }
}

pub fn root_mode() -> bool {
    std::env::var("PODBAG_TEST_ROOT").as_deref() == Ok("1")
}

/// Separate data and runtime directories for one test.
pub struct Sandbox {
    pub tmp: tempfile::TempDir,
    /// Extra environment for podbag; passed through sudo in root mode.
    pub env: Vec<(String, String)>,
}

// Remove what a test leaves: its containers, and (as root) files the test user cannot
// delete, through sudo, so the VM's /tmp (a small tmpfs) does not fill up over many runs.
impl Drop for Sandbox {
    fn drop(&mut self) {
        // A test that failed half-way may leave containers running (and holding ports).
        if let Ok(out) = self.podbag().args(["ps", "-aq"]).output() {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(String::from)
                .collect();
            if !ids.is_empty() {
                let _ = self.podbag().args(["rm", "-f"]).args(&ids).output();
            }
        }
        // The sandbox's keeper, if one still lingers: nothing of a test stays behind.
        let keeper = format!("__keeper {}$", self.runtime().display());
        let mut pkill = if root_mode() {
            let mut c = Command::new("sudo");
            c.args(["-n", "pkill"]);
            c
        } else {
            Command::new("pkill")
        };
        let _ = pkill
            .args(["-TERM", "-f", &keeper])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if root_mode() {
            // The bridges of this sandbox's networks (its default network has a subnet
            // and so a bridge of its own), with their firewall chains: root's live in the
            // host's namespace and would pile up there over the runs. A bridge that still
            // has containers on it belongs to another sandbox too (networks of different
            // sandboxes can get the same subnet, and so the same bridge).
            let _ = Command::new("sudo")
                .args([
                    "-n",
                    "sh",
                    "-c",
                    r#"for f in "$1"/networks/*.json; do
                         b=$(sed -n 's/.*"bridge": *"\(pbg-[^"]*\)".*/\1/p' "$f" 2>/dev/null)
                         [ -n "$b" ] || continue
                         # The ports of containers that were just removed go a moment later.
                         n=0
                         while [ -n "$(ls "/sys/class/net/$b/brif" 2>/dev/null)" ] && [ $n -lt 20 ]; do
                           sleep 0.05; n=$((n+1))
                         done
                         [ -z "$(ls "/sys/class/net/$b/brif" 2>/dev/null)" ] || continue
                         ip link del "$b" 2>/dev/null
                         for c in "post-$b" "fwd-$b"; do
                           nft flush chain ip podbag "$c" 2>/dev/null
                           nft delete chain ip podbag "$c" 2>/dev/null
                         done
                       done"#,
                    "_",
                ])
                .arg(self.data())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            // Shared image mounts (composefs) stay until their image is removed; take
            // down every mount below this sandbox, deepest first, before deleting it.
            for point in mounts_under(self.tmp.path()).into_iter().rev() {
                let _ = Command::new("sudo")
                    .args(["-n", "umount", "-l"])
                    .arg(&point)
                    .status();
            }
            let _ = Command::new("sudo")
                .args(["-n", "rm", "-rf"])
                .arg(self.tmp.path())
                .status();
        }
    }
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let mut sb = Sandbox {
            tmp: tempfile::tempdir().unwrap(),
            env: Vec::new(),
        };
        // As root, the bridges are in the host's network namespace, where the default
        // subnet's bridge is one device for all: each sandbox gets a subnet of its own for
        // its default network, and with it a bridge of its own (named after the subnet).
        if root_mode() {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = (std::process::id() + n) % 250 + 1;
            sb.env.push((
                "PODBAG_DEFAULT_SUBNET".to_string(),
                format!("10.79.{slot}.0/24"),
            ));
        }
        // The keeper of a sandbox leaves soon after its last container (a later value
        // of the same variable wins, for tests that want another time).
        sb.env
            .push(("PODBAG_KEEPER_LINGER_MS".to_string(), "300".to_string()));
        // PODBAG_TEST_KEEPER=0 runs the suite with a shim per container.
        if std::env::var("PODBAG_TEST_KEEPER").as_deref() == Ok("0") {
            sb.env.push(("PODBAG_KEEPER".to_string(), "0".to_string()));
        }
        // Created by the test user, so the test can list it even when podbag runs as root.
        std::fs::create_dir_all(sb.data().join("containers")).unwrap();
        sb
    }

    pub fn with_env(key: &str, value: &str) -> Sandbox {
        let mut sb = Sandbox::new();
        sb.env.push((key.to_string(), value.to_string()));
        sb
    }

    pub fn data(&self) -> std::path::PathBuf {
        self.tmp.path().join("data")
    }

    pub fn runtime(&self) -> std::path::PathBuf {
        self.tmp.path().join("run")
    }

    /// rootless: run podbag directly. root: run it through `sudo -n env ...`.
    pub fn podbag(&self) -> Command {
        self.podbag_bin(BIN)
    }

    pub fn podbag_bin(&self, bin: &str) -> Command {
        let mut vars = vec![
            (
                "PODBAG_DATA_DIR".to_string(),
                self.data().display().to_string(),
            ),
            (
                "PODBAG_RUNTIME_DIR".to_string(),
                self.runtime().display().to_string(),
            ),
        ];
        vars.extend(self.env.iter().cloned());
        if root_mode() {
            let mut cmd = Command::new("sudo");
            cmd.args(["-n", "env"])
                .args(vars.iter().map(|(k, v)| format!("{k}={v}")))
                .arg(bin);
            cmd
        } else {
            let mut cmd = Command::new(bin);
            cmd.envs(vars);
            cmd
        }
    }

    /// `podbag run --rm --rootfs <rootfs()> <args...>`
    pub fn run(&self, args: &[&str]) -> Output {
        self.podbag()
            .args(["run", "--rm", "--rootfs", &rootfs()])
            .args(args)
            .output()
            .unwrap()
    }

    /// `podbag run --rm --rootfs <rootfs()> <options...> <command...>`
    pub fn run_with(&self, options: &[&str], command: &[&str]) -> Output {
        self.podbag()
            .args(["run", "--rm", "--rootfs", &rootfs()])
            .args(options)
            .args(command)
            .output()
            .unwrap()
    }
}

/// Mount points at or below `dir` (sorted, so parents come first), with their types.
pub fn mounts_with_types(dir: &std::path::Path) -> Vec<(String, String)> {
    let prefix = dir.to_string_lossy().into_owned();
    let mut out: Vec<(String, String)> = std::fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let point = f.nth(1)?.to_string();
            let fstype = f.next()?.to_string();
            point.starts_with(&prefix).then_some((point, fstype))
        })
        .collect();
    out.sort();
    out
}

/// Mount points at or below `dir`, parents first.
pub fn mounts_under(dir: &std::path::Path) -> Vec<String> {
    mounts_with_types(dir).into_iter().map(|(p, _)| p).collect()
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}
