#![allow(dead_code)]

pub mod api;
pub mod registry;
use std::process::{Command, Output};

pub const BIN: &str = env!("CARGO_BIN_EXE_openpod");

/// The unpacked Alpine root filesystem. Fails with instructions when it is not set.
pub fn rootfs() -> String {
    std::env::var("OPENPOD_TEST_ROOTFS")
        .expect("OPENPOD_TEST_ROOTFS is not set; run these tests with `dev/vm.sh test`")
}

/// True when the tests run openpod as root (`dev/vm.sh test-root`).
pub fn root_mode() -> bool {
    std::env::var("OPENPOD_TEST_ROOT").as_deref() == Ok("1")
}

/// Separate data and runtime directories for one test.
pub struct Sandbox {
    pub tmp: tempfile::TempDir,
    /// Extra environment for openpod; passed through sudo in root mode.
    pub env: Vec<(String, String)>,
}

// Remove what a test leaves: its containers, and (as root) files the test user cannot
// delete, through sudo, so the VM's /tmp (a small tmpfs) does not fill up over many runs.
impl Drop for Sandbox {
    fn drop(&mut self) {
        // A test that failed half-way may leave containers running (and holding ports).
        if let Ok(out) = self.openpod().args(["ps", "-aq"]).output() {
            let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(String::from)
                .collect();
            if !ids.is_empty() {
                let _ = self.openpod().args(["rm", "-f"]).args(&ids).output();
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
        // As root, every sandbox's default network uses the host's one bridge device; its
        // own subnet keeps the sandboxes' address tables from handing out the same address.
        if root_mode() {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = (std::process::id() + n) % 250 + 1;
            sb.env.push((
                "OPENPOD_DEFAULT_SUBNET".to_string(),
                format!("10.79.{slot}.0/24"),
            ));
        }
        // The keeper of a sandbox leaves soon after its last container (a later value
        // of the same variable wins, for tests that want another time).
        sb.env
            .push(("OPENPOD_KEEPER_LINGER_MS".to_string(), "300".to_string()));
        // OPENPOD_TEST_KEEPER=0 runs the suite with a shim per container.
        if std::env::var("OPENPOD_TEST_KEEPER").as_deref() == Ok("0") {
            sb.env.push(("OPENPOD_KEEPER".to_string(), "0".to_string()));
        }
        // Created by the test user, so the test can list it even when openpod runs as root.
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

    /// rootless: run openpod directly. root: run it through `sudo -n env ...`.
    pub fn openpod(&self) -> Command {
        self.openpod_bin(BIN)
    }

    pub fn openpod_bin(&self, bin: &str) -> Command {
        let mut vars = vec![
            (
                "OPENPOD_DATA_DIR".to_string(),
                self.data().display().to_string(),
            ),
            (
                "OPENPOD_RUNTIME_DIR".to_string(),
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

    /// `openpod run --rm --rootfs <rootfs()> <args...>`
    pub fn run(&self, args: &[&str]) -> Output {
        self.openpod()
            .args(["run", "--rm", "--rootfs", &rootfs()])
            .args(args)
            .output()
            .unwrap()
    }

    /// `openpod run --rm --rootfs <rootfs()> <options...> <command...>`
    pub fn run_with(&self, options: &[&str], command: &[&str]) -> Output {
        self.openpod()
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
