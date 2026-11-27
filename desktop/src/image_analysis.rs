//! Scan the selected engine's image with an explicitly invoked local Trivy.

use podbag::client::{Client, Closer, Kind, choice::find_tool};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::Duration;

const MAX_REPORT: u64 = 64 * 1024 * 1024;
/// An export that sends nothing for this long is cut off. A large export that keeps
/// sending has no limit.
const EXPORT_IDLE: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Failure {
    Busy,
    EngineChanged,
    ImageUnavailable,
    ScannerMissing,
    Export,
    ExportStalled,
    Workspace,
    Scanner,
    Report,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub image_id: String,
    pub packages: Vec<Package>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    target: String,
    name: String,
    version: String,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    severity: Severity,
    advisory: String,
    package: String,
    installed: String,
    fixed: String,
    target: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all(serialize = "lowercase", deserialize = "UPPERCASE"))]
enum Severity {
    Critical,
    High,
    Medium,
    Low,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TrivyReport {
    schema_version: u8,
    artifact_type: String,
    results: Option<Vec<TrivyTarget>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TrivyTarget {
    target: String,
    packages: Option<Vec<TrivyPackage>>,
    vulnerabilities: Option<Vec<TrivyFinding>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TrivyPackage {
    name: String,
    #[serde(default)]
    epoch: u64,
    #[serde(default)]
    version: String,
    #[serde(default)]
    release: String,
}

impl TrivyPackage {
    /// The installed version as RPM writes it, `epoch:version-release`. Trivy keeps the
    /// parts apart; its findings already join them this way.
    fn full_version(&self) -> String {
        let mut version = self.version.clone();
        if !self.release.is_empty() {
            version = format!("{version}-{}", self.release);
        }
        if self.epoch != 0 {
            version = format!("{}:{version}", self.epoch);
        }
        version
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TrivyFinding {
    #[serde(rename = "VulnerabilityID")]
    advisory: String,
    #[serde(rename = "PkgName")]
    package: String,
    #[serde(rename = "InstalledVersion")]
    installed: String,
    #[serde(rename = "FixedVersion", default)]
    fixed: String,
    severity: Severity,
}

pub fn analyze(client: &Client, id: &str, cache: &Path) -> Result<Report, Failure> {
    let scanner = find_tool("trivy").ok_or(Failure::ScannerMissing)?;
    analyze_with(client, id, cache, &scanner)
}

fn digest(id: &str) -> Option<&str> {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    (hex.len() == 64
        && hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
    .then_some(hex)
}

fn check_image(client: &Client, id: &str, hex: &str) -> Result<(), Failure> {
    let image = client
        .inspect(Kind::Images, id)
        .map_err(|_| Failure::ImageUnavailable)?;
    if image["Id"].as_str().and_then(digest) != Some(hex) {
        return Err(Failure::ImageUnavailable);
    }
    Ok(())
}

fn analyze_with(
    client: &Client,
    id: &str,
    cache: &Path,
    scanner: &Path,
) -> Result<Report, Failure> {
    // Passing the full ID makes tag moves irrelevant and rules out prefix collisions.
    let hex = digest(id).ok_or(Failure::ImageUnavailable)?;
    let name = format!("sha256:{hex}");
    let scanner = scanner
        .canonicalize()
        .map_err(|_| Failure::ScannerMissing)?;
    check_image(client, &name, hex)?;
    let work = tempfile::tempdir().map_err(|_| Failure::Workspace)?;
    let archive = work.path().join("image.tar");
    let report = work.path().join("report.json");
    std::fs::create_dir_all(cache).map_err(|_| Failure::Workspace)?;
    let cache = cache.canonicalize().map_err(|_| Failure::Workspace)?;
    {
        let mut stream = client
            .export_images(&[&name])
            .map_err(|_| Failure::Export)?;
        let mut output = File::create(&archive).map_err(|_| Failure::Workspace)?;
        let closer = stream.closer();
        save_export(&mut stream, &closer, &mut output, EXPORT_IDLE)?;
    }

    let mut command = Command::new(scanner);
    command
        .args(["image", "--input"])
        .arg(&archive)
        .args(["--scanners", "vuln", "--format", "json", "--list-all-pkgs"])
        .args(["--offline-scan", "--skip-version-check", "--timeout", "5m"])
        .arg("--cache-dir")
        .arg(&cache)
        .arg("--output")
        .arg(&report)
        .current_dir(work.path())
        .env_clear()
        .env("HOME", work.path())
        .env("USERPROFILE", work.path())
        .env("DOCKER_CONFIG", work.path())
        .env("XDG_CONFIG_HOME", work.path())
        .env("TMPDIR", work.path())
        .env("TMP", work.path())
        .env("TEMP", work.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        command.creation_flags(0x0800_0000);
    }
    // Archive input keeps Trivy from fetching the image. The process can still update its
    // public databases. It receives no login, engine endpoint, or caller environment.
    let status = command.status().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => Failure::ScannerMissing,
        _ => Failure::Scanner,
    })?;
    if !status.success() {
        return Err(Failure::Scanner);
    }
    if !std::fs::symlink_metadata(&report)
        .map_err(|_| Failure::Report)?
        .file_type()
        .is_file()
    {
        return Err(Failure::Report);
    }
    let mut data = Vec::new();
    File::open(&report)
        .and_then(|file| file.take(MAX_REPORT + 1).read_to_end(&mut data))
        .map_err(|_| Failure::Report)?;
    if data.len() as u64 > MAX_REPORT {
        return Err(Failure::Report);
    }
    let report = read_report(id, &data)?;
    check_image(client, &name, hex)?;
    Ok(report)
}

/// Copy the export to `output`. If the engine sends nothing for `idle`, the connection is
/// closed and the export fails, so the scan ends and frees its lock.
fn save_export(
    stream: &mut impl Read,
    closer: &Closer,
    output: &mut impl Write,
    idle: Duration,
) -> Result<(), Failure> {
    let stalled = &AtomicBool::new(false);
    let (progress, watch) = channel();
    let copied = std::thread::scope(|scope| {
        scope.spawn(move || {
            loop {
                match watch.recv_timeout(idle) {
                    Ok(()) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        stalled.store(true, Ordering::SeqCst);
                        closer.close();
                        return;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        copy_reporting(stream, output, progress)
    });
    // A closed connection can look like a normal end, so check this first.
    if stalled.load(Ordering::SeqCst) {
        return Err(Failure::ExportStalled);
    }
    copied.map_err(|_| Failure::Export)
}

/// Copy `stream` to `output`, with a message on `progress` after each read. Dropping
/// `progress` at the end stops the watcher.
fn copy_reporting(
    stream: &mut impl Read,
    output: &mut impl Write,
    progress: Sender<()>,
) -> std::io::Result<()> {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) => return output.flush(),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        output.write_all(&buffer[..count])?;
        let _ = progress.send(());
    }
}

fn read_report(id: &str, bytes: &[u8]) -> Result<Report, Failure> {
    let raw: TrivyReport = serde_json::from_slice(bytes).map_err(|_| Failure::Report)?;
    if raw.schema_version != 2 || raw.artifact_type != "container_image" {
        return Err(Failure::Report);
    }

    let mut packages = Vec::new();
    let mut findings = Vec::new();
    for result in raw.results.into_iter().flatten() {
        packages.extend(
            result
                .packages
                .into_iter()
                .flatten()
                .map(|package| Package {
                    target: result.target.clone(),
                    version: package.full_version(),
                    name: package.name,
                }),
        );
        findings.extend(
            result
                .vulnerabilities
                .into_iter()
                .flatten()
                .map(|finding| Finding {
                    target: result.target.clone(),
                    severity: finding.severity,
                    advisory: finding.advisory,
                    package: finding.package,
                    installed: finding.installed,
                    fixed: finding.fixed,
                }),
        );
    }
    packages.sort_unstable();
    packages.dedup();
    findings.sort_unstable();
    findings.dedup();
    Ok(Report {
        image_id: id.to_string(),
        packages,
        findings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const INPUT_REPORT: &str = r#"{
        "SchemaVersion": 2,
        "ArtifactType": "container_image",
        "Results": [{
            "Target": "alpine",
            "Packages": [
                {"Name":"libssl3","Version":"3.0.0"},
                {"Name":"libssl3","Version":"3.0.0"},
                {"Name":"musl","Version":"1.2.0"}
            ],
            "Vulnerabilities": [
                {"VulnerabilityID":"DEMO-MEDIUM","PkgName":"musl","InstalledVersion":"1.2.0","Severity":"MEDIUM"},
                {"VulnerabilityID":"DEMO-HIGH","PkgName":"libssl3","InstalledVersion":"3.0.0","FixedVersion":"3.0.1","Severity":"HIGH"}
            ]
        }]
    }"#;

    #[test]
    fn reports_keep_severity_packages_and_fixed_versions() {
        let report = read_report(ID, INPUT_REPORT.as_bytes()).unwrap();
        assert_eq!(report.image_id, ID);
        assert_eq!(report.packages.len(), 2);
        assert_eq!(report.findings.len(), 2);
        assert_eq!(report.findings[0].severity, Severity::High);
        assert_eq!(report.findings[0].fixed, "3.0.1");
        assert_eq!(report.findings[1].severity, Severity::Medium);
        assert_eq!(report.findings[1].fixed, "");
    }

    #[test]
    fn package_versions_keep_epoch_and_release() {
        let report = read_report(
            ID,
            br#"{"SchemaVersion":2,"ArtifactType":"container_image","Results":[{
                "Target":"rocky",
                "Packages":[
                    {"Name":"openssl-libs","Epoch":1,"Version":"3.0.7","Release":"27.el9"},
                    {"Name":"openssl-libs","Epoch":1,"Version":"3.0.7","Release":"28.el9"},
                    {"Name":"bash","Version":"5.1.8","Release":"9.el9"},
                    {"Name":"musl","Version":"1.2.0"}
                ]
            }]}"#,
        )
        .unwrap();
        let versions: Vec<&str> = report
            .packages
            .iter()
            .map(|package| package.version.as_str())
            .collect();
        assert_eq!(
            versions,
            ["5.1.8-9.el9", "1.2.0", "1:3.0.7-27.el9", "1:3.0.7-28.el9"]
        );
    }

    #[test]
    fn accepts_empty_image_reports_but_rejects_other_artifacts_and_bad_json() {
        let report = read_report(
            ID,
            br#"{"SchemaVersion":2,"ArtifactType":"container_image","Results":null}"#,
        )
        .unwrap();
        assert!(report.packages.is_empty());
        assert!(report.findings.is_empty());
        for malformed in [
            br#"{"SchemaVersion":2,"ArtifactType":"filesystem"}"#.as_slice(),
            br#"{"SchemaVersion":1,"ArtifactType":"container_image"}"#,
            b"not a report",
        ] {
            assert_eq!(read_report(ID, malformed), Err(Failure::Report));
        }
    }

    #[cfg(unix)]
    mod local {
        use super::*;
        use podbag::client::fake::{FakeEngine, json};
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU32, Ordering};

        fn scanner(folder: &Path, script: &str) -> PathBuf {
            let path = folder.join("trivy");
            std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        }

        fn engine(change: Option<Option<&'static str>>) -> FakeEngine {
            let read = AtomicU32::new(0);
            FakeEngine::start(move |_method, path| {
                if path == format!("/images/{ID}/json") {
                    if read.fetch_add(1, Ordering::Relaxed) > 0 {
                        if let Some(next) = change {
                            return match next {
                                Some(id) => json(200, json!({"Id":id})),
                                None => json(404, json!({"message":"No such image"})),
                            };
                        }
                    }
                    return json(200, json!({"Id":ID}));
                }
                if path == format!("/images/get?names={}", podbag::client::encode(ID)) {
                    return (200, "application/x-tar", b"selected image data\n".to_vec());
                }
                json(404, json!({"message":"No such request"}))
            })
        }

        fn reporting_scanner(folder: &Path) -> PathBuf {
            scanner(
                folder,
                &format!(
                    r#"
[ "$1" = image ]
[ "$2" = --input ]
IFS= read -r data < "$3"
[ "$data" = "selected image data" ]
[ "$HOME" = "$DOCKER_CONFIG" ]
[ "$(cd "$HOME" && pwd -P)" = "$(pwd -P)" ]
[ ! -e "$DOCKER_CONFIG/config.json" ]
[ -z "${{PODBAG_SCAN_TEST_SECRET:-}}" ]
[ -z "${{TRIVY_PASSWORD:-}}" ]
while [ "$1" != "--output" ]; do shift; done
cat_path="$2"
printf '%s\n' '{INPUT_REPORT}' > "$cat_path"
"#
                ),
            )
        }

        #[test]
        fn exports_only_the_full_selected_id_and_reads_a_local_report() {
            let temp = tempfile::tempdir().unwrap();
            let engine = engine(None);
            let report = analyze_with(
                &engine.client,
                ID,
                &temp.path().join("cache"),
                &reporting_scanner(temp.path()),
            )
            .unwrap();
            assert_eq!(report.findings.len(), 2);
            assert_eq!(
                engine.requests(),
                [
                    format!("GET /images/{ID}/json"),
                    format!("GET /images/get?names={}", podbag::client::encode(ID)),
                    format!("GET /images/{ID}/json"),
                ]
            );
            for index in 0..3 {
                assert_eq!(engine.header(index, "X-Registry-Auth"), None);
                assert_eq!(engine.header(index, "Authorization"), None);
            }
        }

        #[test]
        fn does_not_return_a_report_if_the_image_was_deleted_or_replaced() {
            let temp = tempfile::tempdir().unwrap();
            let scanner = reporting_scanner(temp.path());
            for replacement in [
                None,
                Some("sha256:2222222222222222222222222222222222222222222222222222222222222222"),
            ] {
                let engine = engine(Some(replacement));
                let result = analyze_with(&engine.client, ID, &temp.path().join("cache"), &scanner);
                assert_eq!(result, Err(Failure::ImageUnavailable));
            }
        }

        #[test]
        fn an_export_that_stops_sending_is_cut_off() {
            use std::io::{BufRead, BufReader};
            use std::os::unix::net::UnixListener;
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("docker.sock");
            let listener = UnixListener::bind(&path).unwrap();
            // An engine that sends part of the archive and then waits, connection open.
            std::thread::spawn(move || {
                let (mut conn, _) = listener.accept().unwrap();
                let mut request = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                while request.read_line(&mut line).unwrap() > 2 {
                    line.clear();
                }
                conn.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-tar\r\n\
                      Content-Length: 1000\r\n\r\npartial",
                )
                .unwrap();
                let _ = request.read_to_end(&mut Vec::new());
            });
            let client = Client::new(podbag::client::Endpoint::Socket { path, own: false });
            let mut stream = client.export_images(&[ID]).unwrap();
            let closer = stream.closer();
            let mut output = Vec::new();
            let started = std::time::Instant::now();
            assert_eq!(
                save_export(
                    &mut stream,
                    &closer,
                    &mut output,
                    Duration::from_millis(200)
                ),
                Err(Failure::ExportStalled)
            );
            assert!(started.elapsed() < Duration::from_secs(10));
        }

        #[test]
        fn a_complete_export_is_saved() {
            let mut output = Vec::new();
            save_export(
                &mut b"whole archive".as_slice(),
                &Closer::default(),
                &mut output,
                Duration::from_secs(5),
            )
            .unwrap();
            assert_eq!(output, b"whole archive");
        }

        #[test]
        fn rejects_names_and_reports_missing_or_failed_scanners() {
            let temp = tempfile::tempdir().unwrap();
            let engine = engine(None);
            let cache = temp.path().join("cache");
            let missing = temp.path().join("missing");
            assert_eq!(
                analyze_with(&engine.client, "my-image:latest", &cache, &missing),
                Err(Failure::ImageUnavailable)
            );
            assert_eq!(
                analyze_with(&engine.client, ID, &cache, &missing),
                Err(Failure::ScannerMissing)
            );
            assert!(engine.requests().is_empty());
            assert_eq!(
                analyze_with(&engine.client, ID, &cache, &scanner(temp.path(), "exit 1")),
                Err(Failure::Scanner)
            );
        }
    }
}
