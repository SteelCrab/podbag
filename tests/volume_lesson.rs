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
