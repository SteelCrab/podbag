//! `podbag upgrade`: replace this program with a release from GitHub.
//!
//! The release is read from GitHub's API over HTTPS. This platform's file and the
//! release's `SHA256SUMS` are downloaded next to the program; the file must match its
//! checksum before anything runs it. Then it must say the expected version (and, on
//! macOS, carry a valid signature with the virtualization entitlement). Only then is it
//! renamed over the program; the old one stays as `<name>.old`.
//!
//! `PODBAG_RELEASES_URL` (tests only) sets another API address; with it, plain HTTP to
//! 127.0.0.1 is allowed, and `PODBAG_UPGRADE_FROM` pretends another current version.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The repository of the releases.
pub const REPO: &str = "SteelCrab/podbag";

/// The exit code when the program's folder is not writable (sysexits' EX_NOPERM).
pub const NEEDS_PERMISSION: i32 = 77;

/// The most a release file may be.
const MAX_ASSET: u64 = 256 << 20;

/// How old the saved update check may be before the UIs check again.
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);

/// A release version: `major.minor.patch`, with an optional pre-release (`-rc.1`), which
/// comes before the release itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    /// `1.2.3`, `v1.2.3`, `1.2.3-rc.1`; build metadata (`+…`) is ignored.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((c, p)) if !p.is_empty() => (c, Some(p.to_string())),
            Some(_) => return None,
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut next = || -> Option<u64> {
            let p = parts.next()?;
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse().ok())
                .flatten()
        };
        let v = Version {
            major: next()?,
            minor: next()?,
            patch: next()?,
            pre,
        };
        parts.next().is_none().then_some(v)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => pre_cmp(a, b),
            })
    }
}

/// Pre-releases compare part by part: numbers as numbers, below words.
fn pre_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.split('.'), b.split('.'));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => {
                let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => p.cmp(q),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

/// The version of this program (`PODBAG_UPGRADE_FROM` in tests).
pub fn current() -> Version {
    let real = env!("CARGO_PKG_VERSION");
    let pretend = test_mode()
        .then(|| std::env::var("PODBAG_UPGRADE_FROM").ok())
        .flatten();
    pretend
        .as_deref()
        .and_then(Version::parse)
        .or_else(|| Version::parse(real))
        .unwrap_or(Version {
            major: 0,
            minor: 0,
            patch: 0,
            pre: None,
        })
}

/// The release file for an operating system and CPU (Rust's names: `linux`, `macos`,
/// `windows`; `x86_64`, `aarch64`).
pub fn asset_name(os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        ("linux", "x86_64" | "aarch64") => Some(format!("podbag-{arch}-linux")),
        ("macos", "x86_64" | "aarch64") => Some(format!("podbag-{arch}-macos")),
        ("windows", "x86_64" | "aarch64") => Some(format!("podbag-{arch}-windows.exe")),
        _ => None,
    }
}

/// This platform's release file. On a Mac, the one for its CPU: an Intel build that
/// Rosetta runs on Apple silicon cannot start the VM, so it upgrades to the arm64 build.
pub fn this_asset() -> Option<String> {
    #[cfg(target_os = "macos")]
    let arch = crate::macos::cpu::mac();
    #[cfg(not(target_os = "macos"))]
    let arch = std::env::consts::ARCH;
    asset_name(std::env::consts::OS, arch)
}

/// The SHA-256 (lowercase hex) that a `SHA256SUMS` file gives for `name`. Lines are
/// `<hash>  <name>` (`*<name>` in binary mode).
pub fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn test_mode() -> bool {
    std::env::var_os("PODBAG_RELEASES_URL").is_some_and(|v| !v.is_empty())
}

fn api_base() -> String {
    std::env::var("PODBAG_RELEASES_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.github.com".into())
        .trim_end_matches('/')
        .to_string()
}

