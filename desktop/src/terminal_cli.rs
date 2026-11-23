//! The `podbag` command for terminals, made from the podbag that comes with the app. It
//! leads to the app's own podbag, so it changes when the app is updated.
//!
//! - macOS and Linux: the link `/usr/local/bin/podbag`. When that folder needs an
//!   administrator, macOS asks for the password and Linux asks through pkexec.
//! - Windows: the app's folder, added at the end of the user's `Path`.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where Install puts the command.
#[cfg(unix)]
pub const LINK_DIR: &str = "/usr/local/bin";

/// What a terminal runs for `podbag`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// `app`: this app's podbag (or another copy of the app's); `other`: a podbag that
    /// was installed on its own; `missing`: no podbag.
    pub state: &'static str,
    /// The podbag a terminal runs.
    pub path: Option<String>,
    /// Where Install puts the command: the link, or (Windows) the folder.
    pub target: String,
    /// Why Install cannot work now: `translocated` (macOS runs a copy of the app that
    /// moves; the app must be moved to Applications first).
    pub problem: Option<&'static str>,
}

/// What a terminal runs, when the first `podbag` it finds is `found`.
pub fn status(app_cli: &Path, found: Option<&Path>) -> Status {
    let own = app_cli.canonicalize().ok();
    let state = match found {
        None => "missing",
        Some(p) => {
            let real = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            if Some(&real) == own.as_ref() || podbag::upgrade::package_of(&real).is_some() {
                "app"
            } else {
                "other"
            }
        }
    };
    Status {
        state,
        path: found.map(|p| p.display().to_string()),
        target: target(app_cli).display().to_string(),
        problem: problem(own.as_deref().unwrap_or(app_cli)),
    }
}

/// What a terminal runs now.
pub fn current(app_cli: &Path) -> Status {
    status(app_cli, podbag::client::podbag_on_path().as_deref())
}

#[cfg(unix)]
fn target(_app_cli: &Path) -> PathBuf {
    Path::new(LINK_DIR).join("podbag")
}

#[cfg(windows)]
fn target(app_cli: &Path) -> PathBuf {
    app_cli.parent().unwrap_or(app_cli).to_path_buf()
}

/// macOS starts an app from a quarantined download as a moving copy: a link to it
/// would break.
fn problem(app_cli: &Path) -> Option<&'static str> {
    app_cli
        .to_string_lossy()
        .contains("/AppTranslocation/")
        .then_some("translocated")
}

/// Make the command. `prompt` is what the password dialog says (macOS). Ok(None): the
/// user did not give the password.
pub fn install(app_cli: &Path, prompt: &str) -> Result<Option<Status>, String> {
    let real = app_cli
        .canonicalize()
        .map_err(|e| format!("cannot find {}: {e}", app_cli.display()))?;
    if problem(&real).is_some() {
        return Err("move podbag.app to the Applications folder first".into());
    }
    if !make(&real, prompt)? {
        return Ok(None);
    }
    #[cfg(unix)]
    let found = Some(target(&real));
    // This app's own Path stays as it started; new terminals get the new one.
    #[cfg(windows)]
    let found = Some(real.clone());
    Ok(Some(status(&real, found.as_deref())))
}

/// `dir/podbag` as a link to `target`: a new link, or one in place of a link that leads
/// nowhere. A file or a link that works is never replaced.
#[cfg(unix)]
pub fn link(dir: &Path, target: &Path) -> std::io::Result<()> {
    let link = dir.join("podbag");
    let broken = link
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        && !link.exists();
    if broken {
        std::fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(target, &link)
}

/// The same as [`link`] in [`LINK_DIR`], for `sh -c` with the target as `$1`.
#[cfg(unix)]
fn link_script() -> String {
    format!(
        "set -e; mkdir -p {LINK_DIR}; l={LINK_DIR}/podbag; \
         if [ -L \"$l\" ] && [ ! -e \"$l\" ]; then rm -f \"$l\"; fi; ln -s \"$1\" \"$l\""
    )
}

#[cfg(unix)]
fn make(target: &Path, prompt: &str) -> Result<bool, String> {
    use std::io::ErrorKind;
    match link(Path::new(LINK_DIR), target) {
        Ok(()) => Ok(true),
        Err(e) if matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::NotFound) => {
            as_admin(target, prompt)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(format!("{LINK_DIR}/podbag already exists"))
        }
        Err(e) => Err(format!("cannot make {LINK_DIR}/podbag: {e}")),
    }
}

/// The password dialog of macOS, then the link as root.
#[cfg(target_os = "macos")]
fn as_admin(target: &Path, prompt: &str) -> Result<bool, String> {
    let out = Command::new("/usr/bin/osascript")
        .args(osascript_args(prompt))
        .arg(target)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot ask for the password: {e}"))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(true)
    } else if err.contains("(-128)") {
        Ok(false)
    } else {
        Err(err.trim().to_string())
    }
}

/// The script, the prompt, then the target goes last.
#[cfg(target_os = "macos")]
fn osascript_args(prompt: &str) -> Vec<String> {
    vec![
        "-e".into(),
        "on run argv".into(),
        "-e".into(),
        "do shell script \"/bin/sh -c \" & quoted form of (item 1 of argv) & \" sh \" & \
         quoted form of (item 3 of argv) with prompt (item 2 of argv) with administrator \
         privileges"
            .into(),
        "-e".into(),
        "end run".into(),
        link_script(),
        prompt.into(),
    ]
}

