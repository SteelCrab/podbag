//! `openpod __vm-host`: the Mac process that owns the VM while it runs.
//!
//! Only the process that made a VM may open its vsock connections, so this one also
//! proxies them: `vm.sock` takes a first line `PORT <n>` (answered `OK` once the guest's
//! port n is connected, then bytes both ways) or `STOP` (the VM is powered off, then the
//! connection closes); `docker.sock` goes straight to the Docker API. One host runs per
//! user (a lock file). It powers the VM off and ends when no connection has been open
//! for the idle time (`OPENPOD_VM_IDLE`, seconds; 60) and the VM has no work, when asked,
//! or on SIGTERM, SIGINT or SIGHUP (a logout).

use super::vm::{Vm, VmSpec};
use crate::vm::proto::{API_PORT, CLI_PORT, CliRequest, Frame};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// The files of the Mac side.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `~/.openpod` (`OPENPOD_MAC_HOME` for tests).
    pub home: PathBuf,
    /// The user's alone (0700): its sockets give root in the VM.
    pub run: PathBuf,
    /// The guest: `Image`, `initramfs.gz`, `disk.zst`.
    pub guest: PathBuf,
    pub disk: PathBuf,
    pub console: PathBuf,
    pub vm_sock: PathBuf,
    pub docker_sock: PathBuf,
    pub lock: PathBuf,
    pub log: PathBuf,
}

pub fn paths() -> Result<Paths, String> {
    let home = match std::env::var_os("OPENPOD_MAC_HOME") {
        Some(h) => PathBuf::from(h),
        None => {
            let user = std::env::var_os("HOME").ok_or("HOME is not set")?;
            PathBuf::from(user).join(".openpod")
        }
    };
    let run = home.join("run");
    Ok(Paths {
        guest: guest_dir(&home),
        disk: home.join("disk.img"),
        console: home.join("console.log"),
        vm_sock: run.join("vm.sock"),
        docker_sock: run.join("docker.sock"),
        lock: run.join("host.lock"),
        log: run.join("host.log"),
        run,
        home,
    })
}

/// Where the guest files are: `OPENPOD_VM_GUEST`, then `vm/` beside the program's
/// directory (`target/vm` for a build in the repository), then `~/.openpod/vm`.
fn guest_dir(home: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENPOD_VM_GUEST") {
        return PathBuf::from(dir);
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent().map(|d| d.join("vm")));
    match beside {
        Some(dir) if dir.join("Image").exists() => dir,
        _ => home.join("vm"),
    }
}

/// `run/`, made the user's alone (also when it was there before).
pub fn make_run_dir(p: &Paths) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let err = |e: std::io::Error| format!("{}: {e}", p.run.display());
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&p.run)
        .map_err(err)?;
    std::fs::set_permissions(&p.run, std::fs::Permissions::from_mode(0o700)).map_err(err)
}

pub fn host_main() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("openpod: {e}");
            1
        }
    }
}

/// A line for the host's log (errors are the lines that start with `openpod: `).
fn note(what: &str) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    eprintln!("vm [{at}]: {what}");
}

/// The connections, and whether the host is stopping (then it takes none).
#[derive(Default)]
struct Activity {
    open: usize,
    changed: Option<Instant>,
    /// Counts every connection that opened or closed.
    generation: u64,
    stopping: bool,
}

#[derive(Default)]
struct State(Mutex<Activity>);

impl State {
    fn lock(&self) -> std::sync::MutexGuard<'_, Activity> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A new connection; false when the host is stopping (it is not served).
    fn open(&self) -> bool {
        let mut a = self.lock();
        if a.stopping {
            return false;
        }
        a.open += 1;
        a.generation += 1;
        a.changed = Some(Instant::now());
        true
    }

    fn close(&self) {
        let mut a = self.lock();
        a.open = a.open.saturating_sub(1);
        a.generation += 1;
        a.changed = Some(Instant::now());
    }

    /// How long no connection has been open (zero while one is), and the generation.
    fn idle(&self, started: Instant) -> (Duration, u64) {
        let a = self.lock();
        let idle = if a.open > 0 {
            Duration::ZERO
        } else {
            a.changed.unwrap_or(started).elapsed()
        };
        (idle, a.generation)
    }

    /// Stop, unless a connection came or went since `generation` was read.
    fn stop_if_unchanged(&self, generation: u64) -> bool {
        let mut a = self.lock();
        if a.open == 0 && a.generation == generation {
            a.stopping = true;
        }
        a.stopping
    }

    fn stop(&self) {
        self.lock().stopping = true;
    }
}

/// Why the host's loop wakes before its second is up.
enum Wake {
    /// `STOP` on vm.sock: the connection closes once the VM is off.
    Asked(UnixStream),
    Signal(i32),
}

