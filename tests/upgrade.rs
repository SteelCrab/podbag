#![cfg(unix)]
//! `podbag upgrade` against a fake release server on 127.0.0.1 (`PODBAG_RELEASES_URL`),
//! on a copy of the binary in a temp folder.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A release server: one release with these files, and the paths it was asked for.
struct Releases {
    url: String,
    asked: Arc<Mutex<Vec<String>>>,
}

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn asset() -> String {
    podbag::upgrade::this_asset().expect("a release file for this system")
}

impl Releases {
    /// Release `tag` with this platform's file `program` and a SHA256SUMS that says `sum`
    /// for it (None: the right one).
    fn start(tag: &str, program: Vec<u8>, sum: Option<String>) -> Releases {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let sums = format!(
            "{}  {}\n{}  other-file\n",
            sum.unwrap_or_else(|| sha256(&program)),
            asset(),
            "0".repeat(64)
        );
        let release = serde_json::json!({
            "tag_name": tag,
            "assets": [
                {"name": asset(), "browser_download_url": format!("{url}/dl/{}", asset())},
                {"name": "SHA256SUMS", "browser_download_url": format!("{url}/dl/SHA256SUMS")},
            ],
        })
        .to_string();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        let tag = tag.to_string();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                }
                log.lock().unwrap().push(path.clone());
                let (status, body): (&str, Vec<u8>) = match path.as_str() {
                    "/repos/SteelCrab/podbag/releases/latest" => ("200 OK", release.clone().into()),
                    p if p == format!("/repos/SteelCrab/podbag/releases/tags/{tag}") => {
                        ("200 OK", release.clone().into())
                    }
                    "/dl/SHA256SUMS" => ("200 OK", sums.clone().into()),
                    p if p == format!("/dl/{}", asset()) => ("200 OK", program.clone()),
                    _ => ("404 Not Found", b"{}".to_vec()),
                };
                let _ = write!(
                    conn,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(&body);
            }
        });
        Releases { url, asked }
    }

    fn downloads(&self) -> Vec<String> {
        let asked = self.asked.lock().unwrap();
        asked
            .iter()
            .filter(|p| p.starts_with("/dl/"))
            .cloned()
            .collect()
    }
}

/// A copy of podbag in a folder of its own, with its own homes.
struct Install {
    dir: tempfile::TempDir,
}

impl Install {
    fn new() -> Install {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_podbag"), dir.path().join("bin/podbag")).unwrap();
        Install { dir }
    }

    fn exe(&self) -> PathBuf {
        self.dir.path().join("bin/podbag")
    }

    fn upgrade(&self, releases: &Releases, from: Option<&str>, args: &[&str]) -> Output {
        let mut c = Command::new(self.exe());
        c.arg("upgrade")
            .args(args)
            .env("PODBAG_RELEASES_URL", &releases.url)
            .env("PODBAG_UI_DIR", self.dir.path().join("ui"))
            .env("PODBAG_MAC_HOME", self.dir.path().join("mac"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("run"))
            .env_remove("PODBAG_UPGRADE_FROM")
            .stdin(std::process::Stdio::null());
        if let Some(v) = from {
            c.env("PODBAG_UPGRADE_FROM", v);
        }
        c.output().unwrap()
    }
}

/// What a release would carry for this system: the binary, signed on macOS with the
/// virtualization entitlement as releases are.
fn release_program() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("podbag");
    std::fs::copy(env!("CARGO_BIN_EXE_podbag"), &file).unwrap();
    if cfg!(target_os = "macos") {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let ok = Command::new(repo.join("dev/macos/sign.sh"))
            .arg(&file)
            .status()
            .unwrap();
        assert!(ok.success());
    }
    std::fs::read(&file).unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Files left in the folder besides podbag (and podbag.old).
fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "podbag" && n != "podbag.old")
        .collect()
}

#[test]
fn a_newer_release_replaces_the_program() {
    let program = release_program();
    let releases = Releases::start(&format!("v{VERSION}"), program.clone(), None);
    let install = Install::new();
    let before = std::fs::read(install.exe()).unwrap();
    let out = install.upgrade(&releases, Some("0.0.1"), &["--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains(&format!("to {VERSION}")),
        "{}",
        text(&out)
    );
    assert_eq!(std::fs::read(install.exe()).unwrap(), program);
    assert_eq!(
        std::fs::read(install.dir.path().join("bin/podbag.old")).unwrap(),
        before
    );
    assert!(leftovers(&install.dir.path().join("bin")).is_empty());
    // The new program runs.
    let v = Command::new(install.exe())
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&v.stdout).trim(),
        format!("podbag {VERSION}")
    );
}

