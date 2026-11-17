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
