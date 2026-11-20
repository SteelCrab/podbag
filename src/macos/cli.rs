//! The Mac's `podbag`: commands run in the VM. It starts the VM host when none runs,
//! sends the command (see [`crate::vm::proto`]), and passes input, output, the terminal's
//! size and signals until the command's exit code comes back.

use super::host::{Paths, make_run_dir, paths};
use super::settings;
use crate::vm::proto::{CLI_PORT, CliRequest, Frame};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A command for the VM, and what the Mac's side must know of it.
pub struct Forward {
    /// The arguments after `podbag`.
    pub argv: Vec<String>,
    /// It reads stdin (`run -i`, `exec -i`, `start -ai`, `load`): only then is input sent,
    /// and a terminal put in raw mode.
    pub stdin: bool,
    /// Mac paths it names (bind mount sources, files), as given.
    pub paths: Vec<String>,
    /// More environment for the command: a login for its registry ([`crate::image::auth::AUTH_ENV`]).
    /// It goes to the VM in memory, never to its disk.
    pub env: Vec<(String, String)>,
}

/// How long a command waits for a VM: one that stops first (30 s at most), then a new
/// one (the host's boot time, 120 s at most).
const WAIT_FOR_VM: Duration = Duration::from_secs(180);

/// The Mac's settings that mean the same in the VM. Paths and programs
/// (PODBAG_DATA_DIR, PODBAG_BIN, …) are the Mac's own.
const SETTINGS: &[&str] = &[
    "PODBAG_KEEPER",
    "PODBAG_KEEPER_LINGER_MS",
    "PODBAG_IMAGE_MOUNT",
    "PODBAG_STORE_LIMIT",
    "PODBAG_MEMORY_BUDGET",
    "PODBAG_DEFAULT_SUBNET",
];

/// Input sent ahead of what the command took.
const INPUT_WINDOW: u64 = 1 << 20;

/// The Mac's clock, for the VM.
pub fn now() -> (u64, u32) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}