fn run() -> Result<(), String> {
    let p = paths()?;
    // SAFETY: umask only sets this process's mask for new files: they are the user's.
    unsafe { libc::umask(0o077) };
    make_run_dir(&p)?;
    let lock = std::fs::File::create(&p.lock).map_err(|e| format!("{}: {e}", p.lock.display()))?;
    if !take_lock(&lock) {
        // Another host runs (or is starting, or stopping): the CLI waits for it.
        return Ok(());
    }
    // A logout ends the host with SIGTERM: the VM is powered off properly first.
    let signals = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];
    crate::signals::block(&signals);
    let (wake, woken) = mpsc::channel();
    {
        let wake = wake.clone();
        std::thread::spawn(move || {
            let _ = wake.send(Wake::Signal(wait_for(&signals)));
        });
    }
    for f in ["Image", "initramfs.gz"] {
        if !p.guest.join(f).exists() {
            return Err(format!(
                "the VM's {f} is missing in {}: build the guest with \
                 `dev/vm.sh run dev/macos/build-guest.sh`",
                p.guest.display()
            ));
        }
    }
    make_disk(&p)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let spec = VmSpec {
        kernel: p.guest.join("Image"),
        initrd: p.guest.join("initramfs.gz"),
        disk: p.disk.clone(),
        console: p.console.clone(),
        cpus: std::thread::available_parallelism()
            .map_or(2, |n| n.get())
            .min(8),
        memory: env_number("OPENPOD_VM_MEMORY", 2048) << 20,
        // After `--`: the argument of /init, which makes openpod the VM's init.
        cmdline: format!(
            "console=hvc0 ip=dhcp loglevel=4 panic=-1 openpod.time={}.{:09} -- {}",
            now.as_secs(),
            now.subsec_nanos(),
            crate::vm::proto::INIT_ARG
        ),
        share: PathBuf::from("/Users"),
    };
    let vm = Arc::new(Vm::start(&spec).map_err(start_error)?);
    let started = Instant::now();
    if let Err(e) = wait_ready(&vm, &p) {
        power_off(&vm);
        return Err(e);
    }
    note("started");
    let state = Arc::new(State::default());
    let api = listen(&p.docker_sock)?;
    let proxy = listen(&p.vm_sock)?;
    for (listener, docker) in [(api, true), (proxy, false)] {
        let (vm, state, wake) = (vm.clone(), state.clone(), wake.clone());
        std::thread::spawn(move || serve(&listener, &vm, &state, &wake, docker));
    }
    let idle = Duration::from_secs(env_number("OPENPOD_VM_IDLE", 60));
    let mut askers = Vec::new();
    let why = loop {
        match woken.recv_timeout(Duration::from_secs(1)) {
            Ok(Wake::Asked(conn)) => {
                askers.push(conn);
                break "asked".to_string();
            }
            Ok(Wake::Signal(sig)) => break format!("signal {sig}"),
            Err(_) => {}
        }
        if !vm.running() {
            note("the VM stopped by itself; see the console log");
            remove_sockets(&p);
            return Ok(());
        }
        let (quiet, generation) = state.idle(started);
        if quiet >= idle && !busy(&vm) && state.stop_if_unchanged(generation) {
            break "idle".to_string();
        }
    };
    // From here on no command reaches this VM: the next one starts another host.
    state.stop();
    remove_sockets(&p);
    note(&format!("stopping ({why})"));
    power_off(&vm);
    note("stopped");
    drop(askers);
    Ok(())
}

