//! podbag on Windows: every command runs in podbag's WSL2 distribution (see `crate::wsl`),
//! which podbag.exe sets up the first time it is needed and updates when its own version
//! changes. Experimental: built and unit-tested, not yet tried on a Windows machine.

use crate::wsl::{self, DISTRO, ENGINE};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The Linux engine for the distribution (`--features embed-linux`, PODBAG_LINUX_BIN).
#[cfg(feature = "embed-linux")]
static LINUX: &[u8] = include_bytes!(env!("PODBAG_LINUX_BIN"));
#[cfg(not(feature = "embed-linux"))]
static LINUX: &[u8] = &[];

const VERSION: &str = env!("CARGO_PKG_VERSION");

const NO_ENGINE: &str = "this podbag.exe carries no Linux engine for WSL: download a \
                         release build, or build with `--features embed-linux` and \
                         PODBAG_LINUX_BIN set to a static Linux podbag";

/// Run a podbag command in the distribution; its exit code.
pub fn forward(argv: &[String]) -> i32 {
    if let Err(e) = ensure() {
        eprintln!("podbag: {e}");
        return 125;
    }
    let mut cmd = wsl();
    cmd.args(["-d", DISTRO]);
    // The engine starts where the command was given, so relative paths mean the same.
    if let Ok(dir) = std::env::current_dir() {
        if wsl::wsl_path(&dir.to_string_lossy()).is_ok_and(|p| p.starts_with("/mnt/")) {
            cmd.arg("--cd").arg(&dir);
        }
    }
    cmd.args(["--exec", ENGINE]).args(argv);
    match cmd.status() {
        Ok(s) => s.code().unwrap_or(125),
        Err(e) => {
            eprintln!("podbag: cannot run wsl.exe: {e}");
            125
        }
    }
}

/// `podbag vm start|status|stop|rm`.
pub fn vm_command(args: &[String]) -> i32 {
    let done = match args.first().map(String::as_str) {
        Some("start") => ensure()
            .and_then(|()| run_quiet(&["-d", DISTRO, "--exec", ENGINE, "--version"]))
            .map(|()| println!("running")),
        Some("status") => status().map(|s| println!("{s}")),
        Some("stop") => run_quiet(&["--terminate", DISTRO]),
        Some("rm") => remove(),
        _ => {
            eprintln!("usage: podbag vm start|status|stop|rm");
            return 2;
        }
    };
    match done {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("podbag: {e}");
            1
        }
    }
}

fn status() -> Result<&'static str, String> {
    if !names(false)?.iter().any(|n| n == DISTRO) {
        return Ok("not set up");
    }
    Ok(if names(true)?.iter().any(|n| n == DISTRO) {
        "running"
    } else {
        "stopped"
    })
}

fn remove() -> Result<(), String> {
    if names(false)?.iter().any(|n| n == DISTRO) {
        run_quiet(&["--unregister", DISTRO])?;
    }
    let home = home();
    let _ = std::fs::remove_file(home.join("version"));
    let _ = std::fs::remove_dir_all(home.join("wsl"));
    println!("removed podbag's WSL distribution, with its images and containers");
    Ok(())
}

/// Set up the distribution, or bring its engine to this version.
fn ensure() -> Result<(), String> {
    let stamp = home().join("version");
    if !names(false)?.iter().any(|n| n == DISTRO) {
        return install(&stamp);
    }
    if std::fs::read_to_string(&stamp).ok().as_deref() != Some(VERSION) {
        return update(&stamp);
    }
    Ok(())
}

fn install(stamp: &Path) -> Result<(), String> {
    if LINUX.is_empty() {
        return Err(NO_ENGINE.into());
    }
    let home = home();
    let dir = home.join("wsl");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tar = home.join("rootfs.tar");
    let bytes = wsl::distro_tar(LINUX, VERSION).map_err(|e| e.to_string())?;
    std::fs::write(&tar, bytes).map_err(|e| format!("cannot write {}: {e}", tar.display()))?;
    eprintln!("podbag: setting up its WSL distribution (once)");
    let out = wsl()
        .arg("--import")
        .arg(DISTRO)
        .arg(&dir)
        .arg(&tar)
        .args(["--version", "2"])
        .output();
    let _ = std::fs::remove_file(&tar);
    let out = out.map_err(|e| format!("cannot run wsl.exe: {e}"))?;
    if !out.status.success() {
        return Err(format!("wsl --import failed: {}", text(&out)));
    }
    std::fs::write(stamp, VERSION).map_err(|e| format!("cannot write {}: {e}", stamp.display()))
}

fn update(stamp: &Path) -> Result<(), String> {
    if LINUX.is_empty() {
        return Err(NO_ENGINE.into());
    }
    let mut child = wsl()
        .args(["-d", DISTRO, "--exec", ENGINE, "__install-self", ENGINE])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run wsl.exe: {e}"))?;
    // Written and closed before waiting: the engine reads to the end first.
    let written = child.stdin.take().map(|mut i| i.write_all(LINUX));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if written.is_some_and(|w| w.is_err()) || !out.status.success() {
        return Err(format!(
            "cannot update the engine in podbag's WSL distribution ({}); `podbag vm rm` \
             removes the distribution, with its images and containers, and the next \
             command sets it up again",
            text(&out)
        ));
    }
    std::fs::write(stamp, VERSION).map_err(|e| format!("cannot write {}: {e}", stamp.display()))
}

/// The names of WSL distributions (`running`: of those running).
fn names(running: bool) -> Result<Vec<String>, String> {
    let mut c = wsl();
    c.arg("--list");
    if running {
        c.arg("--running");
    }
    let out = c.arg("--quiet").output().map_err(|e| {
        format!(
            "cannot run wsl.exe ({e}): podbag needs WSL 2; install it with `wsl --install` \
             in an administrator terminal, then restart Windows"
        )
    })?;
    // With no distribution at all, wsl.exe says so and fails.
    Ok(if out.status.success() {
        wsl::distro_names(&out.stdout)
    } else {
        Vec::new()
    })
}

fn wsl() -> Command {
    let mut c = Command::new("wsl.exe");
    c.env("WSL_UTF8", "1");
    c
}

fn run_quiet(args: &[&str]) -> Result<(), String> {
    let out = wsl()
        .args(args)
        .output()
        .map_err(|e| format!("cannot run wsl.exe: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("wsl.exe {}: {}", args.join(" "), text(&out)))
    }
}

fn text(out: &Output) -> String {
    let mut s = wsl::decode(&out.stderr);
    s.push(' ');
    s.push_str(&wsl::decode(&out.stdout));
    s.trim().to_string()
}

/// `%LOCALAPPDATA%\podbag`: the distribution's disk and the version of its engine.
fn home() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Local"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
        .join("podbag")
}
