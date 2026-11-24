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

/// The recovery Training from start to finish against a real practice engine: a server
/// that exits for a missing setting, its log, and the fixed server's page. Pulls busybox
/// from Docker Hub: runs only with PODBAG_TEST_NETWORK=1.
#[cfg(target_os = "linux")]
#[test]
fn the_recovery_training_goes_from_a_failed_start_to_a_working_page() {
    use podbag::volume_lesson::{Module, course, gui::Registration};
    if std::env::var("PODBAG_TEST_NETWORK").as_deref() != Ok("1") {
        eprintln!("PODBAG_TEST_NETWORK is not 1: skipped");
        return;
    }
    let library = tempfile::tempdir().unwrap();
    let library = library.path();
    let mut engine = Command::new(env!("CARGO_BIN_EXE_podbag"))
        .args(["lab", "workspace", "--containers"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(engine.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let root = PathBuf::from(line.trim_end());
    course::Attempt::start(&root, Module::Recovery).unwrap();
    let registration = Registration::open_for(library, &root, course::ACTIVE).unwrap();
    let view = |action: &str, session: Option<&str>, target: &str| {
        let snapshot = course::view(library, action, session, target).unwrap();
        serde_json::to_value(snapshot.unwrap()).unwrap()
    };
    let id_of = |snapshot: &serde_json::Value, name: &str| {
        snapshot["containers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["Names"][0].as_str().unwrap().trim_start_matches('/') == name)
            .map(|row| row["Id"].as_str().unwrap().to_string())
            .unwrap()
    };
    // The command a learner copies from the current step, run in the practice terminal.
    let cli = |snapshot: &serde_json::Value| {
        let line = snapshot["command"].as_str().unwrap();
        let words = shlex::split(line).unwrap();
        assert_eq!(words[0], "podbag");
        Command::new(env!("CARGO_BIN_EXE_podbag"))
            .args(&words[1..])
            .env(podbag::volume_lesson::SCOPE, &root)
            .env("PODBAG_DATA_DIR", root.join("data"))
            .env("PODBAG_RUNTIME_DIR", root.join("run"))
            .env("PODBAG_CONFIG_DIR", root.join("config"))
            .env("PODBAG_UI_DIR", root.join("ui"))
            .output()
            .unwrap()
    };
    let step = |stage: &str| {
        let snapshot = view("status", None, "");
        assert_eq!(snapshot["stage"], stage);
        snapshot
    };

    let failed = step("runFailed");
    assert!(
        cli(&failed).status.success(),
        "the failing server still starts"
    );
    let inspect = step("inspectFailed");
    assert_eq!(inspect["inspect_container"], "lab-web");
    let session = inspect["session"].as_str().unwrap().to_string();
    let session = Some(session.as_str());
    let exited = id_of(&inspect, "lab-web");
    assert!(course::view(library, "ports", session, &exited).is_err());
    view("inspect-failed", session, &exited);
    let logs = cli(&step("readRecoveryLogs"));
    assert!(
        String::from_utf8_lossy(&logs.stderr).contains("SITE_TITLE is required"),
        "{logs:?}"
    );
    assert!(cli(&step("removeFailed")).status.success());
    let fixed = cli(&step("runFixed"));
    assert!(fixed.status.success(), "{fixed:?}");
    let ports = step("ports");
    let running = id_of(&ports, "lab-web");
    assert_ne!(running, exited);
    view("ports", session, &running);
    step("browser");
    // httpd starts a moment after the container: the learner can press Open again.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while course::view(library, "open-web", session, &running).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the page never matched"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    step("stop");
    view("stop", session, &running);
    assert!(cli(&step("remove")).status.success());
    step("finish");

    drop(engine.stdin.take());
    assert!(engine.wait().unwrap().success());
    drop(registration);
    assert!(!root.exists());
}
