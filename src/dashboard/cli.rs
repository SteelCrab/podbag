//! What the app asks the `podbag` program: the VM's state and settings, the version, the
//! update check and the upgrade. No Tauri here, so it is tested with a fake program.

use serde::Serialize;
use serde_json::Value;
use std::path::Path;

/// How old the saved update check may be before the window checks again.
pub const CHECK_EVERY_SECS: u64 = 24 * 3600;

/// The exit code of `podbag upgrade` when its folder needs administrator rights.
pub const NEEDS_ADMIN: i32 = 77;

/// What a run of the program gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// The error text: stderr without `podbag: `, else stdout, else the exit code.
    fn error(&self) -> String {
        let err = self.stderr.trim();
        let text = if err.is_empty() {
            self.stdout.trim()
        } else {
            err
        };
        if text.is_empty() {
            format!("podbag ended with {}", self.code)
        } else {
            text.lines()
                .map(|l| l.trim_start_matches("podbag: "))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// Run the program with `args`; stdin is empty, so it never waits for an answer.
pub fn run(podbag: &Path, args: &[&str]) -> Result<Ran, String> {
    let mut cmd = std::process::Command::new(podbag);
    cmd.args(args).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("{}: {e}", podbag.display()))?;
    Ok(Ran {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn run_ok(podbag: &Path, args: &[&str]) -> Result<String, String> {
    let ran = run(podbag, args)?;
    if ran.code == 0 {
        Ok(ran.stdout)
    } else {
        Err(ran.error())
    }
}

/// `podbag vm status --json`: it reads files only and wakes nothing.
pub fn vm_status(podbag: &Path) -> Result<Value, String> {
    let out = run_ok(podbag, &["vm", "status", "--json"])?;
    serde_json::from_str(out.trim()).map_err(|e| format!("podbag vm status --json: {e}"))
}

/// New VM settings; what is None stays as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VmSettings {
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    /// 0: never power off.
    pub idle_seconds: Option<u64>,
    /// Restart the VM now (running containers stop: the window asked first).
    pub restart: bool,
}

/// The arguments of `podbag vm set` for `s`.
pub fn vm_set_args(s: &VmSettings) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = vec!["vm".into(), "set".into()];
    if let Some(n) = s.cpus {
        args.extend(["--cpus".into(), n.to_string()]);
    }
    if let Some(m) = s.memory_mib {
        args.extend(["--memory".into(), format!("{m}M")]);
    }
    if let Some(i) = s.idle_seconds {
        let idle = if i == 0 { "0".into() } else { format!("{i}s") };
        args.extend(["--idle".into(), idle]);
    }
    if args.len() == 2 {
        return Err("nothing to set".into());
    }
    if s.restart {
        args.extend(["--restart".into(), "--yes".into()]);
    }
    Ok(args)
}

/// `podbag vm set …`: what it printed.
pub fn vm_set(podbag: &Path, s: &VmSettings) -> Result<String, String> {
    let args = vm_set_args(s)?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ok(podbag, &args).map(|out| out.trim().to_string())
}

/// `podbag vm start`, `stop` or `restart` (with `--yes`: the window asked first).
pub fn vm_action(podbag: &Path, action: &str) -> Result<(), String> {
    let args: &[&str] = match action {
        "start" => &["vm", "start"],
        "stop" => &["vm", "stop"],
        "restart" => &["vm", "restart", "--yes"],
        _ => return Err(format!("unknown action {action}")),
    };
    run_ok(podbag, args).map(drop)
}

/// `podbag --version`: `0.1.0` from `podbag 0.1.0`.
pub fn engine_version(podbag: &Path) -> Option<String> {
    let out = run_ok(podbag, &["--version"]).ok()?;
    let v = out.split_whitespace().nth(1)?.trim_start_matches('v');
    crate::upgrade::Version::parse(v).map(|v| v.to_string())
}

/// `latest` is newer than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    use crate::upgrade::Version;
    match (Version::parse(latest), Version::parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// The last check in `update.json`: when (Unix seconds) and the latest version.
pub fn saved_check(file: &Path) -> Option<(u64, String)> {
    let v: Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    Some((v["checked"].as_u64()?, v["latest"].as_str()?.to_string()))
}

/// Whether the window should check on its own: no check yet, or the last one is a day
/// old.
pub fn update_due(file: &Path, now: u64) -> bool {
    saved_check(file).is_none_or(|(at, _)| now.saturating_sub(at) >= CHECK_EVERY_SECS)
}

/// What the Updates section shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Updates {
    /// The engine's version (`podbag --version`).
    pub engine: Option<String>,
    /// This app's version.
    pub app: String,
    pub latest: Option<String>,
    pub newer: bool,
    /// When the last check was (Unix seconds).
    pub checked: Option<u64>,
    pub error: Option<String>,
    /// How the app updates itself (`macos-app`, `deb`, `rpm`, `msi`); None: it cannot.
    pub installer: Option<String>,
    /// A `podbag` for the terminal that the app's package does not update.
    pub terminal: Option<String>,
    pub terminal_version: Option<String>,
    /// The app (with its own `podbag`) is older than the latest release.
    pub app_newer: bool,
    /// The `podbag` that Update upgrades (see [`cli_to_upgrade`]) is older.
    pub cli_newer: bool,
}

/// When to ask the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// Only read the last check.
    Never,
    /// When the last check is a day old (the window opened).
    IfDue,
    /// Now (the user asked).
    Now,
}

/// The versions and the last check; with `check`, `podbag upgrade --check --json` first
/// (it saves its result in `file`).
pub fn updates(podbag: &Path, file: Option<&Path>, check: Check, now: u64) -> Updates {
    let mut u = Updates {
        engine: engine_version(podbag),
        app: env!("CARGO_PKG_VERSION").to_string(),
        ..Updates::default()
    };
    let due = file.is_none_or(|f| update_due(f, now));
    if check == Check::Now || (check == Check::IfDue && due) {
        match run_ok(podbag, &["upgrade", "--check", "--json"])
            .and_then(|out| serde_json::from_str::<Value>(out.trim()).map_err(|e| e.to_string()))
        {
            Ok(v) => {
                u.latest = v["latest"].as_str().map(str::to_string);
                u.checked = Some(now);
                if u.engine.is_none() {
                    u.engine = v["current"].as_str().map(str::to_string);
                }
            }
            Err(e) => u.error = Some(e),
        }
    }
    if u.latest.is_none()
        && let Some((at, latest)) = file.and_then(saved_check)
    {
        u.latest = Some(latest);
        u.checked = Some(at);
    }
    u.newer = match (&u.latest, &u.engine) {
        (Some(l), Some(c)) => is_newer(l, c),
        _ => false,
    };
    u
}

/// Which `podbag` Update upgrades with `podbag upgrade`. A packaged app updates its own
/// `podbag` with the app, so only a separate `terminal` one; otherwise the app's.
pub fn cli_to_upgrade(
    app_cli: &Path,
    packaged: bool,
    terminal: Option<&Path>,
) -> Option<std::path::PathBuf> {
    if packaged {
        terminal.map(Path::to_path_buf)
    } else {
        Some(app_cli.to_path_buf())
    }
}

/// Fill in what Update would do: `installer` is how the app updates itself (None: it
/// cannot), `terminal` a separate `podbag` for the terminal.
pub fn plan(u: &mut Updates, installer: Option<&str>, terminal: Option<&Path>) {
    let older = |v: Option<&str>| match (&u.latest, v) {
        (Some(l), Some(c)) => is_newer(l, c),
        _ => false,
    };
    u.installer = installer.map(str::to_string);
    u.terminal = terminal.map(|t| t.display().to_string());
    u.terminal_version = terminal.and_then(engine_version);
    if installer.is_some() {
        u.app_newer = older(Some(&u.app)) || older(u.engine.as_deref());
        u.cli_newer = older(u.terminal_version.as_deref());
    } else {
        u.app_newer = false;
        u.cli_newer = older(u.engine.as_deref());
    }
    u.newer = u.app_newer || u.cli_newer;
}

/// What `podbag upgrade --yes` did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Upgraded {
    pub ok: bool,
    pub code: i32,
    /// What it printed (stdout, then stderr).
    pub output: String,
    /// The folder needs administrator rights: run `command` by hand.
    pub needs_admin: bool,
    pub command: Option<String>,
}