#[test]
fn a_wrong_checksum_changes_nothing() {
    let releases = Releases::start(
        &format!("v{VERSION}"),
        release_program(),
        Some("ab".repeat(32)),
    );
    let install = Install::new();
    let before = std::fs::read(install.exe()).unwrap();
    let out = install.upgrade(&releases, Some("0.0.1"), &["--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("does not match SHA256SUMS"),
        "{}",
        text(&out)
    );
    assert_eq!(std::fs::read(install.exe()).unwrap(), before);
    assert!(!install.dir.path().join("bin/podbag.old").exists());
    assert!(leftovers(&install.dir.path().join("bin")).is_empty());
}

#[test]
fn a_program_that_says_another_version_changes_nothing() {
    // The release says 99.0.0, but its file is this build.
    let releases = Releases::start("v99.0.0", release_program(), None);
    let install = Install::new();
    let before = std::fs::read(install.exe()).unwrap();
    let out = install.upgrade(&releases, None, &["--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("not \"podbag 99.0.0\""),
        "{}",
        text(&out)
    );
    assert_eq!(std::fs::read(install.exe()).unwrap(), before);
    assert!(leftovers(&install.dir.path().join("bin")).is_empty());
}

#[test]
fn the_same_version_is_up_to_date() {
    let releases = Releases::start(&format!("v{VERSION}"), b"never fetched".to_vec(), None);
    let install = Install::new();
    let out = install.upgrade(&releases, None, &["--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("is up to date"), "{}", text(&out));
    assert!(
        releases.downloads().is_empty(),
        "{:?}",
        releases.downloads()
    );
    // --check, for apps.
    let out = install.upgrade(&releases, None, &["--check", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["newer"], false);
    assert_eq!(v["latest"], VERSION);
    // It is saved for the UIs.
    let saved = std::fs::read_to_string(install.dir.path().join("ui/update.json")).unwrap();
    assert!(saved.contains(VERSION), "{saved}");
}

#[test]
fn an_older_release_is_not_installed_unless_asked_for() {
    let releases = Releases::start("v0.0.1", b"never fetched".to_vec(), None);
    let install = Install::new();
    let out = install.upgrade(&releases, None, &["--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("up to date"), "{}", text(&out));
    assert!(releases.downloads().is_empty());
}

#[test]
fn check_says_when_a_newer_one_exists() {
    let releases = Releases::start("v99.0.0", b"never fetched".to_vec(), None);
    let install = Install::new();
    let out = install.upgrade(&releases, None, &["--check"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("podbag 99.0.0 is available"),
        "{}",
        text(&out)
    );
    assert!(releases.downloads().is_empty());
}

#[test]
fn without_a_terminal_it_asks_for_yes() {
    let releases = Releases::start("v99.0.0", b"never fetched".to_vec(), None);
    let install = Install::new();
    let out = install.upgrade(&releases, None, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("--yes"), "{}", text(&out));
    assert!(releases.downloads().is_empty());
}

#[test]
fn a_folder_it_cannot_write_gives_the_sudo_command() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes anywhere
    }
    use std::os::unix::fs::PermissionsExt;
    let releases = Releases::start(&format!("v{VERSION}"), release_program(), None);
    let install = Install::new();
    let bin = install.dir.path().join("bin");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = install.upgrade(&releases, Some("0.0.1"), &["--yes"]);
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(77), "{}", text(&out));
    let real = install.exe().canonicalize().unwrap();
    assert!(
        text(&out).contains(&format!("sudo {} upgrade", real.display())),
        "{}",
        text(&out)
    );
    assert!(releases.downloads().is_empty());
}

#[test]
fn a_missing_release_is_an_error() {
    let releases = Releases::start("v99.0.0", Vec::new(), None);
    let install = Install::new();
    let out = install.upgrade(&releases, None, &["--version", "5.5.5", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("no release v5.5.5"), "{}", text(&out));
}

#[test]
fn plain_http_elsewhere_is_refused() {
    let install = Install::new();
    let out = Command::new(install.exe())
        .args(["upgrade", "--check"])
        .env("PODBAG_RELEASES_URL", "http://example.com")
        .env("PODBAG_UI_DIR", install.dir.path().join("ui"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("HTTPS only"), "{}", text(&out));
}