/// A connection to the VM host's `vm.sock`. With `start`, a host is started when none
/// runs, and it waits (until `deadline`) while one starts or stops.
pub fn connect_host(start: bool, deadline: Instant) -> Result<UnixStream, String> {
    let p = paths()?;
    if let Ok(s) = UnixStream::connect(&p.vm_sock) {
        return Ok(s);
    }
    if !start {
        return Err("the VM is not running".into());
    }
    make_run_dir(&p)?;
    // The host this command started, and where its lines in the log begin.
    let mut ours: Option<(std::process::Child, u64)> = None;
    loop {
        if let Ok(s) = UnixStream::connect(&p.vm_sock) {
            return Ok(s);
        }
        match ours.as_mut().map(|(host, at)| (host.try_wait(), *at)) {
            Some((Ok(Some(status)), at)) if !status.success() => {
                return Err(error_since(&p.log, at));
            }
            // It found another host (starting, or stopping): that one is waited for.
            Some((Ok(Some(_)), _)) => ours = None,
            Some(_) => {}
            None => {
                if lock_is_free(&p.lock) {
                    ours = Some(start_host(&p)?);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the VM did not start in time; see {} and {}",
                p.log.display(),
                p.console.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn start_host(p: &Paths) -> Result<(std::process::Child, u64), String> {
    // Appended to: two starting at once must not wipe each other's lines.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p.log)
        .map_err(|e| format!("{}: {e}", p.log.display()))?;
    let at = log.metadata().map_or(0, |m| m.len());
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__vm-host")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    // Its own session: a Ctrl+C or hangup meant for this command must not reach it.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let host = cmd
        .spawn()
        .map_err(|e| format!("cannot start the VM host: {e}"))?;
    Ok((host, at))
}

/// Whether no VM host holds the lock (none runs, starts or stops).
fn lock_is_free(lock: &Path) -> bool {
    let Ok(f) = std::fs::File::open(lock) else {
        return true;
    };
    // SAFETY: flock on our own descriptor; released when it closes.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// The last error the VM host wrote after `at` in its log.
fn error_since(log: &Path, at: u64) -> String {
    let text = std::fs::read(log).unwrap_or_default();
    let text = String::from_utf8_lossy(text.get(at as usize..).unwrap_or_default());
    match text.rfind("podbag: ") {
        Some(start) => text[start..]
            .trim()
            .trim_start_matches("podbag: ")
            .to_string(),
        None => format!("the VM host ended; see {}", log.display()),
    }
}

const VM_USAGE: &str = "usage: podbag vm start|stop|restart [--yes]|status [--json]|logs\n       \
                        podbag vm set [--cpus N] [--memory SIZE] [--idle TIME] [--restart] [--yes]";

/// `podbag vm start|stop|restart|status|set|logs`.
pub fn vm_command(args: &[String]) -> i32 {
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("start") => match start() {
            0 => {
                println!("running");
                Ok(())
            }
            _ => return 1,
        },
        Some("status") if rest.iter().any(|a| a == "--json") => {
            paths().map(|p| println!("{}", status_json(&p)))
        }
        Some("status") => paths().map(|p| println!("{}", status(&p))),
        Some("stop") => stop(),
        Some("restart") => {
            let yes = rest.iter().any(|a| a == "--yes" || a == "-y");
            restart(yes).map(|()| println!("running"))
        }
        Some("set") => set(rest),
        Some("logs") => paths().and_then(|p| {
            let log = std::fs::read_to_string(&p.console)
                .map_err(|e| format!("{}: {e}", p.console.display()))?;
            print!("{log}");
            Ok(())
        }),
        _ => Err(VM_USAGE.into()),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("podbag: {e}");
            1
        }
    }
}

/// A round trip to the VM's agent: the VM is up, and works. Its exit code.
/// A round trip to the VM's agent: the VM is up, and works.
fn start() -> i32 {
    forward(Forward {
        argv: vec!["__vm-check".into()],
        stdin: false,
        paths: Vec::new(),
        env: Vec::new(),
    })
}

/// `running`, `standby` (the VM is off, the sockets wait), `starting` or `stopped`,
/// without connecting.
pub fn state(p: &Paths) -> &'static str {
    if lock_is_free(&p.lock) {
        return "stopped";
    }
    match std::fs::read_to_string(&p.state).as_deref().map(str::trim) {
        Ok("running") => "running",
        Ok("standby") => "standby",
        _ => "starting",
    }
}

/// What the running VM's host wrote (None when no VM runs).
fn running_info(p: &Paths) -> Option<settings::Running> {
    if state(p) != "running" {
        return None;
    }
    serde_json::from_slice(&std::fs::read(&p.running).ok()?).ok()
}

/// The containers that run in the VM, as its host last counted them.
pub fn running_containers(p: &Paths) -> u64 {
    running_info(p).and_then(|r| r.containers).unwrap_or(0)
}

/// `podbag vm status --json`, for apps: the state, the settings in use and those of the
/// next start, the limits of this Mac, the disk, and what the running VM does. It reads
/// files only: it wakes no VM and keeps none up.
pub fn status_json(p: &Paths) -> serde_json::Value {
    use std::os::unix::fs::MetadataExt;
    let mac = settings::Mac::this();
    let saved = settings::load(&p.home);
    let env = |k: &str| std::env::var(k).ok();
    let (next, overridden) = settings::effective(&saved, &env, &mac);
    let state = state(p);
    let info = running_info(p);
    let in_use = info.as_ref().and_then(|i| i.resources);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let disk = std::fs::metadata(&p.disk).ok().map(|m| {
        serde_json::json!({
            "path": p.disk,
            "allocatedBytes": m.blocks() * 512,
            "sizeBytes": m.len(),
        })
    });
    let pending = in_use.is_some_and(|u| u.cpus != next.cpus || u.memory_mib != next.memory_mib);
    serde_json::json!({
        "platform": "macos",
        "state": state,
        "resourcesEditable": true,
        "inUse": in_use,
        "next": next,
        "saved": saved,
        "overriddenBy": overridden,
        "pending": pending,
        "limits": {
            "cpusMin": 1,
            "cpusMax": mac.cpus,
            "memoryMinMiB": settings::MEMORY_MIN_MIB,
            "memoryMaxMiB": mac.memory_max_mib(),
            "macMemoryMiB": mac.memory_mib,
        },
        "uptimeSeconds": info
            .as_ref()
            .filter(|i| i.started > 0)
            .map(|i| now.saturating_sub(i.started)),
        "containers": info.as_ref().and_then(|i| i.containers),
        "cpuPercent": info.as_ref().and_then(|i| i.cpu_percent),
        "memoryUsedMiB": info.as_ref().and_then(|i| i.memory_used_mib),
        "memoryTotalMiB": info.as_ref().and_then(|i| i.memory_total_mib),
        "disk": disk,
    })
}

fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// "1 container runs" or "3 containers run".
fn containers_run(n: u64) -> String {
    if n == 1 {
        "1 container runs".into()
    } else {
        format!("{n} containers run")
    }
}

/// A restart stops the containers that run: asked first (or `--yes`).
fn confirm_restart(p: &Paths, yes: bool) -> Result<(), String> {
    let n = running_containers(p);
    if n == 0 || yes {
        return Ok(());
    }
    let what = containers_run(n);
    if !interactive() {
        return Err(format!(
            "{what} in the VM, and a restart stops them; add --yes to restart anyway"
        ));
    }
    eprint!("{what} in the VM. A restart stops them. Restart? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err("the VM was not restarted".into())
    }
}

/// `podbag vm restart`: power the VM off and start it again with the saved settings.
/// Running containers stop (asked first, unless `yes`). A VM that is off just starts.
pub fn restart(yes: bool) -> Result<(), String> {
    let p = paths()?;
    if state(&p) == "running" {
        confirm_restart(&p, yes)?;
        stop()?;
    }
    match start() {
        0 => Ok(()),
        _ => Err("the VM did not start again".into()),
    }
}

/// `podbag vm set`: check and save the VM's settings; they apply at its next start
/// (`--restart`: now).
fn set(args: &[String]) -> Result<(), String> {
    let p = paths()?;
    let mut saved = settings::load(&p.home);
    let (mut restart_now, mut yes, mut changed) = (false, false, false);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (a, None),
        };
        let mut value = || -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value\n{VM_USAGE}"))
        };
        match name {
            "--cpus" => {
                let v = value()?;
                let n = v
                    .trim()
                    .parse()
                    .map_err(|_| format!("--cpus: {v:?} is not a number"))?;
                saved.cpus = Some(n);
                changed = true;
            }
            "--memory" => {
                saved.memory_mib = Some(settings::parse_memory(&value()?)?);
                changed = true;
            }
            "--idle" => {
                saved.idle_seconds = Some(settings::parse_idle(&value()?)?);
                changed = true;
            }
            "--restart" => restart_now = true,
            "--yes" | "-y" => yes = true,
            _ => return Err(format!("unknown option {a}\n{VM_USAGE}")),
        }
        i += 1;
    }
    if !changed {
        return Err(format!("nothing to set\n{VM_USAGE}"));
    }
    let mac = settings::Mac::this();
    settings::validate(&saved, &mac)?;
    // Asked before anything is saved: a "no" changes nothing.
    if restart_now && state(&p) == "running" {
        confirm_restart(&p, yes)?;
    }
    settings::save(&p.home, &saved)?;
    let env = |k: &str| std::env::var(k).ok();
    let (next, overridden) = settings::effective(&saved, &env, &mac);
    let idle = match next.idle_seconds {
        0 => "never".to_string(),
        s => format!("after {}", settings::idle_text(s)),
    };
    println!(
        "Saved: {} CPUs, {} memory, power off when idle: {idle}.",
        next.cpus,
        settings::memory_text(next.memory_mib),
    );
    for v in overridden {
        println!("{v} is set here, and it wins over the saved value.");
    }
    if restart_now {
        return restart(true);
    }
    if state(&p) == "running" {
        println!(
            "The VM uses them at its next start. To use them now: podbag vm restart \
             (running containers stop)."
        );
    } else {
        println!("The VM uses them at its next start.");
    }
    Ok(())
}