/// Whether a URL may be used: HTTPS, or plain HTTP to 127.0.0.1 in tests.
pub fn allowed_url(url: &str, test: bool) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("https://")
        || (test
            && (lower.starts_with("http://127.0.0.1:") || lower.starts_with("http://127.0.0.1/")))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .https_only(!test_mode())
        .user_agent(format!("podbag/{}", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_resolve(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// A release: its version and its files (name, download URL).
#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub assets: Vec<(String, String)>,
}

impl Release {
    /// The download URL of the file `name`.
    pub fn asset(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, u)| u.as_str())
    }
}

/// Read a release from the API: the latest one, or the one of `version`.
pub fn fetch_release(version: Option<&Version>) -> Result<Release, String> {
    let url = match version {
        None => format!("{}/repos/{REPO}/releases/latest", api_base()),
        Some(v) => format!("{}/repos/{REPO}/releases/tags/v{v}", api_base()),
    };
    if !allowed_url(&url, test_mode()) {
        return Err(format!("refusing {url}: releases are read over HTTPS only"));
    }
    let resp = agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("cannot reach GitHub ({url}): {e}"))?;
    let status = resp.status().as_u16();
    if status == 404 {
        return Err(match version {
            Some(v) => format!("there is no release v{v}"),
            None => "there is no release yet".into(),
        });
    }
    if status != 200 {
        return Err(format!("GitHub answered {status} for {url}"));
    }
    let text = resp
        .into_body()
        .with_config()
        .limit(8 << 20)
        .read_to_string()
        .map_err(|e| format!("cannot read the release: {e}"))?;
    parse_release(&text)
}

/// A release from the API's JSON.
pub fn parse_release(text: &str) -> Result<Release, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("cannot read the release: {e}"))?;
    let tag = v["tag_name"].as_str().unwrap_or_default();
    let version =
        Version::parse(tag).ok_or_else(|| format!("the release's tag {tag:?} is not a version"))?;
    let assets = v["assets"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x["name"].as_str()?.to_string(),
                        x["browser_download_url"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { version, assets })
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Checked {
    pub current: String,
    pub latest: String,
    /// The latest release is newer than this program.
    pub newer: bool,
}

/// Ask GitHub for the latest release, and save what it said for the UIs.
pub fn check() -> Result<Checked, String> {
    let release = fetch_release(None)?;
    let now = current();
    save_check(&release.version);
    Ok(Checked {
        current: now.to_string(),
        latest: release.version.to_string(),
        newer: release.version > now,
    })
}

fn cache_file() -> Option<PathBuf> {
    crate::client::ui_dir().map(|d| d.join("update.json"))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn save_check(latest: &Version) {
    if let Some(file) = cache_file() {
        let _ = crate::client::choice::update_json(&file, |v| {
            v["checked"] = unix_now().into();
            v["latest"] = latest.to_string().into();
        });
    }
}

/// The last check: when (Unix seconds) and the latest version then.
pub fn last_check() -> Option<(u64, Version)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(cache_file()?).ok()?).ok()?;
    Some((
        v["checked"].as_u64()?,
        Version::parse(v["latest"].as_str()?)?,
    ))
}

/// A newer release, as the last check found it (no network).
pub fn known_newer() -> Option<Version> {
    last_check().map(|(_, v)| v).filter(|v| *v > current())
}

/// A newer release: from the last check, or from a new one when that is older than
/// [`CHECK_EVERY`]. It may wait on the network: call it off the screen's thread.
/// `PODBAG_NO_UPDATE_CHECK=1` turns the network check off.
pub fn newer_release() -> Option<Version> {
    let stale =
        last_check().is_none_or(|(at, _)| unix_now().saturating_sub(at) >= CHECK_EVERY.as_secs());
    let off = std::env::var("PODBAG_NO_UPDATE_CHECK").is_ok_and(|v| v == "1");
    if stale && !off && running_in_wsl_distro().is_none() {
        let _ = check();
    }
    known_newer()
}

/// On Windows the engine in podbag's WSL distribution comes with podbag.exe: inside it,
/// an upgrade is refused (and the UIs do not check).
fn running_in_wsl_distro() -> Option<()> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    text.lines().any(|l| l.trim() == "ID=podbag").then_some(())
}