/// pkexec asks for the password, then the link is made as root.
#[cfg(all(unix, not(target_os = "macos")))]
fn as_admin(target: &Path, _prompt: &str) -> Result<bool, String> {
    let out = Command::new("pkexec")
        .args(["/bin/sh", "-c", &link_script(), "sh"])
        .arg(target)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start pkexec: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(true),
        // The user closed the password dialog.
        Some(126) => Ok(false),
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

/// Add the app's folder to the user's `Path` (the registry value as it is, so `%VAR%`
/// parts stay), then tell Windows, so new terminals get it.
#[cfg(windows)]
const PATH_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
    $dir = $env:PODBAG_CLI_DIR; \
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment'); \
    $path = [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames'); \
    $parts = @($path -split ';' | Where-Object { $_ }); \
    if ($parts -notcontains $dir) { $key.SetValue('Path', (@($parts) + $dir) -join ';', 'ExpandString') }; \
    $key.Close(); \
    [Environment]::SetEnvironmentVariable('PODBAG_PATH_CHANGED', '1', 'User'); \
    [Environment]::SetEnvironmentVariable('PODBAG_PATH_CHANGED', $null, 'User')";

#[cfg(windows)]
fn make(target: &Path, _prompt: &str) -> Result<bool, String> {
    use std::os::windows::process::CommandExt;
    let dir = target.parent().ok_or("podbag.exe has no folder")?;
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
        ])
        .args(["-Command", PATH_SCRIPT])
        .env("PODBAG_CLI_DIR", dir)
        .stdin(Stdio::null())
        // CREATE_NO_WINDOW
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("cannot start PowerShell: {e}"))?;
    if out.status.success() {
        Ok(true)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn exe(path: &Path) {
        std::fs::write(path, "#!/bin/sh\n").unwrap();
    }

    #[test]
    fn the_status_names_whose_podbag_a_terminal_runs() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("podbag.app/Contents/MacOS");
        std::fs::create_dir_all(&app).unwrap();
        let cli = app.join("podbag");
        exe(&cli);
        let other = dir.path().join("other-podbag");
        exe(&other);
        let link = dir.path().join("podbag");
        std::os::unix::fs::symlink(&cli, &link).unwrap();

        let s = status(&cli, None);
        assert_eq!((s.state, s.path, s.problem), ("missing", None, None));
        assert_eq!(s.target, "/usr/local/bin/podbag");
        assert_eq!(status(&cli, Some(&link)).state, "app");
        assert_eq!(status(&cli, Some(&cli)).state, "app");
        let s = status(&cli, Some(&other));
        assert_eq!(s.state, "other");
        assert_eq!(s.path.as_deref(), Some(other.to_str().unwrap()));
    }

    #[test]
    fn another_copy_of_the_app_counts_as_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let mut clis = Vec::new();
        for name in ["podbag.app", "Old podbag.app"] {
            let app = dir.path().join(name).join("Contents/MacOS");
            std::fs::create_dir_all(&app).unwrap();
            exe(&app.join("podbag"));
            clis.push(app.join("podbag"));
        }
        assert_eq!(status(&clis[0], Some(&clis[1])).state, "app");
    }

    #[test]
    fn a_moving_copy_of_the_app_cannot_be_linked() {
        let cli = Path::new(
            "/private/var/folders/x/AppTranslocation/1234/d/podbag.app/Contents/MacOS/podbag",
        );
        assert_eq!(problem(cli), Some("translocated"));
        assert_eq!(
            problem(Path::new("/Applications/podbag.app/Contents/MacOS/podbag")),
            None
        );
    }

    #[test]
    fn the_link_replaces_only_a_broken_link() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("cli");
        exe(&cli);
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();

        link(&bin, &cli).unwrap();
        assert_eq!(std::fs::read_link(bin.join("podbag")).unwrap(), cli);
        // A link that works stays.
        let err = link(&bin, &dir.path().join("elsewhere")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_link(bin.join("podbag")).unwrap(), cli);
        // A link to a moved app is replaced.
        std::fs::remove_file(bin.join("podbag")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), bin.join("podbag")).unwrap();
        link(&bin, &cli).unwrap();
        assert_eq!(std::fs::read_link(bin.join("podbag")).unwrap(), cli);
        // A file stays.
        std::fs::remove_file(bin.join("podbag")).unwrap();
        exe(&bin.join("podbag"));
        assert!(link(&bin, &cli).is_err());
        assert!(!bin.join("podbag").is_symlink());
        // No folder: the caller asks for an administrator, who makes it.
        let err = link(&dir.path().join("none"), &cli).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn the_admin_script_makes_the_same_link() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("my apps/cli");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        exe(&cli);
        let root = dir.path().join("root");
        let script = link_script().replace(LINK_DIR, root.join("usr/local/bin").to_str().unwrap());
        let run = |target: &Path| {
            Command::new("/bin/sh")
                .args(["-c", &script, "sh"])
                .arg(target)
                .status()
                .unwrap()
                .success()
        };
        let made = root.join("usr/local/bin/podbag");
        assert!(run(&cli), "it makes the folder and the link");
        assert_eq!(std::fs::read_link(&made).unwrap(), cli);
        assert!(
            !run(&dir.path().join("elsewhere")),
            "a link that works stays"
        );
        std::fs::remove_file(&made).unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), &made).unwrap();
        assert!(run(&cli), "a broken link is replaced");
        assert_eq!(std::fs::read_link(&made).unwrap(), cli);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_password_dialog_gets_the_script_prompt_and_target_as_arguments() {
        let args = osascript_args("Podbag \"Desktop\" wants it");
        assert_eq!(args[args.len() - 2], link_script());
        assert_eq!(args[args.len() - 1], "Podbag \"Desktop\" wants it");
        assert!(args[3].contains("quoted form of (item 3 of argv)"));
    }
}