/// After `podbag upgrade`: the guest is in the program, so a running VM restarts to run
/// the new one. Not while containers run: then the user is told how and when.
pub fn after_upgrade() {
    let Ok(p) = paths() else { return };
    if state(&p) != "running" {
        return;
    }
    let n = running_containers(&p);
    if n > 0 {
        println!(
            "{} in the VM, which runs the old version until it restarts. `podbag vm \
             restart` restarts it now (the containers stop). Otherwise it powers off when \
             idle, and the next command starts the new version.",
            containers_run(n)
        );
        return;
    }
    println!("Restarting the VM with the new version.");
    if let Err(e) = restart(true) {
        eprintln!("podbag: {e}");
    }
}

/// `podbag vm status`: `running` or `stopped`. It does not connect, so it wakes no VM.
pub fn vm_status() -> Result<&'static str, String> {
    paths().map(|p| status(&p))
}

/// `running` or `stopped`, from the host's state file and its lock: without connecting,
/// since a connection keeps the VM up.
fn status(p: &Paths) -> &'static str {
    let up = std::fs::read_to_string(&p.state).is_ok_and(|s| s.trim() == "running");
    if up && !lock_is_free(&p.lock) {
        "running"
    } else {
        "stopped"
    }
}

/// The process a pid file names: never 0, 1 or a negative number, which `kill` takes
/// for a process group, init, or every process of the user.
fn pid_from(text: &str) -> Option<libc::pid_t> {
    text.trim()
        .parse()
        .ok()
        .filter(|pid: &libc::pid_t| *pid > 1)
}