/// The lock, tried for a moment: a CLI may hold it briefly to see whether a host runs.
fn take_lock(lock: &std::fs::File) -> bool {
    for _ in 0..10 {
        // SAFETY: flock on our own descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

/// The next of `signals` (blocked in every thread).
fn wait_for(signals: &[i32]) -> i32 {
    // SAFETY: a sigset on the stack with the signals the caller blocked.
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

/// The data disk, from the template when there is none: written beside it, flushed,
/// then renamed, so a host that dies meanwhile leaves no half disk behind.
fn make_disk(p: &Paths) -> Result<(), String> {
    if p.disk.exists() {
        return Ok(());
    }
    let template = p.guest.join("disk.zst");
    let part = p.disk.with_extension("img.part");
    let _ = std::fs::remove_file(&part);
    let err = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
    let mut t = std::fs::File::open(&template).map_err(|e| err(&template, e))?;
    crate::vm::template::expand(&mut t, &part).map_err(|e| e.to_string())?;
    std::fs::File::open(&part)
        .and_then(|f| f.sync_all())
        .map_err(|e| err(&part, e))?;
    std::fs::rename(&part, &p.disk).map_err(|e| err(&p.disk, e))
}

fn remove_sockets(p: &Paths) {
    for s in [&p.vm_sock, &p.docker_sock] {
        let _ = std::fs::remove_file(s);
    }
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn start_error(why: String) -> String {
    if why.contains("entitlement") {
        format!(
            "cannot start the VM: {why}\n  fix:   sign openpod with the virtualization \
             entitlement: dev/macos/sign.sh <path to openpod>"
        )
    } else {
        format!("cannot start the VM: {why}")
    }
}

/// Wait until the agent answers, and ask it whether the VM works.
fn wait_ready(vm: &Vm, p: &Paths) -> Result<(), String> {
    let started = Instant::now();
    loop {
        match guest_command(vm, "__vm-check") {
            Ok((0, _)) => return Ok(()),
            Ok((_, said)) => {
                return Err(said.trim().trim_start_matches("openpod: ").to_string());
            }
            Err(_) if started.elapsed() < Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                return Err(format!(
                    "the VM started but its agent did not answer within 30 seconds; see {}",
                    p.console.display()
                ));
            }
        }
    }
}

/// One of the agent's own commands (`__vm-…`): its exit code and what it printed.
fn guest_command(vm: &Vm, what: &str) -> std::io::Result<(i32, String)> {
    let mut s = UnixStream::from(vm.connect(CLI_PORT)?);
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let req = CliRequest {
        argv: vec![what.into()],
        env: Vec::new(),
        cwd: "/".into(),
        tty: false,
        rows: 0,
        cols: 0,
        time: super::cli::now(),
    };
    let mut line = serde_json::to_string(&req).map_err(std::io::Error::other)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut said = String::new();
    loop {
        match Frame::read_from(&mut s)? {
            Some(Frame::Stdout(d) | Frame::Stderr(d)) => {
                said.push_str(&String::from_utf8_lossy(&d))
            }
            Some(Frame::Exit(code)) => return Ok((code, said)),
            Some(_) => {}
            None => return Err(std::io::Error::other("the agent closed the connection")),
        }
    }
}

/// Whether the VM has work (a container runs, or waits to start again). When it cannot
/// tell, it has: the VM stays.
fn busy(vm: &Vm) -> bool {
    guest_command(vm, "__vm-busy").map_or(true, |(code, _)| code == 0)
}

fn listen(path: &Path) -> Result<UnixListener, String> {
    let _ = std::fs::remove_file(path);
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let l = UnixListener::bind(path).map_err(err)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(err)?;
    Ok(l)
}

/// What a vm.sock connection asks first.
enum First {
    Port(u32),
    Stop,
}

/// The first line of a vm.sock connection, read a byte at a time so nothing after it
/// is taken.
fn read_first(stream: &mut UnixStream) -> std::io::Result<First> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 32 {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            let text = String::from_utf8_lossy(&line);
            if text == "STOP" {
                return Ok(First::Stop);
            }
            return text
                .strip_prefix("PORT ")
                .and_then(|n| n.trim().parse().ok())
                .map(First::Port)
                .ok_or_else(|| std::io::Error::other(format!("bad first line {text:?}")));
        }
        line.push(byte[0]);
    }
    Err(std::io::Error::other("the first line is too long"))
}

fn serve(
    listener: &UnixListener,
    vm: &Arc<Vm>,
    state: &Arc<State>,
    wake: &mpsc::Sender<Wake>,
    docker: bool,
) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // Stopping: closed unanswered, and the CLI waits for the next host.
        if !state.open() {
            continue;
        }
        let (vm, state, wake) = (vm.clone(), state.clone(), wake.clone());
        std::thread::spawn(move || {
            if docker {
                if let Ok(fd) = vm.connect(API_PORT) {
                    splice(stream, fd);
                }
            } else {
                match read_first(&mut stream) {
                    Ok(First::Port(n)) => {
                        if let Ok(fd) = vm.connect(n) {
                            if stream.write_all(b"OK\n").is_ok() {
                                splice(stream, fd);
                            }
                        }
                    }
                    Ok(First::Stop) => {
                        let _ = wake.send(Wake::Asked(stream));
                    }
                    Err(_) => {}
                }
            }
            state.close();
        });
    }
}

/// Carry bytes both ways until both directions end.
fn splice(client: UnixStream, guest: OwnedFd) {
    let guest = UnixStream::from(guest);
    let (Ok(mut client_r), Ok(mut guest_w)) = (client.try_clone(), guest.try_clone()) else {
        return;
    };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_r, &mut guest_w);
        let _ = guest_w.shutdown(std::net::Shutdown::Write);
    });
    let (mut guest_r, mut client_w) = (guest, client);
    let _ = std::io::copy(&mut guest_r, &mut client_w);
    let _ = client_w.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

/// Ask the guest to power off; stop it outright if it has not within 30 seconds.
fn power_off(vm: &Vm) {
    let _ = guest_command(vm, "__vm-poweroff");
    let deadline = Instant::now() + Duration::from_secs(30);
    while vm.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if vm.running() {
        vm.stop();
    }
}
