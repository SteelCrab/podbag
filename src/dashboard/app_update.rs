//! Podbag Desktop updates itself. It downloads its package from a release, checks it
//! against the release's `SHA256SUMS` and installs it the way the package came:
//!
//! - macOS: the new `podbag.app` is unpacked next to the old one. Its version and its
//!   signature are checked. Then it is renamed into the old one's place.
//! - Linux: `pkexec apt-get install` with the .deb, or `pkexec dnf install` with the .rpm.
//! - Windows: `msiexec` with the .msi, after the app has ended.
//!
//! Then the app starts again. The package carries the app's own `podbag`, so both change
//! together.

use crate::upgrade::{self, Package, Release, Version};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How this app was installed, so how it updates itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installer {
    /// macOS: this `.app` bundle, from the release's .zip.
    MacApp(PathBuf),
    /// Linux: the .deb (dpkg knows the package).
    Deb,
    /// Linux: the .rpm (rpm knows the package).
    Rpm,
    /// Windows: the .msi.
    Msi,
}

impl Installer {
    /// The installer of the app at `exe` (links resolved). None when the app did not come
    /// from a package (a build of its own), or when no package system knows it.
    pub fn detect(exe: &Path) -> Option<Installer> {
        match upgrade::package_of(exe)? {
            Package::MacApp(app) => Some(Installer::MacApp(app)),
            Package::Windows => Some(Installer::Msi),
            Package::Linux => {
                let deb = Command::new("dpkg-query")
                    .args(["-W", "-f", "${Status}", "podbag-desktop"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .is_ok_and(|o| {
                        o.status.success()
                            && String::from_utf8_lossy(&o.stdout).ends_with("ok installed")
                    });
                let rpm = || {
                    Command::new("rpm")
                        .args(["-q", "podbag-desktop"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                };
                if deb {
                    Some(Installer::Deb)
                } else if rpm() {
                    Some(Installer::Rpm)
                } else {
                    None
                }
            }
        }
    }

    /// The name the window gets: `macos-app`, `deb`, `rpm` or `msi`.
    pub fn name(&self) -> &'static str {
        match self {
            Installer::MacApp(_) => "macos-app",
            Installer::Deb => "deb",
            Installer::Rpm => "rpm",
            Installer::Msi => "msi",
        }
    }
}

/// The release file with the desktop package for `installer` on CPU `arch` (Rust's
/// names: `x86_64`, `aarch64`).
pub fn package_asset(installer: &Installer, arch: &str, release: &Release) -> Option<String> {
    let v = &release.version;
    let find = |prefix: String, suffix: String| {
        release
            .assets
            .iter()
            .map(|(n, _)| n)
            .find(|n| n.starts_with(&prefix) && n.ends_with(&suffix))
            .cloned()
    };
    let name = match (installer, arch) {
        (Installer::MacApp(_), "aarch64") => Some("podbag-desktop-aarch64-macos.zip".to_string()),
        (Installer::Msi, "x86_64" | "aarch64") => {
            Some(format!("podbag-desktop-{arch}-windows.msi"))
        }
        // podbag-desktop_0.3.0-1_amd64.deb
        (Installer::Deb, "x86_64") => find(format!("podbag-desktop_{v}-"), "_amd64.deb".into()),
        (Installer::Deb, "aarch64") => find(format!("podbag-desktop_{v}-"), "_arm64.deb".into()),
        // podbag-desktop-0.3.0-1.x86_64.rpm
        (Installer::Rpm, "x86_64" | "aarch64") => {
            find(format!("podbag-desktop-{v}-"), format!(".{arch}.rpm"))
        }
        _ => None,
    };
    name.filter(|n| release.asset(n).is_some())
}

/// What starts the new app once this one has ended (on Windows: the installer first).
#[derive(Debug)]
pub struct Restart(Command);

impl Restart {
    /// Run `program` with `args` once this process has ended.
    #[cfg(unix)]
    fn after_exit(program: &Path, args: &[&Path]) -> Restart {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(
                "pid=$1; shift; while kill -0 \"$pid\" 2>/dev/null; do sleep 0.2; done; \
                 exec \"$@\"",
            )
            .arg("sh")
            .arg(std::process::id().to_string())
            .arg(program)
            .args(args);
        Restart(cmd)
    }

    /// The command line, for tests and messages.
    pub fn argv(&self) -> Vec<OsString> {
        std::iter::once(self.0.get_program().to_os_string())
            .chain(self.0.get_args().map(|a| a.to_os_string()))
            .collect()
    }

    /// Start it, apart from this process: it outlives the app.
    pub fn spawn(mut self) -> Result<(), String> {
        self.0
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            self.0.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW
            self.0.creation_flags(0x0800_0000);
        }
        self.0
            .spawn()
            .map(drop)
            .map_err(|e| format!("cannot start the new app: {e}"))
    }
}

/// Download and install the package of release `wanted` (None: the latest). `current` is
/// this app's version: a release that is not newer is refused unless asked for by
/// version. Afterwards the app must end; [`Restart::spawn`] first.
pub fn install(
    installer: &Installer,
    wanted: Option<&Version>,
    current: &Version,
) -> Result<Restart, String> {
    let release = upgrade::fetch_release(wanted)?;
    let to = &release.version;
    if wanted.is_none() && to <= current {
        return Err(format!("Podbag Desktop {current} is up to date."));
    }
    if to == current {
        return Err(format!("Podbag Desktop {current} is already installed."));
    }
    let asset = package_asset(installer, std::env::consts::ARCH, &release)
        .ok_or_else(|| format!("release v{to} has no Podbag Desktop package for this system"))?;
    match installer {
        #[cfg(target_os = "macos")]
        Installer::MacApp(app) => mac::install(app, &release, &asset),
        #[cfg(target_os = "linux")]
        Installer::Deb | Installer::Rpm => linux::install(installer, &release, &asset),
        #[cfg(windows)]
        Installer::Msi => windows::install(&release, &asset),
        #[allow(unreachable_patterns)]
        _ => Err("this package does not belong to this system".into()),
    }
}

/// Whether an error means "not allowed to write here".
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
        || e.kind() == std::io::ErrorKind::ReadOnlyFilesystem
}

/// The new `podbag.app` must start the app, say `version` and carry a valid signature.
#[cfg(target_os = "macos")]
pub fn check_mac_app(app: &Path, version: &Version) -> Result<(), String> {
    if !app.join("Contents/MacOS/podbag-desktop").is_file() {
        return Err(format!("{} has no podbag-desktop", app.display()));
    }
    let out = Command::new("plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .map_err(|e| format!("cannot run plutil: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || Version::parse(&said).as_ref() != Some(version) {
        return Err(format!(
            "the downloaded app says version {said:?}, not {version:?}: nothing was changed",
            version = version.to_string()
        ));
    }
    let verify = Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .output()
        .map_err(|e| format!("cannot run codesign: {e}"))?;
    if !verify.status.success() {
        return Err(format!(
            "the downloaded app's signature is not valid ({}): nothing was changed",
            String::from_utf8_lossy(&verify.stderr).trim()
        ));
    }
    Ok(())
}

/// Put `new_app` in `app`'s place; the old bundle moves to `old`. When the second rename
/// fails, the old bundle goes back.
pub fn swap_app(app: &Path, new_app: &Path, old: &Path) -> Result<(), String> {
    std::fs::rename(app, old).map_err(|e| format!("cannot move {}: {e}", app.display()))?;
    if let Err(e) = std::fs::rename(new_app, app) {
        let _ = std::fs::rename(old, app);
        return Err(format!("cannot put the new app at {}: {e}", app.display()));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;

    pub fn install(app: &Path, release: &Release, asset: &str) -> Result<Restart, String> {
        let parent = app.parent().ok_or("the app has no folder")?;
        // Next to the app: the same disk, so the bundles are renamed, not copied.
        let stage = tempfile::Builder::new()
            .prefix(".podbag-update-")
            .tempdir_in(parent)
            .map_err(|e| {
                if denied(&e) {
                    format!(
                        "cannot write to {}: move podbag.app to a folder you can write to, \
                         or install the new version from the releases page",
                        parent.display()
                    )
                } else {
                    format!("{}: {e}", parent.display())
                }
            })?;
        let zip = stage.path().join(asset);
        upgrade::download_checked(release, asset, &zip)?;
        let unpacked = stage.path().join("new");
        let out = Command::new("ditto")
            .args(["-x", "-k"])
            .arg(&zip)
            .arg(&unpacked)
            .output()
            .map_err(|e| format!("cannot run ditto: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "cannot unpack {asset}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let new_app = unpacked.join("podbag.app");
        check_mac_app(&new_app, &release.version)?;
        swap_app(app, &new_app, &stage.path().join("old.app"))?;
        // The old bundle and the .zip go with `stage`.
        Ok(Restart::after_exit(Path::new("/usr/bin/open"), &[app]))
    }
}

/// The command that installs a Linux package with administrator rights.
pub fn linux_install_command(installer: &Installer, file: &Path, dnf: bool) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec!["pkexec".into()];
    match installer {
        Installer::Deb => argv.extend(["apt-get", "install", "-y"].map(OsString::from)),
        _ if dnf => argv.extend(["dnf", "install", "-y"].map(OsString::from)),
        _ => argv.extend(["rpm", "-U"].map(OsString::from)),
    }
    argv.push(file.as_os_str().to_os_string());
    argv
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    fn found(name: &str) -> bool {
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .chain(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from))
            .any(|d| d.join(name).is_file())
    }

    pub fn install(
        installer: &Installer,
        release: &Release,
        asset: &str,
    ) -> Result<Restart, String> {
        use std::os::unix::fs::PermissionsExt;
        if !found("pkexec") {
            return Err(
                "pkexec is missing, so the app cannot ask for the administrator password: \
                 install the new package from the releases page"
                    .into(),
            );
        }
        let dir = tempfile::Builder::new()
            .prefix("podbag-desktop-update-")
            .tempdir()
            .map_err(|e| format!("cannot make a folder for the download: {e}"))?;
        let file = dir.path().join(asset);
        upgrade::download_checked(release, asset, &file)?;
        // apt reads the file as its own user too.
        let _ = std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644));
        let argv = linux_install_command(installer, &file, found("dnf"));
        let out = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run pkexec: {e}"))?;
        match out.status.code() {
            Some(0) => {}
            Some(126) => {
                return Err("the administrator password was not given: nothing was changed".into());
            }
            Some(127) => {
                return Err(
                    "pkexec could not ask for the administrator password: nothing was changed"
                        .into(),
                );
            }
            _ => {
                let text = String::from_utf8_lossy(&out.stderr);
                let tail: Vec<&str> = text.lines().rev().take(5).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                return Err(format!(
                    "the package was not installed:\n{}",
                    tail.join("\n")
                ));
            }
        }
        let exe = Path::new("/usr/lib/podbag-desktop/podbag-desktop");
        Ok(Restart::after_exit(exe, &[]))
    }
}

/// The PowerShell script that waits for the app (`pid`) to end, runs the installer and
/// starts the new app. Paths are quoted for PowerShell; the .msi path also for msiexec.
pub fn windows_script(pid: u32, msi: &Path, exe: &Path, dir: &Path) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let msi = q(&format!("\"{}\"", msi.display()));
    let exe = q(&exe.display().to_string());
    let dir = q(&dir.display().to_string());
    format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; \
         $p = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @('/i', {msi}, '/passive', '/norestart'); \
         if ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) {{ Start-Process -FilePath {exe} }}; \
         Remove-Item -LiteralPath {dir} -Recurse -Force -ErrorAction SilentlyContinue"
    )
}