/// The host (or its standby) powers the VM off and ends: SIGTERM, which wakes no VM.
fn stop() -> Result<(), String> {
    let p = paths()?;
    if lock_is_free(&p.lock) {
        return Ok(());
    }
    let named = |p: &Paths| {
        std::fs::read_to_string(&p.pid)
            .ok()
            .and_then(|s| pid_from(&s))
    };
    // The host, and the standby it may start meanwhile, end; a host that a command starts
    // meanwhile (it never begins as a standby) is left alone. Powering off takes 30
    // seconds at most.
    let deadline = Instant::now() + Duration::from_secs(40);
    let unnamed = Instant::now();
    let mut signalled: Vec<libc::pid_t> = Vec::new();
    loop {
        if lock_is_free(&p.lock) {
            return Ok(());
        }
        // The pid first, then the state: the host and its standby write the state first, so
        // a new pid is never seen with an older state.
        let pid = named(&p);
        let standby = std::fs::read_to_string(&p.state).is_ok_and(|s| s.trim() == "standby");
        match pid {
            Some(pid) if signalled.contains(&pid) => {}
            Some(pid) if signalled.is_empty() || standby => {
                // SAFETY: a signal to the VM host or its standby: it holds the lock, so the
                // pid file is its own.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                signalled.push(pid);
            }
            Some(_) => return Ok(()),
            // Between a host that ended and the standby it started, which names itself in a
            // moment. A host that never names itself is asked on its socket.
            None if signalled.is_empty() && unnamed.elapsed() > Duration::from_secs(1) => {
                if let Ok(mut s) = UnixStream::connect(&p.vm_sock) {
                    let _ = s.write_all(b"STOP\n");
                    let _ = s.read_to_end(&mut Vec::new());
                }
                return Ok(());
            }
            None => {}
        }
        if Instant::now() >= deadline {
            return Err("the VM host did not stop within 40 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The path with `..` and links resolved, as far as it exists (the rest is appended).
fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => resolve(dir).join(name),
        _ => {
            let mut clean = PathBuf::from("/");
            for c in path.components() {
                match c {
                    Component::ParentDir => {
                        clean.pop();
                    }
                    Component::Normal(n) => clean.push(n),
                    _ => {}
                }
            }
            clean
        }
    }
}

/// The VM shares only the Mac's /Users, at the same path: any other path would be the
/// VM's own, silently.
fn check_paths(paths: &[String], cwd: &Path) -> Result<(), String> {
    for given in paths {
        let full = resolve(&cwd.join(given));
        if !full.starts_with("/Users") {
            return Err(format!(
                "cannot use {given} ({}): the VM sees only /Users of this Mac\n  \
                 fix:   move it under /Users, or use a named volume",
                full.display()
            ));
        }
    }
    Ok(())
}

/// Why a connection did not get a command into the VM.
enum Failed {
    /// The host was stopping (or went away) before it took the command: the next host
    /// gets it.
    NotTaken,
    Error(String),
}

/// Run `podbag <argv>` in the VM; returns its exit code.
pub fn forward(f: Forward) -> i32 {
    // SAFETY: isatty has no memory arguments.
    let tty = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    let here = std::env::current_dir().unwrap_or_else(|_| "/".into());
    if let Err(e) = check_paths(&f.paths, &here) {
        eprintln!("podbag: {e}");
        return 125;
    }
    // The VM has the same directory only under /Users.
    let real = resolve(&here);
    let cwd = if real.starts_with("/Users") {
        real.display().to_string()
    } else {
        "/".to_string()
    };
    let deadline = Instant::now() + WAIT_FOR_VM;
    loop {
        let result = connect_host(true, deadline)
            .map_err(Failed::Error)
            .and_then(|stream| run_remote(stream, &f, tty, &cwd));
        match result {
            Ok(code) => return code,
            Err(Failed::NotTaken) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(Failed::NotTaken) => {
                eprintln!("podbag: no VM took the command in time");
                return 125;
            }
            Err(Failed::Error(e)) => {
                eprintln!("podbag: {e}");
                return 125;
            }
        }
    }
}

fn window_size() -> (u16, u16) {
    // SAFETY: TIOCGWINSZ fills a winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 {
            (ws.ws_row, ws.ws_col)
        } else {
            (24, 80)
        }
    }
}

