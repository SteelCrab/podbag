use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_podbag");

#[test]
fn version_prints_package_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("podbag {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_argument_exits_2() {
    let out = Command::new(BIN).arg("no-such-command").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

// On macOS the command runs in the VM (tests/macos_vm.rs covers the forwarding).
#[cfg(not(target_os = "macos"))]
#[test]
fn prune_requires_force() {
    let out = Command::new(BIN)
        .args(["system", "prune"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(125));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("add -f to confirm"), "{err}");
}

#[test]
fn image_commands_exist() {
    for args in [
        &["pull", "--help"][..],
        &["images", "--help"],
        &["rmi", "--help"],
        &["system", "df", "--help"],
    ] {
        let out = Command::new(BIN).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn dashboard_commands_are_visible_and_print_help_without_an_engine() {
    let commands = ["engine", "import", "kube", "settings", "tutorial"];
    let help = Command::new(BIN).arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    for command in commands {
        assert!(
            help.lines()
                .any(|line| line.split_whitespace().next() == Some(command))
        );
    }
    for args in [
        ["engine", "prune", "--help"],
        ["import", "images", "--help"],
        ["kube", "migrate", "--help"],
        ["settings", "startup", "--help"],
        ["tutorial", "session", "--help"],
    ] {
        let out = Command::new(BIN).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        assert!(
            help.lines()
                .any(|line| line.split_whitespace().next() == Some("vm"))
        );
        assert!(
            Command::new(BIN)
                .args(["vm", "--help"])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

// podbag as the first process of a PID namespace (a container's entry point, or
// `unshare -pf`) is a CLI like any other: only the macOS VM's /init is an init.
//
// It runs in a sandbox, so a regression cannot reach this machine: a root of its own
// (the binary and its libraries), new mount, UTS, IPC and network namespaces, and a
// KILL for the whole PID namespace when the time is up.
#[cfg(target_os = "linux")]
#[test]
fn pid_one_runs_the_cli() {
    if Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .map_or(true, |s| !s.success())
    {
        eprintln!("no sudo: skipped");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let place = |from: &std::path::Path, to: &std::path::Path| {
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(from, to).unwrap();
    };
    place(std::path::Path::new(BIN), &root.path().join("podbag"));
    let ldd = Command::new("ldd").arg(BIN).output().unwrap();
    for lib in String::from_utf8_lossy(&ldd.stdout)
        .split_whitespace()
        .filter(|w| w.starts_with('/'))
    {
        let lib = std::path::Path::new(lib);
        place(lib, &root.path().join(lib.strip_prefix("/").unwrap()));
    }
    let out = Command::new("sudo")
        .args([
            "-n", "timeout", "-s", "KILL", "20", "unshare", "--pid", "--fork",
        ])
        .args([
            "--kill-child",
            "--mount",
            "--uts",
            "--ipc",
            "--net",
            "--root",
        ])
        .arg(root.path())
        .args(["/podbag", "--version"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("podbag"));
}