#[cfg(windows)]
mod windows {
    use super::*;

    pub fn install(release: &Release, asset: &str) -> Result<Restart, String> {
        let dir =
            std::env::temp_dir().join(format!("podbag-desktop-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let msi = dir.join(asset);
        if let Err(e) = upgrade::download_checked(release, asset, &msi) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        let exe = std::env::current_exe().map_err(|e| format!("cannot find this app: {e}"))?;
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
        ])
        .arg(windows_script(std::process::id(), &msi, &exe, &dir));
        Ok(Restart(cmd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, names: &[&str]) -> Release {
        Release {
            version: Version::parse(tag).unwrap(),
            assets: names
                .iter()
                .map(|n| (n.to_string(), format!("https://x/{n}")))
                .collect(),
        }
    }

    const NAMES: &[&str] = &[
        "SHA256SUMS",
        "podbag-aarch64-macos",
        "podbag-desktop-0.4.0-1.aarch64.rpm",
        "podbag-desktop-0.4.0-1.x86_64.rpm",
        "podbag-desktop-aarch64-macos.zip",
        "podbag-desktop-aarch64-windows.msi",
        "podbag-desktop-x86_64-windows.msi",
        "podbag-desktop_0.4.0-1_amd64.deb",
        "podbag-desktop_0.4.0-1_arm64.deb",
    ];

    #[test]
    fn each_installer_finds_its_package() {
        let r = release("v0.4.0", NAMES);
        let mac = Installer::MacApp("/Applications/podbag.app".into());
        let got = |i: &Installer, arch: &str| package_asset(i, arch, &r);
        assert_eq!(
            got(&mac, "aarch64").as_deref(),
            Some("podbag-desktop-aarch64-macos.zip")
        );
        assert_eq!(got(&mac, "x86_64"), None);
        assert_eq!(
            got(&Installer::Deb, "x86_64").as_deref(),
            Some("podbag-desktop_0.4.0-1_amd64.deb")
        );
        assert_eq!(
            got(&Installer::Deb, "aarch64").as_deref(),
            Some("podbag-desktop_0.4.0-1_arm64.deb")
        );
        assert_eq!(
            got(&Installer::Rpm, "x86_64").as_deref(),
            Some("podbag-desktop-0.4.0-1.x86_64.rpm")
        );
        assert_eq!(
            got(&Installer::Rpm, "aarch64").as_deref(),
            Some("podbag-desktop-0.4.0-1.aarch64.rpm")
        );
        assert_eq!(
            got(&Installer::Msi, "aarch64").as_deref(),
            Some("podbag-desktop-aarch64-windows.msi")
        );
        assert_eq!(got(&Installer::Deb, "riscv64"), None);
    }

    #[test]
    fn a_package_of_another_version_or_a_missing_one_is_not_taken() {
        // A .deb of another version in the release is not this release's package.
        let r = release("v0.4.1", NAMES);
        assert_eq!(package_asset(&Installer::Deb, "x86_64", &r), None);
        assert_eq!(package_asset(&Installer::Rpm, "x86_64", &r), None);
        let r = release("v0.4.0", &["SHA256SUMS", "podbag-aarch64-macos"]);
        let mac = Installer::MacApp("/Applications/podbag.app".into());
        assert_eq!(package_asset(&mac, "aarch64", &r), None);
        assert_eq!(package_asset(&Installer::Msi, "x86_64", &r), None);
    }

    #[test]
    fn installers_have_names() {
        let names: Vec<_> = [
            Installer::MacApp("/a.app".into()),
            Installer::Deb,
            Installer::Rpm,
            Installer::Msi,
        ]
        .iter()
        .map(Installer::name)
        .collect();
        assert_eq!(names, ["macos-app", "deb", "rpm", "msi"]);
        assert_eq!(Installer::detect(Path::new("/usr/local/bin/podbag")), None);
        assert_eq!(
            Installer::detect(Path::new(
                "/Applications/podbag.app/Contents/MacOS/podbag-desktop"
            )),
            Some(Installer::MacApp("/Applications/podbag.app".into()))
        );
    }

    #[test]
    fn linux_packages_install_with_pkexec() {
        let f = Path::new("/tmp/u/podbag-desktop_0.4.0-1_amd64.deb");
        let s = |v: Vec<OsString>| {
            v.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            s(linux_install_command(&Installer::Deb, f, true)),
            "pkexec apt-get install -y /tmp/u/podbag-desktop_0.4.0-1_amd64.deb"
        );
        let f = Path::new("/tmp/u/p.rpm");
        assert_eq!(
            s(linux_install_command(&Installer::Rpm, f, true)),
            "pkexec dnf install -y /tmp/u/p.rpm"
        );
        assert_eq!(
            s(linux_install_command(&Installer::Rpm, f, false)),
            "pkexec rpm -U /tmp/u/p.rpm"
        );
    }

    #[test]
    fn the_windows_script_quotes_paths() {
        let s = windows_script(
            42,
            Path::new(r"C:\Users\Jo O'Neil\AppData\Local\Temp\u\podbag-desktop-x86_64-windows.msi"),
            Path::new(r"C:\Program Files\Podbag Desktop\podbag-desktop.exe"),
            Path::new(r"C:\Users\Jo O'Neil\AppData\Local\Temp\u"),
        );
        assert!(s.starts_with("Wait-Process -Id 42 "), "{s}");
        assert!(
            s.contains(r#"'/i', '"C:\Users\Jo O''Neil\AppData\Local\Temp\u\podbag-desktop-x86_64-windows.msi"', '/passive'"#),
            "{s}"
        );
        assert!(
            s.contains(
                r"Start-Process -FilePath 'C:\Program Files\Podbag Desktop\podbag-desktop.exe'"
            ),
            "{s}"
        );
        assert!(
            s.contains(r"Remove-Item -LiteralPath 'C:\Users\Jo O''Neil\AppData\Local\Temp\u' "),
            "{s}"
        );
    }

    #[test]
    fn the_new_app_takes_the_old_ones_place() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("podbag.app");
        let new_app = dir.path().join("stage/new/podbag.app");
        let old = dir.path().join("stage/old.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(app.join("Contents/v"), "old").unwrap();
        std::fs::create_dir_all(new_app.join("Contents")).unwrap();
        std::fs::write(new_app.join("Contents/v"), "new").unwrap();
        swap_app(&app, &new_app, &old).unwrap();
        assert_eq!(
            std::fs::read_to_string(app.join("Contents/v")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(old.join("Contents/v")).unwrap(),
            "old"
        );
        // When the new app is missing, the old one stays where it was.
        let gone = dir.path().join("stage/none.app");
        let old2 = dir.path().join("stage/old2.app");
        assert!(swap_app(&app, &gone, &old2).is_err());
        assert_eq!(
            std::fs::read_to_string(app.join("Contents/v")).unwrap(),
            "new"
        );
        assert!(!old2.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_restart_waits_for_this_process() {
        let r = Restart::after_exit(
            Path::new("/usr/bin/open"),
            &[Path::new("/Applications/podbag.app")],
        );
        let argv: Vec<String> = r
            .argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[3], "sh");
        assert_eq!(argv[4], std::process::id().to_string());
        assert_eq!(&argv[5..], ["/usr/bin/open", "/Applications/podbag.app"]);
    }

    /// A small signed `podbag.app` that says `version`.
    #[cfg(target_os = "macos")]
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
            .unwrap()
            .success();
        assert!(ok, "codesign");
        app
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_downloaded_mac_app_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let app = fake_app(dir.path(), "0.4.0");
        let v = |s: &str| Version::parse(s).unwrap();
        check_mac_app(&app, &v("0.4.0")).unwrap();
        let err = check_mac_app(&app, &v("0.4.1")).unwrap_err();
        assert!(err.contains("\"0.4.0\""), "{err}");
        // A changed file breaks the signature.
        std::fs::write(app.join("Contents/MacOS/extra"), "x").unwrap();
        let err = check_mac_app(&app, &v("0.4.0")).unwrap_err();
        assert!(err.contains("signature"), "{err}");
        assert!(check_mac_app(&dir.path().join("none.app"), &v("0.4.0")).is_err());
    }
}
