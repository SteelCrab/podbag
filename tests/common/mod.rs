#![allow(dead_code)]

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

impl Sandbox {
    pub fn new() -> Sandbox {
        let sb = Sandbox {
            tmp: tempfile::tempdir().unwrap(),
            env: Vec::new(),
        };
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

    /// `openpod run --rootfs <rootfs()> <args...>`
    pub fn run(&self, args: &[&str]) -> Output {
        self.openpod()
            .args(["run", "--rootfs", &rootfs()])
            .args(args)
            .output()
            .unwrap()
    }

    /// `openpod run --rootfs <rootfs()> <options...> <command...>`
    pub fn run_with(&self, options: &[&str], command: &[&str]) -> Output {
        self.openpod()
            .args(["run", "--rootfs", &rootfs()])
            .args(options)
            .args(command)
            .output()
            .unwrap()
    }
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}