/// Download `url` into `out`, at most `limit` bytes; the SHA-256 of what came.
fn download(url: &str, out: &mut impl Write, limit: u64) -> Result<String, String> {
    use sha2::Digest;
    if !allowed_url(url, test_mode()) {
        return Err(format!(
            "refusing {url}: downloads are made over HTTPS only"
        ));
    }
    let resp = agent()
        .get(url)
        .call()
        .map_err(|e| format!("cannot download {url}: {e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("cannot download {url}: {}", resp.status()));
    }
    let mut body = resp.into_body();
    let mut reader = body.as_reader();
    let mut hash = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("cannot download {url}: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            return Err(format!("{url} is larger than {} MiB", limit >> 20));
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|e| format!("cannot write the download: {e}"))?;
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The file of this program (links resolved).
pub fn program_file() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    exe.canonicalize()
        .map_err(|e| format!("cannot find this program ({}): {e}", exe.display()))
}

/// Where a Podbag Desktop package put the app and the CLI that comes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Package {
    /// macOS: inside this `.app` bundle (`<app>/Contents/MacOS/`).
    MacApp(PathBuf),
    /// Linux: `/usr/lib/podbag-desktop/`, from the .deb or .rpm.
    Linux,
    /// Windows: `Podbag Desktop` in Program Files, from the .msi.
    Windows,
}

/// The package that put `file` (the app or its CLI) in place, by its folder.
pub fn package_of(file: &Path) -> Option<Package> {
    let dir = file.parent()?;
    if dir == Path::new("/usr/lib/podbag-desktop") {
        return Some(Package::Linux);
    }
    if dir.ends_with("Contents/MacOS") {
        let app = dir.parent()?.parent()?;
        if app
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("app"))
        {
            return Some(Package::MacApp(app.to_path_buf()));
        }
    }
    let windows_dir = dir
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Podbag Desktop"));
    (cfg!(windows) && windows_dir).then_some(Package::Windows)
}

/// Download the release's file `name` to `out` (a new file) and check it against the
/// release's `SHA256SUMS`. On any error `out` is removed.
pub fn download_checked(release: &Release, name: &str, out: &Path) -> Result<(), String> {
    let v = &release.version;
    let url = release
        .asset(name)
        .ok_or_else(|| format!("release v{v} has no {name}"))?;
    let sums_url = release
        .asset("SHA256SUMS")
        .ok_or_else(|| format!("release v{v} has no SHA256SUMS: it cannot be checked"))?;
    let mut sums = Vec::new();
    download(sums_url, &mut sums, 1 << 20)?;
    let want = checksum_for(&String::from_utf8_lossy(&sums), name)
        .ok_or_else(|| format!("SHA256SUMS has no line for {name}"))?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let done = (|| {
        let mut w = std::io::BufWriter::new(&file);
        let got = download(url, &mut w, MAX_ASSET)?;
        w.flush()
            .map_err(|e| format!("cannot write the download: {e}"))?;
        drop(w);
        file.sync_all()
            .map_err(|e| format!("cannot write the download: {e}"))?;
        if got == want {
            Ok(())
        } else {
            Err(format!(
                "the download of {name} does not match SHA256SUMS (got {got}, want {want})"
            ))
        }
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(out);
    }
    done
}

/// Whether an error means "not allowed to write here".
fn denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
        || e.kind() == std::io::ErrorKind::ReadOnlyFilesystem
        || e.raw_os_error() == Some(30)
}

