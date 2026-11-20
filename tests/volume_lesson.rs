#![cfg(unix)]
use podbag::client::{Client, Endpoint, Kind};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[test]
fn a_practice_terminal_and_tui_share_only_their_own_volumes() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let scratch = tempfile::tempdir().unwrap();
    let start = || {
        let mut owner = Command::new(env!("CARGO_BIN_EXE_podbag"))
            .args(["lab", "workspace"])
            .env("PODBAG_UI_DIR", scratch.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(owner.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let path = PathBuf::from(line.trim_end());
        assert!(
            path.is_absolute()
                && path
                    .join("api.sock")
                    .metadata()
                    .unwrap()
                    .file_type()
                    .is_socket()
        );
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o077, 0);
        (owner, path)
    };
    let (mut first, a) = start();
    let (mut second, b) = start();
    let cli = |root: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_podbag"))
            .args(args)
            .env(podbag::volume_lesson::SCOPE, root)
            .env("PODBAG_UI_DIR", scratch.path())
            .env("PODBAG_DATA_DIR", root.join("data"))
            .env("PODBAG_RUNTIME_DIR", root.join("run"))
            .env("PODBAG_CONFIG_DIR", root.join("config"))
            .output()
            .unwrap()
    };
    for path in [&a, &b] {
        assert!(cli(path, &["volume", "create", "my-api"]).status.success());
    }
    let client = Client::new(Endpoint::Socket {
        path: a.join("api.sock"),
        own: false,
    });
    assert_eq!(
        client.inspect(Kind::Volumes, "my-api").unwrap()["Name"],
        "my-api"
    );
    assert!(cli(&a, &["volume", "rm", "my-api"]).status.success());
    assert!(cli(&a, &["volume", "ls"]).status.success());
    assert!(
        !a.join("verified").exists(),
        "CLI deletion skips the TUI task"
    );
    assert!(cli(&a, &["volume", "create", "my-api"]).status.success());
    assert!(
        !cli(&a, &["tui", "--engine", "docker", "--once"])
            .status
            .success()
    );
    client.remove(Kind::Volumes, "my-api", false).unwrap();
    assert!(cli(&a, &["volume", "ls"]).status.success());
    assert!(a.join("verified").is_file());
    assert!(cli(&b, &["volume", "inspect", "my-api"]).status.success());
    drop(first.stdin.take());
    assert!(first.wait().unwrap().success());
    assert!(!a.exists());
    assert!(b.is_dir());
    drop(second.stdin.take());
    assert!(second.wait().unwrap().success());
    assert!(!b.exists());
}

#[test]
fn desktop_observes_cli_creation_and_requires_inspection_before_deletion() {
    use podbag::volume_lesson::gui::{Registration, view};
    let library = tempfile::tempdir().unwrap();
    let library = library.path();
    let mut engine = Command::new(env!("CARGO_BIN_EXE_podbag"))
        .args(["lab", "workspace"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(engine.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let root = Path::new(line.trim_end());
    let session = Registration::open(library, root).unwrap();
    assert!(Registration::open(library, root).is_err());
    let snapshot = |action, session| view(library, action, session, "my-api");
    let first = snapshot("status", None).unwrap().unwrap();
    assert_eq!(first.stage, "create");
    let id = Some(first.session.as_str());
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_podbag"))
            .args(args)
            .env(podbag::volume_lesson::SCOPE, root)
            .env("PODBAG_DATA_DIR", root.join("data"))
            .env("PODBAG_RUNTIME_DIR", root.join("run"))
            .env("PODBAG_CONFIG_DIR", root.join("config"))
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(cli(&["volume", "create", "my-api"]));
    assert_eq!(snapshot("status", None).unwrap().unwrap().stage, "inspect");
    assert!(snapshot("remove", id).is_err());
    assert!(snapshot("inspect", Some("another-session")).is_err());
    assert_eq!(
        snapshot("inspect", id).unwrap().unwrap().detail.unwrap()["Name"],
        "my-api"
    );
    assert_eq!(snapshot("status", None).unwrap().unwrap().stage, "remove");
    assert_eq!(snapshot("remove", id).unwrap().unwrap().stage, "verify");
    assert!(cli(&["volume", "ls"]));
    assert_eq!(snapshot("status", None).unwrap().unwrap().stage, "finish");
    drop(engine.stdin.take());
    assert!(engine.wait().unwrap().success());
    drop(session);
    assert!(snapshot("status", None).unwrap().is_none());
    assert!(snapshot("remove", id).is_err());
}