/// The terminal in raw mode until dropped: the VM's terminal does the line editing.
struct Raw(libc::termios);

impl Raw {
    fn enter() -> Option<Raw> {
        // SAFETY: termios calls on stdin with structs we own.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            Some(Raw(saved))
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: restores what `enter` read.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

extern "C" fn nothing(_: libc::c_int) {}

/// Signals another process (or the terminal, outside raw mode) sends this command go to
/// the VM's; a terminal's new size too.
fn take_signals(tty: bool) -> Vec<i32> {
    let mut signals = vec![libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];
    if tty {
        signals.push(libc::SIGWINCH);
        // SAFETY: an empty handler. macOS drops a blocked signal whose action is to ignore
        // it (SIGWINCH's default) instead of keeping it for sigwait.
        unsafe { libc::signal(libc::SIGWINCH, nothing as *const () as libc::sighandler_t) };
    }
    crate::signals::block(&signals);
    signals
}

/// The connection's sending side, shared by the threads that send.
#[derive(Clone)]
struct Out(Arc<Mutex<UnixStream>>);

impl Out {
    fn send(&self, f: Frame) -> std::io::Result<()> {
        let mut w = self.0.lock().unwrap_or_else(|p| p.into_inner());
        f.write_to(&mut *w)
    }
}

/// Input sent and not yet taken by the command.
#[derive(Default)]
struct Window(Mutex<u64>, Condvar);

fn run_remote(mut stream: UnixStream, f: &Forward, tty: bool, cwd: &str) -> Result<i32, Failed> {
    // A host that is stopping closes without `OK`: the command never reached its VM. One
    // whose VM cannot start says why (`ERR …`).
    stream
        .write_all(format!("PORT {CLI_PORT}\n").as_bytes())
        .map_err(|_| Failed::NotTaken)?;
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    while answer.len() < 4096 {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => answer.push(byte[0]),
            _ => return Err(Failed::NotTaken),
        }
    }
    let answer = String::from_utf8_lossy(&answer);
    if let Some(why) = answer.strip_prefix("ERR ") {
        return Err(Failed::Error(
            why.trim_start_matches("podbag: ").replace('\t', "\n"),
        ));
    }
    if answer != "OK" {
        return Err(Failed::NotTaken);
    }
    let io = |e: std::io::Error| Failed::Error(format!("the connection to the VM failed: {e}"));
    let (rows, cols) = if tty { window_size() } else { (24, 80) };
    let req = CliRequest {
        argv: f.argv.clone(),
        env: std::env::vars()
            .filter(|(k, _)| SETTINGS.contains(&k.as_str()))
            .chain(f.env.iter().cloned())
            .collect(),
        cwd: cwd.to_string(),
        tty,
        rows,
        cols,
        time: now(),
    };
    let mut line = serde_json::to_string(&req).map_err(|e| Failed::Error(e.to_string()))?;
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(io)?;
    // Blocked before any thread starts, so only the signal thread takes them.
    let signals = take_signals(tty);
    let raw = if tty && f.stdin { Raw::enter() } else { None };
    let out = Out(Arc::new(Mutex::new(stream.try_clone().map_err(io)?)));
    let window = Arc::new(Window::default());
    if f.stdin {
        let (out, window) = (out.clone(), window.clone());
        std::thread::spawn(move || send_input(&out, &window));
    } else if !tty {
        // The command has no input: it sees its end at once.
        let _ = out.send(Frame::StdinEnd);
    }
    {
        let out = out.clone();
        std::thread::spawn(move || {
            loop {
                let sig = wait_for(&signals);
                let frame = if sig == libc::SIGWINCH {
                    let (rows, cols) = window_size();
                    Frame::Resize { rows, cols }
                } else {
                    Frame::Signal(sig)
                };
                if out.send(frame).is_err() {
                    break;
                }
            }
        });
    }
    let result = take_output(&mut stream, &window);
    drop(raw);
    // Whatever made it end, the VM's command learns it: an early end is a hangup there.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    result
}

/// The Mac's stdin to the VM, at most a window ahead of what the command took.
fn send_input(out: &Out, window: &Window) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut stdin = std::io::stdin().lock();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = out.send(Frame::StdinEnd);
                return;
            }
            Ok(n) => {
                let mut ahead = window.0.lock().unwrap_or_else(|p| p.into_inner());
                while *ahead >= INPUT_WINDOW {
                    ahead = window.1.wait(ahead).unwrap_or_else(|p| p.into_inner());
                }
                *ahead += n as u64;
                drop(ahead);
                if out.send(Frame::Stdin(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
        }
    }
}