/// The command to upgrade with administrator rights.
pub fn admin_command(podbag: &Path) -> String {
    let path = podbag.display().to_string();
    let quoted = if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path
    };
    if cfg!(windows) {
        format!("{quoted} upgrade")
    } else {
        format!("sudo {quoted} upgrade")
    }
}

/// `podbag upgrade --yes`.
pub fn upgrade(podbag: &Path) -> Upgraded {
    upgrade_to(podbag, None)
}

/// `podbag upgrade --yes`, with `--version` when given (the app's new version, so the
/// two match).
pub fn upgrade_to(podbag: &Path, version: Option<&str>) -> Upgraded {
    let mut args = vec!["upgrade", "--yes"];
    if let Some(v) = version {
        args.extend(["--version", v]);
    }
    match run(podbag, &args) {
        Ok(ran) => {
            let output = format!("{}\n{}", ran.stdout.trim(), ran.stderr.trim())
                .trim()
                .to_string();
            let needs_admin = ran.code == NEEDS_ADMIN;
            Upgraded {
                ok: ran.code == 0,
                code: ran.code,
                output,
                needs_admin,
                command: needs_admin.then(|| admin_command(podbag)),
            }
        }
        Err(e) => Upgraded {
            code: -1,
            output: e,
            ..Upgraded::default()
        },
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fake `podbag`: it writes its arguments to `args.log` and answers from a table.
    struct Fake {
        dir: tempfile::TempDir,
        bin: PathBuf,
    }

    impl Fake {
        fn new(body: &str) -> Fake {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("podbag");
            let log = dir.path().join("args.log");
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$*\" in\n{body}\n*) echo \"podbag: unknown $*\" >&2; exit 2;;\nesac\n",
                log.display()
            );
            std::fs::write(&bin, script).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            Fake { dir, bin }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("args.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    const STATUS: &str = r#"{"platform":"macos","state":"running","resourcesEditable":true,"inUse":{"cpus":4,"memoryMiB":2048,"idleSeconds":60},"containers":3}"#;

    #[test]
    fn vm_status_is_the_programs_json() {
        let fake = Fake::new(&format!("'vm status --json') echo '{STATUS}';;"));
        let v = vm_status(&fake.bin).unwrap();
        assert_eq!(v["state"], "running");
        assert_eq!(v["inUse"]["memoryMiB"], 2048);
        assert_eq!(v["containers"], 3);
        assert_eq!(fake.calls(), ["vm status --json"]);
        let broken = Fake::new("'vm status --json') echo 'podbag: no home' >&2; exit 1;;");
        assert_eq!(vm_status(&broken.bin).unwrap_err(), "no home");
    }

    #[test]
    fn vm_set_sends_what_changed() {
        let fake = Fake::new("vm\\ set*) echo 'Saved: 6 CPUs, 8 GiB memory.';;");
        let s = VmSettings {
            cpus: Some(6),
            memory_mib: Some(8192),
            idle_seconds: Some(0),
            restart: false,
        };
        assert_eq!(
            vm_set(&fake.bin, &s).unwrap(),
            "Saved: 6 CPUs, 8 GiB memory."
        );
        let s = VmSettings {
            idle_seconds: Some(300),
            restart: true,
            ..VmSettings::default()
        };
        vm_set(&fake.bin, &s).unwrap();
        assert_eq!(
            fake.calls(),
            [
                "vm set --cpus 6 --memory 8192M --idle 0",
                "vm set --idle 300s --restart --yes"
            ]
        );
        assert!(vm_set(&fake.bin, &VmSettings::default()).is_err());
        assert_eq!(fake.calls().len(), 2, "nothing to set runs nothing");
    }

    #[test]
    fn vm_set_errors_come_back() {
        let fake = Fake::new(
            "vm\\ set*) echo 'podbag: --cpus: 64 is more than this Mac has (10)' >&2; exit 1;;",
        );
        let s = VmSettings {
            cpus: Some(64),
            ..VmSettings::default()
        };
        assert_eq!(
            vm_set(&fake.bin, &s).unwrap_err(),
            "--cpus: 64 is more than this Mac has (10)"
        );
    }

    #[test]
    fn vm_actions_restart_with_yes() {
        let fake = Fake::new("vm\\ *) echo running;;");
        vm_action(&fake.bin, "start").unwrap();
        vm_action(&fake.bin, "restart").unwrap();
        vm_action(&fake.bin, "stop").unwrap();
        assert!(vm_action(&fake.bin, "rm").is_err());
        assert_eq!(fake.calls(), ["vm start", "vm restart --yes", "vm stop"]);
    }

    #[test]
    fn the_version_comes_from_the_program() {
        let fake = Fake::new("--version) echo 'podbag 0.3.1';;");
        assert_eq!(engine_version(&fake.bin).as_deref(), Some("0.3.1"));
        assert_eq!(engine_version(Path::new("/nonexistent/podbag")), None);
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("x", "0.1.0"));
    }

    #[test]
    fn the_check_runs_at_most_once_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("update.json");
        let now = 1_800_000_000;
        assert!(update_due(&file, now), "no check yet");
        std::fs::write(
            &file,
            format!(r#"{{"checked": {}, "latest": "0.2.0"}}"#, now - 3600),
        )
        .unwrap();
        assert!(!update_due(&file, now));
        assert!(update_due(&file, now + CHECK_EVERY_SECS));
        std::fs::write(&file, "not json").unwrap();
        assert!(update_due(&file, now));
    }

    #[test]
    fn updates_ask_the_network_only_when_due_or_asked() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("update.json");
        let now = 1_800_000_000;
        let fake = Fake::new(
            "--version) echo 'podbag 0.1.0';;\n\
             'upgrade --check --json') echo '{\"current\":\"0.1.0\",\"latest\":\"0.2.0\",\"newer\":true}';;",
        );
        // Due (no check yet): it checks.
        let u = updates(&fake.bin, Some(&file), Check::IfDue, now);
        assert_eq!(u.engine.as_deref(), Some("0.1.0"));
        assert_eq!(u.latest.as_deref(), Some("0.2.0"));
        assert!(u.newer);
        assert_eq!(u.app, env!("CARGO_PKG_VERSION"));
        // The program saves its result; here the test does.
        std::fs::write(&file, format!(r#"{{"checked": {now}, "latest": "0.2.0"}}"#)).unwrap();
        let u = updates(&fake.bin, Some(&file), Check::IfDue, now + 60);
        assert_eq!((u.latest.as_deref(), u.checked), (Some("0.2.0"), Some(now)));
        let u = updates(
            &fake.bin,
            Some(&file),
            Check::Never,
            now + 2 * CHECK_EVERY_SECS,
        );
        assert!(u.newer);
        updates(&fake.bin, Some(&file), Check::Now, now + 120);
        let checks = fake
            .calls()
            .iter()
            .filter(|c| c.starts_with("upgrade"))
            .count();
        assert_eq!(checks, 2, "{:?}", fake.calls());
    }

    #[test]
    fn a_failed_check_is_an_error_not_a_panic() {
        let fake = Fake::new(
            "--version) echo 'podbag 0.1.0';;\n\
             upgrade*) echo 'podbag: cannot reach github.com' >&2; exit 1;;",
        );
        let u = updates(&fake.bin, None, Check::Now, 1);
        assert_eq!(u.error.as_deref(), Some("cannot reach github.com"));
        assert!(!u.newer);
    }

    #[test]
    fn an_upgrade_that_needs_admin_rights_gives_the_command() {
        let fake = Fake::new(
            "'upgrade --yes') echo 'podbag: cannot write to /usr/local/bin: it needs administrator rights' >&2; exit 77;;",
        );
        let u = upgrade(&fake.bin);
        assert!(!u.ok && u.needs_admin);
        assert_eq!(u.code, 77);
        assert_eq!(
            u.command,
            Some(format!("sudo {} upgrade", fake.bin.display()))
        );
        assert!(u.output.contains("administrator rights"));
        let fine = Fake::new("'upgrade --yes') echo 'podbag 0.2.0 is installed';;");
        let u = upgrade(&fine.bin);
        assert!(u.ok && !u.needs_admin && u.command.is_none());
        assert_eq!(u.output, "podbag 0.2.0 is installed");
        let other = Fake::new("'upgrade --yes') echo 'podbag: checksum mismatch' >&2; exit 1;;");
        let u = upgrade(&other.bin);
        assert!(!u.ok && !u.needs_admin && u.command.is_none());
    }

    #[test]
    fn an_upgrade_to_a_version_says_it() {
        let fake = Fake::new("'upgrade --yes --version 0.4.0') echo 'Upgraded';;");
        let u = upgrade_to(&fake.bin, Some("0.4.0"));
        assert!(u.ok, "{u:?}");
        assert_eq!(fake.calls(), ["upgrade --yes --version 0.4.0"]);
    }

    #[test]
    fn a_packaged_app_upgrades_only_a_separate_terminal_podbag() {
        let app = Path::new("/Applications/podbag.app/Contents/MacOS/podbag");
        let term = Path::new("/usr/local/bin/podbag");
        assert_eq!(cli_to_upgrade(app, true, Some(term)), Some(term.into()));
        assert_eq!(cli_to_upgrade(app, true, None), None);
        assert_eq!(cli_to_upgrade(term, false, None), Some(term.into()));
    }

    #[test]
    fn the_plan_says_what_is_older() {
        let term = Fake::new("--version) echo 'podbag 0.3.0';;");
        let base = Updates {
            engine: Some("0.4.0".into()),
            app: "0.4.0".into(),
            latest: Some("0.4.0".into()),
            ..Updates::default()
        };
        // Packaged and current, with an old terminal podbag: only the CLI.
        let mut u = base.clone();
        plan(&mut u, Some("macos-app"), Some(&term.bin));
        assert_eq!(u.installer.as_deref(), Some("macos-app"));
        assert_eq!(u.terminal_version.as_deref(), Some("0.3.0"));
        assert!(u.newer && u.cli_newer && !u.app_newer);
        // An old packaged app: the app.
        let mut u = Updates {
            app: "0.3.0".into(),
            engine: Some("0.3.0".into()),
            ..base.clone()
        };
        plan(&mut u, Some("deb"), None);
        assert!(u.newer && u.app_newer && !u.cli_newer);
        assert_eq!(u.terminal, None);
        // Not packaged: the app's podbag, as before.
        let mut u = Updates {
            engine: Some("0.3.0".into()),
            ..base.clone()
        };
        plan(&mut u, None, None);
        assert!(u.newer && u.cli_newer && !u.app_newer);
        let mut u = base.clone();
        plan(&mut u, None, None);
        assert!(!u.newer);
    }

    #[test]
    fn admin_commands_quote_paths_with_spaces() {
        assert_eq!(
            admin_command(Path::new("/usr/local/bin/podbag")),
            "sudo /usr/local/bin/podbag upgrade"
        );
        assert_eq!(
            admin_command(Path::new("/Applications/My Tools/podbag")),
            "sudo \"/Applications/My Tools/podbag\" upgrade"
        );
    }
}