/// A file being made next to the program; removed on drop unless kept.
struct Staged {
    path: PathBuf,
    file: Option<std::fs::File>,
    keep: bool,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.keep {
            self.file = None;
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Why staging failed: no permission (with the folder), or something else.
#[derive(Debug)]
pub enum StageError {
    Denied(PathBuf),
    Other(String),
}

fn stage(exe: &Path) -> Result<Staged, StageError> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    let path = dir.join(format!(".{name}.upgrade-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o700);
    }
    match opts.open(&path) {
        Ok(file) => Ok(Staged {
            path,
            file: Some(file),
            keep: false,
        }),
        Err(e) if denied(&e) => Err(StageError::Denied(dir.to_path_buf())),
        Err(e) => Err(StageError::Other(format!("{}: {e}", path.display()))),
    }
}

/// Put `new` in place of `exe`, keeping the old one as `<exe>.old`; returns that path.
/// On Unix the rename over `exe` is atomic: a command started meanwhile runs the old
/// program or the new one. Windows cannot replace a running .exe: it is renamed first.
pub fn replace(exe: &Path, new: &Path) -> Result<PathBuf, String> {
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    let old = exe.with_file_name(format!("{name}.old"));
    let err = |what: &str, e: std::io::Error| format!("cannot {what}: {e}");
    match std::fs::remove_file(&old) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(err(&format!("remove {}", old.display()), e)),
    }
    #[cfg(unix)]
    {
        if std::fs::hard_link(exe, &old).is_err() {
            std::fs::copy(exe, &old).map_err(|e| err(&format!("keep {}", old.display()), e))?;
        }
        std::fs::rename(new, exe).map_err(|e| err(&format!("replace {}", exe.display()), e))?;
        if let Some(dir) = exe.parent()
            && let Ok(d) = std::fs::File::open(dir)
        {
            let _ = d.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::rename(exe, &old).map_err(|e| err(&format!("move {}", exe.display()), e))?;
        if let Err(e) = std::fs::rename(new, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(err(&format!("replace {}", exe.display()), e));
        }
    }
    Ok(old)
}

/// The command that upgrades with the needed rights, for messages.
pub fn elevated_command(exe: &Path) -> String {
    if cfg!(windows) {
        format!("\"{}\" upgrade", exe.display())
    } else {
        format!("sudo {} upgrade", exe.display())
    }
}

/// Run the new file: it must print `podbag <version>`.
fn check_runs(file: &Path, want: &Version) -> Result<(), String> {
    let out = std::process::Command::new(file)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run the downloaded program: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let expected = format!("podbag {want}");
    if out.status.success() && said == expected {
        Ok(())
    } else {
        Err(format!(
            "the downloaded program says {said:?}, not {expected:?}: nothing was changed"
        ))
    }
}

/// macOS: a valid signature with the virtualization entitlement (the VM needs it).
#[cfg(target_os = "macos")]
fn check_signature(file: &Path) -> Result<(), String> {
    let verify = std::process::Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(file)
        .output()
        .map_err(|e| format!("cannot run codesign: {e}"))?;
    if !verify.status.success() {
        return Err(format!(
            "the downloaded program's signature is not valid ({}): nothing was changed",
            String::from_utf8_lossy(&verify.stderr).trim()
        ));
    }
    let ent = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(file)
        .output()
        .map_err(|e| format!("cannot run codesign: {e}"))?;
    let text = String::from_utf8_lossy(&ent.stdout);
    if !text.contains("com.apple.security.virtualization") {
        return Err(
            "the downloaded program lacks the virtualization entitlement: nothing was changed"
                .into(),
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn check_signature(_file: &Path) -> Result<(), String> {
    Ok(())
}

/// What `podbag upgrade` was asked.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub check: bool,
    pub version: Option<String>,
    pub yes: bool,
    pub json: bool,
}

fn ask(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// `podbag upgrade`; its exit code.
pub fn upgrade_main(opts: Options) -> i32 {
    match upgrade(&opts) {
        Ok(code) => code,
        Err(Failure::Denied { dir, exe }) => {
            let fix = if cfg!(windows) {
                format!(
                    "run it in an administrator terminal: {}",
                    elevated_command(&exe)
                )
            } else {
                format!("run: {}", elevated_command(&exe))
            };
            eprintln!(
                "podbag: cannot write to {}: it needs administrator rights\n  fix:   {fix}",
                dir.display()
            );
            NEEDS_PERMISSION
        }
        Err(Failure::Error(e)) => {
            eprintln!("podbag: {e}");
            1
        }
    }
}

enum Failure {
    Denied { dir: PathBuf, exe: PathBuf },
    Error(String),
}

impl From<String> for Failure {
    fn from(e: String) -> Failure {
        Failure::Error(e)
    }
}

impl From<&str> for Failure {
    fn from(e: &str) -> Failure {
        Failure::Error(e.to_string())
    }
}

fn upgrade(opts: &Options) -> Result<i32, Failure> {
    if running_in_wsl_distro().is_some() {
        return Err(
            "this is the engine in podbag's WSL distribution: run `podbag upgrade` \
                    in Windows instead; the next command brings the new engine here"
                .to_string()
                .into(),
        );
    }
    let wanted = match &opts.version {
        Some(v) => Some(Version::parse(v).ok_or_else(|| format!("{v:?} is not a version"))?),
        None => None,
    };
    let now = current();
    let release = fetch_release(wanted.as_ref())?;
    if wanted.is_none() {
        save_check(&release.version);
    }
    let to = release.version.clone();
    if opts.check {
        let checked = Checked {
            current: now.to_string(),
            latest: to.to_string(),
            newer: to > now,
        };
        if opts.json {
            println!("{}", serde_json::to_string(&checked).unwrap_or_default());
        } else if checked.newer {
            println!("podbag {to} is available (this is {now}). Run `podbag upgrade`.");
        } else if wanted.is_some() {
            println!("podbag {to} is a release (this is {now}).");
        } else {
            println!("podbag {now} is up to date.");
        }
        return Ok(0);
    }
    if to == now {
        println!("podbag {now} is up to date.");
        return Ok(0);
    }
    // Going down only when asked for by version.
    if to < now && wanted.is_none() {
        println!("podbag {now} is up to date (the latest release is {to}).");
        return Ok(0);
    }
    let asset = this_asset().ok_or("there is no podbag release for this system")?;
    let asset_url = release
        .asset(&asset)
        .ok_or_else(|| format!("release v{to} has no {asset}"))?
        .to_string();
    let sums_url = release
        .asset("SHA256SUMS")
        .ok_or_else(|| format!("release v{to} has no SHA256SUMS: it cannot be checked"))?
        .to_string();
    let exe = program_file()?;
    if package_of(&exe).is_some() {
        return Err(format!(
            "this podbag ({}) comes with Podbag Desktop: update the app in Settings > \
             Updates, and the app updates its podbag too",
            exe.display()
        )
        .into());
    }
    if !opts.yes {
        let what = if to < now { "Go back" } else { "Upgrade" };
        let question = format!("{what} from podbag {now} to {to} ({})?", exe.display());
        if !interactive() {
            return Err(format!("{question} Add --yes to do it without a question.").into());
        }
        if !ask(&question) {
            println!("Nothing was changed.");
            return Ok(1);
        }
    }
    let mut staged = match stage(&exe) {
        Ok(s) => s,
        Err(StageError::Denied(dir)) => return Err(Failure::Denied { dir, exe }),
        Err(StageError::Other(e)) => return Err(e.into()),
    };
    eprintln!("Downloading {asset} (v{to})");
    let mut sums = Vec::new();
    download(&sums_url, &mut sums, 1 << 20)?;
    let want = checksum_for(&String::from_utf8_lossy(&sums), &asset)
        .ok_or_else(|| format!("SHA256SUMS has no line for {asset}"))?;
    let got = {
        let file = staged.file.as_mut().ok_or("the download file is closed")?;
        let mut w = std::io::BufWriter::new(&mut *file);
        let got = download(&asset_url, &mut w, MAX_ASSET)?;
        w.flush()
            .map_err(|e| format!("cannot write the download: {e}"))?;
        got
    };
    if got != want {
        return Err(format!(
            "the download of {asset} does not match SHA256SUMS (got {got}, want {want}): \
             nothing was changed"
        )
        .into());
    }
    if let Some(f) = staged.file.take() {
        f.sync_all()
            .map_err(|e| format!("cannot write the download: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged.path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", staged.path.display()))?;
    }
    check_signature(&staged.path)?;
    check_runs(&staged.path, &to)?;
    let old = replace(&exe, &staged.path)?;
    staged.keep = true;
    println!(
        "Upgraded podbag {now} to {to} ({}). The old one is {}.",
        exe.display(),
        old.display()
    );
    after_swap();
    Ok(0)
}

/// What the new program needs after the swap.
fn after_swap() {
    #[cfg(target_os = "macos")]
    crate::macos::cli::after_upgrade();
    #[cfg(target_os = "linux")]
    println!(
        "A running `podbag system service` ends when it is idle; the next client starts \
         the new one. Running containers keep running."
    );
    #[cfg(windows)]
    println!("The next command brings the new engine into podbag's WSL distribution.");
}

/// Whether this program's file was replaced since it started (an upgrade): long-lived
/// processes (`podbag system service`) end when idle, and the new one takes over.
#[cfg(unix)]
pub struct ProgramWatch {
    path: PathBuf,
    id: (u64, u64),
}

#[cfg(unix)]
impl ProgramWatch {
    pub fn new() -> Option<ProgramWatch> {
        use std::os::unix::fs::MetadataExt;
        let path = std::env::current_exe().ok()?;
        // Linux names a replaced file "<path> (deleted)".
        let text = path.to_string_lossy();
        let path = PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(&text));
        let running = if cfg!(target_os = "linux") {
            std::fs::metadata("/proc/self/exe").ok()?
        } else {
            std::fs::metadata(&path).ok()?
        };
        Some(ProgramWatch {
            path,
            id: (running.dev(), running.ino()),
        })
    }

    /// For tests: watch `path` as it is now.
    pub fn of(path: &Path) -> Option<ProgramWatch> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(ProgramWatch {
            path: path.to_path_buf(),
            id: (m.dev(), m.ino()),
        })
    }

    /// Another file is at the program's path now.
    pub fn replaced(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) != self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    #[test]
    fn versions_parse() {
        assert_eq!(v("v0.1.0"), v("0.1.0"));
        assert_eq!(v("1.2.3").to_string(), "1.2.3");
        assert_eq!(v("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
        assert_eq!(v("1.2.3+build.5"), v("1.2.3"));
        for bad in [
            "", "1", "1.2", "1.2.3.4", "a.b.c", "1.2.x", "1.2.3-", "v", "1..3", "-1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn versions_compare() {
        assert!(v("0.1.1") > v("0.1.0"));
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.2") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.9"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha"));
        assert!(v("1.0.0-alpha.1") > v("1.0.0-alpha"));
        assert_eq!(v("1.0.0").cmp(&v("v1.0.0")), std::cmp::Ordering::Equal);
    }

    #[test]
    fn each_platform_has_its_file() {
        assert_eq!(
            asset_name("linux", "x86_64").unwrap(),
            "podbag-x86_64-linux"
        );
        assert_eq!(
            asset_name("linux", "aarch64").unwrap(),
            "podbag-aarch64-linux"
        );
        assert_eq!(
            asset_name("macos", "aarch64").unwrap(),
            "podbag-aarch64-macos"
        );
        assert_eq!(
            asset_name("windows", "x86_64").unwrap(),
            "podbag-x86_64-windows.exe"
        );
        assert_eq!(
            asset_name("windows", "aarch64").unwrap(),
            "podbag-aarch64-windows.exe"
        );
        assert_eq!(
            asset_name("macos", "x86_64").unwrap(),
            "podbag-x86_64-macos"
        );
        assert_eq!(asset_name("freebsd", "x86_64"), None);
        assert!(this_asset().is_some());
    }

    #[test]
    fn checksums_are_found_by_name() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums =
            format!("{a}  podbag-x86_64-linux\n{b} *podbag-aarch64-macos\nxyz  podbag-short\n");
        assert_eq!(checksum_for(&sums, "podbag-x86_64-linux"), Some(a.clone()));
        assert_eq!(
            checksum_for(&sums, "podbag-aarch64-macos"),
            Some(b.to_lowercase())
        );
        assert_eq!(checksum_for(&sums, "podbag-short"), None);
        assert_eq!(checksum_for(&sums, "podbag-x86_64"), None);
        assert_eq!(checksum_for("", "x"), None);
    }

    #[test]
    fn only_https_except_local_tests() {
        assert!(allowed_url("https://api.github.com/x", false));
        assert!(!allowed_url("http://api.github.com/x", false));
        assert!(!allowed_url("http://127.0.0.1:8080/x", false));
        assert!(allowed_url("http://127.0.0.1:8080/x", true));
        assert!(!allowed_url("http://127.0.0.1.evil.com/x", true));
        assert!(!allowed_url("http://localhost:8080/x", true));
        assert!(!allowed_url("file:///etc/passwd", true));
    }

    #[test]
    fn a_release_from_the_api() {
        let r = parse_release(
            r#"{"tag_name": "v0.2.0", "assets": [
                {"name": "SHA256SUMS", "browser_download_url": "https://x/SHA256SUMS"},
                {"name": "podbag-aarch64-macos", "browser_download_url": "https://x/m"}]}"#,
        )
        .unwrap();
        assert_eq!(r.version, v("0.2.0"));
        assert_eq!(r.asset("podbag-aarch64-macos"), Some("https://x/m"));
        assert_eq!(r.asset("podbag-x86_64-linux"), None);
        assert!(parse_release(r#"{"tag_name": "latest"}"#).is_err());
        assert!(parse_release("{").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_new_file_replaces_the_program_and_the_old_one_stays() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("podbag");
        std::fs::write(&exe, "old").unwrap();
        let watch = ProgramWatch::of(&exe).unwrap();
        assert!(!watch.replaced());
        let new = dir.path().join(".podbag.upgrade-1");
        std::fs::write(&new, "new").unwrap();
        let old = replace(&exe, &new).unwrap();
        assert_eq!(old, dir.path().join("podbag.old"));
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "old");
        assert!(!new.exists());
        assert!(watch.replaced());
        // Again: the older .old goes.
        std::fs::write(&new, "newer").unwrap();
        replace(&exe, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "newer");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_without_write_permission_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root writes anywhere
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("podbag");
        std::fs::write(&exe, "old").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let got = stage(&exe);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(got, Err(StageError::Denied(d)) if d == dir.path()));
        // A staged file goes when dropped.
        let staged = stage(&exe).ok().unwrap();
        let path = staged.path.clone();
        assert!(path.exists());
        drop(staged);
        assert!(!path.exists());
    }

    #[test]
    fn packages_are_known_by_their_folder() {
        let p = |s: &str| package_of(Path::new(s));
        assert_eq!(p("/usr/lib/podbag-desktop/podbag"), Some(Package::Linux));
        assert_eq!(
            p("/usr/lib/podbag-desktop/podbag-desktop"),
            Some(Package::Linux)
        );
        assert_eq!(
            p("/Applications/podbag.app/Contents/MacOS/podbag"),
            Some(Package::MacApp("/Applications/podbag.app".into()))
        );
        assert_eq!(
            p("/Users/me/Apps/Podbag.APP/Contents/MacOS/podbag-desktop"),
            Some(Package::MacApp("/Users/me/Apps/Podbag.APP".into()))
        );
        for none in [
            "/usr/local/bin/podbag",
            "/usr/lib/podbag-desktop/sub/podbag",
            "/home/me/podbag/target/release/podbag",
            "/Applications/tools/Contents/MacOS/podbag",
            "podbag",
        ] {
            assert_eq!(p(none), None, "{none}");
        }
        let windows = p(r"C:\Program Files\Podbag Desktop\podbag.exe");
        if cfg!(windows) {
            assert_eq!(windows, Some(Package::Windows));
        } else {
            assert_eq!(windows, None);
        }
    }

    #[test]
    fn the_command_with_rights() {
        let cmd = elevated_command(Path::new("/usr/local/bin/podbag"));
        if cfg!(windows) {
            assert!(cmd.ends_with(" upgrade"));
        } else {
            assert_eq!(cmd, "sudo /usr/local/bin/podbag upgrade");
        }
    }
}