/// The command's output and exit code. A reader that went away (`| head`) ends this
/// command as it would end a local one: exit code 141, as from SIGPIPE.
fn take_output(stream: &mut UnixStream, window: &Window) -> Result<i32, Failed> {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr();
    loop {
        match Frame::read_from(stream) {
            Ok(Some(Frame::Stdout(d))) => {
                if out.write_all(&d).and_then(|()| out.flush()).is_err() {
                    return Ok(128 + libc::SIGPIPE);
                }
            }
            Ok(Some(Frame::Stderr(d))) => {
                let _ = err.write_all(&d);
            }
            Ok(Some(Frame::StdinTaken(n))) => {
                let mut ahead = window.0.lock().unwrap_or_else(|p| p.into_inner());
                *ahead = ahead.saturating_sub(u64::from(n));
                window.1.notify_all();
            }
            Ok(Some(Frame::Exit(code))) => return Ok(code),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                return Err(Failed::Error(
                    "the VM ended the command without an exit code".into(),
                ));
            }
        }
    }
}

/// The next of `signals` (blocked in every thread).
fn wait_for(signals: &[i32]) -> i32 {
    // SAFETY: a sigset on the stack with the signals blocked by `take_signals`.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &s in signals {
            libc::sigaddset(&mut set, s);
        }
        let mut sig = 0;
        libc::sigwait(&set, &mut sig);
        sig
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_paths_under_users_are_mounted() {
        let paths = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let home = std::env::var("HOME").unwrap();
        let home = Path::new(&home);
        assert!(check_paths(&paths(&["./src", "x/y", "/Users"]), home).is_ok());
        assert!(check_paths(&paths(&["./src"]), Path::new("/tmp")).is_err());
        assert!(check_paths(&paths(&["/opt/x"]), home).is_err());
        let up = "../".repeat(home.components().count());
        assert!(check_paths(&paths(&[&format!("{up}tmp")]), home).is_err());
        assert!(
            check_paths(
                &paths(&[&format!("{}/a/../../../../tmp", home.display())]),
                home
            )
            .is_err()
        );
    }

    #[test]
    fn links_out_of_users_are_followed() {
        let dir = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let link = dir.path().join("out");
        std::os::unix::fs::symlink("/tmp", &link).unwrap();
        let paths = vec![link.join("new-file").display().to_string()];
        assert!(check_paths(&paths, dir.path()).is_err());
        let paths = vec![dir.path().join("new/deeper").display().to_string()];
        assert!(check_paths(&paths, dir.path()).is_ok());
    }
}

#[cfg(test)]
mod pid_tests {
    use super::*;

    #[test]
    fn a_pid_file_names_one_process() {
        assert_eq!(pid_from("123\n"), Some(123));
        // kill(0) and kill(-1) would reach the process group or every process.
        for bad in ["0", "-1", "1", "", "x", "-42"] {
            assert_eq!(pid_from(bad), None, "{bad:?}");
        }
    }
}
