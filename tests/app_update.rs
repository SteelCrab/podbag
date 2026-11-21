#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
//! Podbag Desktop's own update on macOS, against a fake release server on 127.0.0.1
//! (`PODBAG_RELEASES_URL`): a small signed `podbag.app` in a temp folder is replaced.

use podbag::dashboard::app_update::{Installer, install};
use podbag::upgrade::Version;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

const ZIP: &str = "podbag-desktop-aarch64-macos.zip";

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A small signed `podbag.app` in `dir` that says `version`.
fn fake_app(dir: &Path, version: &str) -> PathBuf {
    let app = dir.join("podbag.app");
    std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    std::fs::copy("/usr/bin/true", app.join("Contents/MacOS/podbag-desktop")).unwrap();
    std::fs::write(
        app.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>podbag-desktop</string>
  <key>CFBundleIdentifier</key><string>io.github.steelcrab.podbag.test</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
        ),
    )
    .unwrap();
    let ok = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&app)
        .status()
        .unwrap();
    assert!(ok.success(), "codesign");
    app
}

/// The release .zip of an app that says `version`, as the release has it.
fn app_zip(version: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let app = fake_app(dir.path(), version);
    let zip = dir.path().join(ZIP);
    let ok = Command::new("ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(&app)
        .arg(&zip)
        .status()
        .unwrap();
    assert!(ok.success(), "ditto");
    std::fs::read(zip).unwrap()
}

/// One server for this test binary (the address is read from the environment): its
/// releases by tag, and every path it was asked for.
struct Server {
    url: String,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

fn server() -> &'static Server {
    static SERVER: OnceLock<Server> = OnceLock::new();
    SERVER.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        // SAFETY: set once, before any test reads it, from the first test to run.
        unsafe { std::env::set_var("PODBAG_RELEASES_URL", &url) };
        let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let (f, a) = (files.clone(), asked.clone());
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
                a.lock().unwrap().push(path.clone());
                let body = f.lock().unwrap().get(&path).cloned();
                let (status, body) = match body {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", b"{}".to_vec()),
                };
                let _ = write!(
                    conn,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(&body);
            }
        });
        Server { url, files, asked }
    })
}

impl Server {
    /// Release `tag` with the app .zip; SHA256SUMS says `sum` for it (None: the right one).
    /// With `latest`, it is also the latest release.
    fn release(&self, tag: &str, zip: Vec<u8>, sum: Option<String>, latest: bool) {
        let url = &self.url;
        let sums = format!("{}  {ZIP}\n", sum.unwrap_or_else(|| sha256(&zip)));
        let json = serde_json::json!({
            "tag_name": tag,
            "assets": [
                {"name": ZIP, "browser_download_url": format!("{url}/dl/{tag}/{ZIP}")},
                {"name": "SHA256SUMS", "browser_download_url": format!("{url}/dl/{tag}/SHA256SUMS")},
            ],
        })
        .to_string();
        let mut files = self.files.lock().unwrap();
        files.insert(
            format!("/repos/SteelCrab/podbag/releases/tags/{tag}"),
            json.clone().into(),
        );
        if latest {
            files.insert(
                "/repos/SteelCrab/podbag/releases/latest".into(),
                json.into(),
            );
        }
        files.insert(format!("/dl/{tag}/SHA256SUMS"), sums.into());
        files.insert(format!("/dl/{tag}/{ZIP}"), zip);
    }

    fn downloads_of(&self, tag: &str) -> usize {
        let prefix = format!("/dl/{tag}/");
        self.asked
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with(&prefix))
            .count()
    }
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn version_of(app: &Path) -> String {
    let out = Command::new("plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// What the folder holds besides the app.
fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "podbag.app")
        .collect()
}

#[test]
fn a_newer_release_replaces_the_app() {
    let s = server();
    s.release("v9.9.1", app_zip("9.9.1"), None, false);
    let dir = tempfile::tempdir().unwrap();
    let app = fake_app(dir.path(), "0.3.0");
    let restart = install(
        &Installer::MacApp(app.clone()),
        Some(&v("9.9.1")),
        &v("0.3.0"),
    )
    .unwrap();
    assert_eq!(version_of(&app), "9.9.1");
    assert!(
        leftovers(dir.path()).is_empty(),
        "{:?}",
        leftovers(dir.path())
    );
    let argv: Vec<String> = restart
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        argv[argv.len() - 2..],
        ["/usr/bin/open".to_string(), app.display().to_string()]
    );
    let ok = Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&app)
        .status()
        .unwrap();
    assert!(ok.success(), "the new app's signature");
}

#[test]
fn a_wrong_checksum_changes_nothing() {
    let s = server();
    s.release("v9.9.2", app_zip("9.9.2"), Some("ab".repeat(32)), false);
    let dir = tempfile::tempdir().unwrap();
    let app = fake_app(dir.path(), "0.3.0");
    let err = install(
        &Installer::MacApp(app.clone()),
        Some(&v("9.9.2")),
        &v("0.3.0"),
    )
    .unwrap_err();
    assert!(err.contains("does not match SHA256SUMS"), "{err}");
    assert_eq!(version_of(&app), "0.3.0");
    assert!(
        leftovers(dir.path()).is_empty(),
        "{:?}",
        leftovers(dir.path())
    );
}

#[test]
fn an_app_that_says_another_version_changes_nothing() {
    let s = server();
    s.release("v9.9.3", app_zip("9.9.0"), None, false);
    let dir = tempfile::tempdir().unwrap();
    let app = fake_app(dir.path(), "0.3.0");
    let err = install(
        &Installer::MacApp(app.clone()),
        Some(&v("9.9.3")),
        &v("0.3.0"),
    )
    .unwrap_err();
    assert!(err.contains("\"9.9.0\""), "{err}");
    assert_eq!(version_of(&app), "0.3.0");
    assert!(
        leftovers(dir.path()).is_empty(),
        "{:?}",
        leftovers(dir.path())
    );
}

#[test]
fn the_latest_release_that_is_not_newer_downloads_nothing() {
    let s = server();
    s.release("v0.3.0", b"never fetched".to_vec(), None, true);
    let dir = tempfile::tempdir().unwrap();
    let app = fake_app(dir.path(), "0.3.0");
    let err = install(&Installer::MacApp(app.clone()), None, &v("0.3.0")).unwrap_err();
    assert!(err.contains("up to date"), "{err}");
    assert_eq!(s.downloads_of("v0.3.0"), 0);
    assert_eq!(version_of(&app), "0.3.0");
}

#[test]
fn a_folder_it_cannot_write_says_so() {
    use std::os::unix::fs::PermissionsExt;
    let s = server();
    s.release("v9.9.4", app_zip("9.9.4"), None, false);
    let dir = tempfile::tempdir().unwrap();
    let app = fake_app(dir.path(), "0.3.0");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let got = install(
        &Installer::MacApp(app.clone()),
        Some(&v("9.9.4")),
        &v("0.3.0"),
    );
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = got.unwrap_err();
    assert!(err.contains("cannot write to"), "{err}");
    assert_eq!(s.downloads_of("v9.9.4"), 0);
    assert_eq!(version_of(&app), "0.3.0");
}
