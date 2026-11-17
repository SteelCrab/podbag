use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_openpod");

#[test]
fn version_prints_package_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("openpod {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_argument_exits_2() {
    let out = Command::new(BIN).arg("no-such-command").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

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
