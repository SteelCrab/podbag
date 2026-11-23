//! `podbag __vm-host`: the Mac process that owns the VM while it runs.
//!
//! Only the process that made a VM may open its vsock connections, so this one also
//! proxies them: `vm.sock` takes a first line `PORT <n>` (answered `OK` once the guest's
//! port n is connected, then bytes both ways) or `STOP` (the VM is powered off, then the
//! connection closes); `docker.sock` goes straight to the Docker API. One host runs per
//! user (a lock file). When no connection has been open for the idle time (`podbag vm set`,
//! or `PODBAG_VM_IDLE`; 60 s; 0: never) and the VM has no work, it powers the VM off and
//! starts the standby (see [`super::standby`]) and ends: the sockets stay, and the next
//! connection starts a VM again. When asked (`podbag vm stop`: SIGTERM, or `STOP`), and
//! on SIGTERM, SIGINT or SIGHUP (a logout), it powers the VM off and ends.

use super::settings;
use super::vm::{Vm, VmSpec};
use crate::network::Proto;
use crate::vm::proto::{API_PORT, CLI_PORT, CliRequest, FORWARD_PORT, Frame};
use crate::vm::udp;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// The files of the Mac side.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `~/.podbag` (`PODBAG_MAC_HOME` for tests).
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
    /// `running` while the VM is up (read by `podbag vm status`, which must not
    /// connect: a connection keeps the VM up).
    pub state: PathBuf,
    /// The process id of the host, or of its standby (a new process at each idle stop).
    pub pid: PathBuf,
    /// What the running VM started with, and its last counts (see
    /// [`super::settings::Running`]).
    pub running: PathBuf,
}

pub(super) fn home() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("PODBAG_MAC_HOME") {
        Some(h) => PathBuf::from(h),
        None => {
            let user = std::env::var_os("HOME").ok_or("HOME is not set")?;
            PathBuf::from(user).join(".podbag")
        }
    })
}

pub fn paths() -> Result<Paths, String> {
    let home = home()?;
    let run = home.join("run");
    Ok(Paths {
        guest: guest_dir(&home),
        disk: home.join("disk.img"),
        console: home.join("console.log"),
        vm_sock: run.join("vm.sock"),
        docker_sock: run.join("docker.sock"),
        lock: run.join("host.lock"),
        log: run.join("host.log"),
        state: run.join("state"),
        pid: run.join("host.pid"),
        running: run.join("vm.json"),
        run,
        home,
    })
}

/// Where the guest files are: `PODBAG_VM_GUEST`, then the guest this binary carries
/// (written to `~/.podbag/vm/<id>/`), then `vm/` beside the program's directory
/// (`target/vm` for a build in the repository), then `~/.podbag/vm`.
fn guest_dir(home: &Path) -> PathBuf {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent().map(|d| d.join("vm")))
        .filter(|dir| dir.join("Image").exists());
    choose_guest(
        std::env::var_os("PODBAG_VM_GUEST").map(PathBuf::from),
        || {
            let carried = super::embedded::EMBEDDED?;
            let root = home.join("vm");
            match super::embedded::extract(&carried, &root) {
                Ok(dir) => Some(dir),
                Err(e) => {
                    eprintln!(
                        "podbag: cannot write the VM's files to {}: {e}",
                        root.display()
                    );
                    None
                }
            }
        },
        beside,
        home,
    )
}

/// The first of: the setting, the carried guest, the one beside the program, `home/vm`.
fn choose_guest(
    setting: Option<PathBuf>,
    carried: impl FnOnce() -> Option<PathBuf>,
    beside: Option<PathBuf>,
    home: &Path,
) -> PathBuf {
    setting
        .or_else(carried)
        .or(beside)
        .unwrap_or_else(|| home.join("vm"))
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
            eprintln!("podbag: {e}");
            1
        }
    }
}

/// A line for the host's log (errors are the lines that start with `podbag: `).
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

/// What the host hands its standby and gets back (through exec): the lock and the two
/// listening sockets, named by `PODBAG_VM_FDS=<lock>,<docker>,<vm>`.
pub struct Inherited {
    pub lock: RawFd,
    pub docker: RawFd,
    pub vm: RawFd,
}

impl Inherited {
    pub fn from_env() -> Option<Inherited> {
        let fds: Vec<RawFd> = std::env::var("PODBAG_VM_FDS")
            .ok()?
            .split(',')
            .filter_map(|n| n.parse().ok())
            .collect();
        let [lock, docker, vm] = fds[..] else {
            return None;
        };
        Some(Inherited { lock, docker, vm })
    }

    /// `podbag <what>` with these descriptors open in it.
    fn command(&self, what: &str) -> std::io::Result<std::process::Command> {
        for fd in [self.lock, self.docker, self.vm] {
            // SAFETY: clears close-on-exec on descriptors this process owns.
            unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        }
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg(what).env(
            "PODBAG_VM_FDS",
            format!("{},{},{}", self.lock, self.docker, self.vm),
        );
        Ok(cmd)
    }

    /// Become `what` (the standby becomes `__vm-host`). Returns only on failure. Only a
    /// process that never ran a VM does this: one that did has hung in the kernel's
    /// exec (macOS 27).
    pub fn exec(&self, what: &str) -> std::io::Error {
        use std::os::unix::process::CommandExt;
        match self.command(what) {
            Ok(mut cmd) => cmd.exec(),
            Err(e) => e,
        }
    }

    /// `what` in a new process (the host starts its standby, then ends).
    pub fn spawn(&self, what: &str) -> std::io::Result<std::process::Child> {
        self.command(what)?
            .stdin(std::process::Stdio::null())
            .spawn()
    }
}

/// The sockets: no new connection reaches this host.
fn remove_sockets(p: &Paths) {
    for f in [&p.vm_sock, &p.docker_sock] {
        let _ = std::fs::remove_file(f);
    }
}

/// The run directory's files of a host that ends; the pid file last (`podbag vm stop`
/// waits for it to go).
/// Write `text` to `path` whole: through a file of this process, renamed over it, so that
/// a reader (`vm stop`, `vm status`) sees the old content or the new, never an empty file.
pub fn write_whole(path: &Path, text: &str) {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{}", std::process::id()));
    if std::fs::write(&tmp, text).is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub fn remove_files(p: &Paths) {
    remove_sockets(p);
    let _ = std::fs::remove_file(&p.running);
    let _ = std::fs::remove_file(&p.state);
    let _ = std::fs::remove_file(&p.pid);
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
    // The host and its standby live long: not in the directory of the command that
    // started them (a disk to eject, a directory to remove).
    let _ = std::env::set_current_dir("/");
    make_run_dir(&p)?;
    // From the standby: the lock is held and the sockets listen already.
    let inherited = Inherited::from_env();
    let lock = match &inherited {
        // SAFETY: the standby handed over these descriptors, which nothing else owns.
        Some(fds) => unsafe { std::fs::File::from_raw_fd(fds.lock) },
        None => {
            let lock =
                std::fs::File::create(&p.lock).map_err(|e| format!("{}: {e}", p.lock.display()))?;
            if !take_lock(&lock) {
                // Another host runs (or starts, or stops): the CLI waits for it.
                return Ok(());
            }
            write_whole(&p.pid, &std::process::id().to_string());
            lock
        }
    };
    let result = serve_vm(&p, &lock, inherited.as_ref());
    if let Err(e) = &result {
        match &inherited {
            // Woken by clients: they get the error, and the standby waits again (the
            // next client tries again).
            Some(fds) => {
                answer_queued(fds, e);
                if fds.spawn("__vm-standby").is_err() {
                    remove_files(&p);
                }
            }
            None => remove_files(&p),
        }
    }
    result
}

/// Answer the connections waiting in the sockets' queues with `error`: a docker client an
/// HTTP 500 (the docker CLI shows "Error response from daemon: …"), a podbag CLI an
/// `ERR` line instead of `OK`.
fn answer_queued(fds: &Inherited, error: &str) {
    let message = format!("podbag: cannot start the VM: {error}");
    for (fd, docker) in [(fds.docker, true), (fds.vm, false)] {
        // SAFETY: a listening socket this process owns; ManuallyDrop leaves it open.
        let listener = std::mem::ManuallyDrop::new(unsafe { UnixListener::from_raw_fd(fd) });
        let _ = listener.set_nonblocking(true);
        while let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            if docker {
                // The request head, read before the answer.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                let body = serde_json::json!({ "message": message }).to_string();
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            } else if read_first(&mut stream).is_ok() {
                let _ =
                    stream.write_all(format!("ERR {}\n", message.replace('\n', "\t")).as_bytes());
            }
        }
        let _ = listener.set_nonblocking(false);
    }
}

/// Start a VM and serve it until it stops; then become the standby, or end.
fn serve_vm(p: &Paths, lock: &std::fs::File, inherited: Option<&Inherited>) -> Result<(), String> {
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
                "the VM's {f} is missing in {}: use a build of podbag that carries its \
                 guest (`cargo build --release --features embed-guest` after \
                 `dev/vm.sh run dev/macos/build-guest.sh` in podbag's repository), or set \
                 PODBAG_VM_GUEST to a directory with Image, initramfs.gz and disk.zst",
                p.guest.display()
            ));
        }
    }
    super::cpu::check_guest(&p.guest.join("Image"))?;
    make_disk(p)?;
    let env = |k: &str| std::env::var(k).ok();
    let (resources, _) =
        settings::effective(&settings::read(&p.home)?, &env, &settings::Mac::this());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let memory = resources
        .memory_mib
        .checked_mul(1 << 20)
        .ok_or("VM memory is too large")?;
    let spec = VmSpec {
        kernel: p.guest.join("Image"),
        initrd: p.guest.join("initramfs.gz"),
        disk: p.disk.clone(),
        console: p.console.clone(),
        cpus: resources.cpus as usize,
        memory,
        // After `--`: the argument of /init, which makes podbag the VM's init. The kernel
        // sizes its bounce buffers for the VM's whole memory (64 MB at 8 GiB, written at
        // boot, so the Mac holds them), and nothing uses them: virtio here does not go
        // through the DMA API.
        cmdline: format!(
            "console=hvc0 ip=dhcp loglevel=4 panic=-1 swiotlb=2048 podbag.time={}.{:09} -- {}",
            now.as_secs(),
            now.subsec_nanos(),
            crate::vm::proto::INIT_ARG
        ),
        share: PathBuf::from("/Users"),
        mac: mac_address(&p.home),
    };
    let vm = Arc::new(Vm::start(&spec).map_err(start_error)?);
    let started = Instant::now();
    if let Err(e) = wait_ready(&vm, p) {
        power_off(&vm);
        return Err(e);
    }
    note(&format!(
        "started: {} CPUs, {} memory",
        resources.cpus,
        settings::memory_text(resources.memory_mib)
    ));
    let monitor = Arc::new(Monitor {
        file: p.running.clone(),
        seen: Mutex::new(settings::Running {
            started: now.as_secs(),
            resources: Some(resources),
            ..Default::default()
        }),
    });
    monitor.write();
    write_whole(&p.state, "running");
    // wait_ready's request set the VM's clock.
    let clock = Arc::new(Mutex::new((std::time::SystemTime::now(), Instant::now())));
    let (api, proxy) = match inherited {
        // SAFETY: listening sockets the standby handed over.
        Some(fds) => unsafe {
            (
                UnixListener::from_raw_fd(fds.docker),
                UnixListener::from_raw_fd(fds.vm),
            )
        },
        None => (listen(&p.docker_sock)?, listen(&p.vm_sock)?),
    };
    // Kept here too: a serve thread that ends must not close its socket.
    let (api, proxy) = (Arc::new(api), Arc::new(proxy));
    let state = Arc::new(State::default());
    let (refresh, refreshed) = mpsc::channel();
    for (listener, docker) in [(api.clone(), true), (proxy.clone(), false)] {
        let (vm, state, wake, refresh) = (vm.clone(), state.clone(), wake.clone(), refresh.clone());
        let clock = docker.then(|| clock.clone());
        std::thread::spawn(move || serve(&listener, &vm, &state, &wake, clock.as_ref(), &refresh));
    }
    {
        let (vm, monitor) = (vm.clone(), monitor.clone());
        std::thread::spawn(move || watch_ports(&vm, &refreshed, &monitor));
    }
    {
        let (vm, monitor) = (vm.clone(), monitor.clone());
        std::thread::spawn(move || watch_use(&vm, &monitor));
    }
    // 0: the VM never powers off by itself.
    let idle = (resources.idle_seconds > 0).then(|| Duration::from_secs(resources.idle_seconds));
    let mut askers = Vec::new();
    let mut tick = (std::time::SystemTime::now(), Instant::now());
    let standby = loop {
        match woken.recv_timeout(Duration::from_secs(1)) {
            Ok(Wake::Asked(conn)) => {
                askers.push(conn);
                break false;
            }
            Ok(Wake::Signal(sig)) => {
                note(&format!("signal {sig}"));
                break false;
            }
            Err(_) => {}
        }
        let now = (std::time::SystemTime::now(), Instant::now());
        if slept(tick, now) {
            // The VM's clock and its DHCP lease stood still with the Mac; the lease may have
            // ended meanwhile. `__vm-renew` renews it and sets the clock.
            let vm = vm.clone();
            std::thread::spawn(move || match guest_command(&vm, "__vm-renew") {
                Ok((0, secs)) => note(&format!(
                    "the Mac slept: the VM's address lease renewed for {} s",
                    secs.trim()
                )),
                Ok((_, why)) => note(&format!(
                    "the Mac slept, and the VM's address lease was not renewed: {}",
                    why.trim()
                )),
                Err(e) => note(&format!("the Mac slept; the VM did not answer: {e}")),
            });
        }
        tick = now;
        if !vm.running() {
            note("the VM stopped by itself; see the console log");
            state.stop();
            break true;
        }
        let (quiet, generation) = state.idle(started);
        if idle.is_some_and(|idle| quiet >= idle)
            && !busy(&vm)
            && state.stop_if_unchanged(generation)
        {
            break true;
        }
    };
    // No connection is taken from here on: for a standby, new ones wait in the sockets'
    // queues for the next VM; otherwise the sockets go at once.
    state.stop();
    if standby {
        note("stopping (idle)");
    } else {
        remove_sockets(p);
        note("stopping (asked)");
    }
    power_off(&vm);
    note("stopped");
    let _ = std::fs::remove_file(&p.running);
    drop(askers);
    // A stop asked meanwhile ends the host instead.
    if !standby || woken.try_recv().is_ok() {
        remove_files(p);
        return Ok(());
    }
    // The standby takes the lock and the sockets over; this process ends.
    let fds = Inherited {
        lock: lock.as_raw_fd(),
        docker: api.as_raw_fd(),
        vm: proxy.as_raw_fd(),
    };
    let mut standby = fds
        .spawn("__vm-standby")
        .map_err(|e| format!("cannot start the standby: {e}"))?;
    // Written here too, before the last look at a stop: a `vm stop` from now on finds
    // the standby (and signals it), never this ending process. The state first: a pid is
    // never seen with an older state (`vm stop` reads the pid, then the state).
    write_whole(&p.state, "standby");
    write_whole(&p.pid, &standby.id().to_string());
    // A stop that came while it started ends it too.
    if woken.try_recv().is_ok() {
        // SAFETY: a signal to our own child.
        unsafe { libc::kill(standby.id() as libc::pid_t, libc::SIGTERM) };
        let _ = standby.wait();
        remove_files(p);
    }
    Ok(())
}

/// The VM's MAC address, from the home's path: locally administered and unicast (the
/// first byte 02), the same at every start. A new address at each start would take one
/// more of the 253 addresses the Mac's DHCP server leases for an hour.
pub fn mac_address(home: &Path) -> [u8; 6] {
    // FNV-1a: the same on every build, unlike std's hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in home.as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
    }
    let h = hash.to_be_bytes();
    [0x02, h[0], h[1], h[2], h[3], h[4]]
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

/// What the running VM started with and what it does, in `run/vm.json` for
/// `podbag vm status` (which must not connect: a connection keeps the VM up).
struct Monitor {
    file: PathBuf,
    seen: Mutex<settings::Running>,
}

impl Monitor {
    fn write(&self) {
        let seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(text) = serde_json::to_string(&*seen) {
            write_whole(&self.file, &text);
        }
    }

    fn update(&self, change: impl FnOnce(&mut settings::Running)) {
        {
            let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            let before = seen.clone();
            change(&mut seen);
            if *seen == before {
                return;
            }
        }
        self.write();
    }
}

/// The guest's CPU and memory use, every 2 seconds while the VM runs.
fn watch_use(vm: &Arc<Vm>, monitor: &Monitor) {
    let mut last: Option<(u64, u64)> = None;
    while vm.running() {
        if let Ok((0, text)) = guest_command(vm, "__vm-stats")
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim())
        {
            let (total, idle) = (
                v["cpuTotal"].as_u64().unwrap_or(0),
                v["cpuIdle"].as_u64().unwrap_or(0),
            );
            let percent = last.and_then(|(t0, i0)| cpu_percent((t0, i0), (total, idle)));
            last = Some((total, idle));
            let mem_total = v["memTotalKiB"].as_u64().unwrap_or(0) >> 10;
            let mem_free = v["memAvailableKiB"].as_u64().unwrap_or(0) >> 10;
            monitor.update(|r| {
                if percent.is_some() {
                    r.cpu_percent = percent;
                }
                r.memory_total_mib = Some(mem_total);
                r.memory_used_mib = Some(mem_total.saturating_sub(mem_free));
            });
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// The share of CPU time not idle between two samples (jiffies: all, idle), in percent
/// of all the VM's CPUs, one decimal.
fn cpu_percent(before: (u64, u64), after: (u64, u64)) -> Option<f64> {
    let total = after.0.checked_sub(before.0)?;
    let idle = after.1.checked_sub(before.1)?;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle) as f64 / total as f64 * 100.0;
    Some((busy * 10.0).round() / 10.0)
}

fn start_error(why: String) -> String {
    if why.contains("entitlement") {
        format!(
            "cannot start the VM: {why}\n  fix:   sign podbag with the virtualization \
             entitlement: dev/macos/sign.sh <path to podbag>"
        )
    } else {
        format!("cannot start the VM: {why}")
    }
}

/// How long a started VM may take to answer. It boots in about a third of a second; when the Mac's
/// DHCP server had no address left, the kernel waited 80 to 90 seconds for one.
const BOOT_TIME: Duration = Duration::from_secs(120);

/// Wait until the agent answers, and ask it whether the VM works.
fn wait_ready(vm: &Vm, p: &Paths) -> Result<(), String> {
    let started = Instant::now();
    loop {
        match guest_command(vm, "__vm-check") {
            Ok((0, _)) => return Ok(()),
            Ok((_, said)) => {
                return Err(said.trim().trim_start_matches("podbag: ").to_string());
            }
            // The agent listens about 200 ms after the kernel starts: look often, as a
            // client waits for this (a failed vsock connect costs little).
            Err(_) if started.elapsed() < BOOT_TIME => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                return Err(format!(
                    "the VM started but its agent did not answer within {} seconds; see {}",
                    BOOT_TIME.as_secs(),
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

/// Serve a listener: docker.sock (with `clock`, when the VM's clock was last set) or
/// vm.sock.
fn serve(
    listener: &UnixListener,
    vm: &Arc<Vm>,
    state: &Arc<State>,
    wake: &mpsc::Sender<Wake>,
    clock: Option<&Arc<Mutex<(std::time::SystemTime, Instant)>>>,
    refresh: &mpsc::Sender<()>,
) {
    let _ = listener.set_nonblocking(true);
    loop {
        let mut ready = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd on the stack; woken regularly to see a stop.
        unsafe { libc::poll(&mut ready, 1, 250) };
        // Accepted under the lock the stop is decided under: once the host stops, what
        // comes waits in the socket's queue for the next VM.
        let mut a = state.lock();
        if a.stopping {
            return;
        }
        let Ok((mut stream, _)) = listener.accept() else {
            continue;
        };
        a.open += 1;
        a.generation += 1;
        a.changed = Some(Instant::now());
        drop(a);
        // macOS hands the listener's non-blocking mode to what it accepts.
        let _ = stream.set_nonblocking(false);
        let (vm, state, wake, refresh) = (vm.clone(), state.clone(), wake.clone(), refresh.clone());
        let clock = clock.cloned();
        std::thread::spawn(move || {
            if let Some(clock) = clock {
                set_clock_if_due(&vm, &clock);
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
            // A command that ended may have published a port.
            let _ = refresh.send(());
        });
    }
}

/// The Mac's side of published ports: every second, and right after a client connection
/// closes (a `run -p` that just ended), the published TCP and UDP ports of the running
/// containers, and a socket on the Mac's 127.0.0.1 for each (not on its network). A port
/// the Mac does not give is reported once and tried again.
fn watch_ports(vm: &Arc<Vm>, refresh: &mpsc::Receiver<()>, monitor: &Monitor) {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut open: std::collections::HashMap<(u16, Proto), Arc<AtomicBool>> = Default::default();
    let mut reported = std::collections::HashSet::new();
    loop {
        if let Err(mpsc::RecvTimeoutError::Disconnected) =
            refresh.recv_timeout(Duration::from_secs(1))
        {
            std::thread::sleep(Duration::from_secs(1));
        }
        while refresh.try_recv().is_ok() {}
        let Some((tcp, udp, running)) = published_ports(vm) else {
            continue;
        };
        monitor.update(|r| r.containers = Some(running));
        let wanted: std::collections::HashSet<(u16, Proto)> = tcp
            .into_iter()
            .map(|p| (p, Proto::Tcp))
            .chain(udp.into_iter().map(|p| (p, Proto::Udp)))
            .collect();
        open.retain(|port, stop| {
            let keep = wanted.contains(port);
            if !keep {
                stop.store(true, Ordering::SeqCst);
            }
            keep
        });
        for (port, proto) in wanted {
            if open.contains_key(&(port, proto)) {
                continue;
            }
            match open_port(port, proto, vm) {
                Ok(stop) => {
                    reported.remove(&(port, proto));
                    open.insert((port, proto), stop);
                }
                Err(e) => {
                    if reported.insert((port, proto)) {
                        note(&format!(
                            "cannot forward port {} on this Mac: {}",
                            port_name(port, proto),
                            port_error(&e)
                        ));
                    }
                }
            }
        }
    }
}

/// `8080` for TCP, `5353/udp` for UDP.
fn port_name(port: u16, proto: Proto) -> String {
    match proto {
        Proto::Tcp => port.to_string(),
        Proto::Udp => format!("{port}/udp"),
    }
}

/// Why a Mac port cannot be had, with what to do when another program holds it.
fn port_error(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        format!("{e}: another program on this Mac uses it (stop it, or publish another port)")
    } else {
        e.to_string()
    }
}

/// Bind the Mac's sockets for a published port and serve each, until the returned flag
/// is set.
fn open_port(
    port: u16,
    proto: Proto,
    vm: &Arc<Vm>,
) -> std::io::Result<Arc<std::sync::atomic::AtomicBool>> {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    match proto {
        Proto::Tcp => {
            for (listener, loopback_only) in mac_sockets(port, std::net::TcpListener::bind)? {
                let (vm, stop) = (vm.clone(), stop.clone());
                std::thread::spawn(move || {
                    serve_port(port, &listener, loopback_only, &vm, &stop);
                });
            }
        }
        Proto::Udp => {
            for (socket, loopback_only) in mac_sockets(port, std::net::UdpSocket::bind)? {
                let (vm, stop) = (vm.clone(), stop.clone());
                std::thread::spawn(move || {
                    serve_udp(port, socket, loopback_only, &vm, &stop);
                });
            }
        }
    }
    Ok(stop)
}

/// The published TCP and UDP ports of the running containers, from the VM's Docker API;
/// None when it does not answer.
type Published = (
    std::collections::HashSet<u16>,
    std::collections::HashSet<u16>,
    u64,
);

fn published_ports(vm: &Vm) -> Option<Published> {
    let mut s = UnixStream::from(vm.connect(API_PORT).ok()?);
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(b"GET /containers/json HTTP/1.0\r\n\r\n").ok()?;
    let mut reply = Vec::new();
    s.read_to_end(&mut reply).ok()?;
    let reply = String::from_utf8_lossy(&reply);
    let list: serde_json::Value = serde_json::from_str(reply.split_once("\r\n\r\n")?.1).ok()?;
    let running = list
        .as_array()?
        .iter()
        .filter(|c| c["State"] == "running")
        .count() as u64;
    let ports: Vec<serde_json::Value> = list
        .as_array()?
        .iter()
        .flat_map(|c| c["Ports"].as_array().cloned().unwrap_or_default())
        .collect();
    let of = |kind: &str| {
        ports
            .iter()
            .filter(|p| p["Type"] == kind)
            .filter_map(|p| u16::try_from(p["PublicPort"].as_u64()?).ok())
            .collect()
    };
    Some((of("tcp"), of("udp"), running))
}

/// The Mac's sockets for `port` (TCP listeners or UDP sockets, made by `bind`), each with
/// whether it must turn away callers that are not on this Mac: 127.0.0.1 (macOS gives a
/// port below 1024 there only to root: then every address, with that filter), and ::1
/// when the Mac gives it (`localhost` is ::1 first on macOS, and some clients do not fall
/// back).
fn mac_sockets<S>(
    port: u16,
    bind: impl Fn(std::net::SocketAddr) -> std::io::Result<S>,
) -> std::io::Result<Vec<(S, bool)>> {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
    let v4 = match bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))) {
        Ok(s) => (s, false),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            (bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?, true)
        }
        Err(e) => return Err(e),
    };
    let mut all = vec![v4];
    if let Ok(v6) = bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))) {
        all.push((v6, false));
    }
    Ok(all)
}

/// One Mac port: each connection goes to the VM's forwarder, until `stop`.
fn serve_port(
    port: u16,
    listener: &std::net::TcpListener,
    loopback_only: bool,
    vm: &Arc<Vm>,
    stop: &std::sync::atomic::AtomicBool,
) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let mut ready = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd on the stack; woken regularly to see a stop.
        unsafe { libc::poll(&mut ready, 1, 250) };
        let Ok((client, peer)) = listener.accept() else {
            continue;
        };
        // A low port listens on every address: only this Mac's callers are served.
        if loopback_only && !peer.ip().is_loopback() {
            continue;
        }
        let _ = client.set_nonblocking(false);
        let vm = vm.clone();
        std::thread::spawn(move || forward_to_vm(client, port, &vm));
    }
}

/// A connection to the VM's forwarder, which answered `OK` to `request` (`TCP <port>`
/// or `UDP <port>`).
fn open_forward(vm: &Vm, request: &str) -> Option<UnixStream> {
    let mut guest = UnixStream::from(vm.connect(FORWARD_PORT).ok()?);
    guest.write_all(format!("{request}\n").as_bytes()).ok()?;
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    while answer.len() < 256 {
        match guest.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => answer.push(byte[0]),
            _ => return None,
        }
    }
    (answer == b"OK").then_some(guest)
}

/// A Mac connection to the VM's forwarder for `port`.
fn forward_to_vm(client: std::net::TcpStream, port: u16, vm: &Vm) {
    let Some(guest) = open_forward(vm, &format!("TCP {port}")) else {
        return;
    };
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

/// One Mac UDP socket for `port`, until `stop`: the datagrams of each client go to the VM
/// in a flow of their own, and the replies come back to that client (see
/// [`crate::vm::udp`]).
fn serve_udp(
    port: u16,
    socket: std::net::UdpSocket,
    loopback_only: bool,
    vm: &Arc<Vm>,
    stop: &std::sync::atomic::AtomicBool,
) {
    let socket = Arc::new(socket);
    // A timeout lets the loop see a stop, and end quiet flows.
    let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));
    let mut flows: udp::Flows<Arc<UnixStream>> = udp::Flows::new(udp::MAX_FLOWS);
    let mut swept = Instant::now();
    let mut buf = vec![0u8; udp::MAX_DATAGRAM];
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        if swept.elapsed() >= Duration::from_secs(1) {
            swept = Instant::now();
            flows.expire(swept).iter().for_each(|f| end_flow(f));
        }
        let (n, client) = match socket.recv_from(&mut buf) {
            Ok(got) => got,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        // A low port listens on every address: only this Mac's callers are served.
        if loopback_only && !client.ip().is_loopback() {
            continue;
        }
        let now = Instant::now();
        if flows.get(&client, now).is_none() {
            let last = udp::LastUse::new(now);
            let Some(flow) = open_flow(port, client, &socket, vm, &last) else {
                continue;
            };
            if let Some(old) = flows.add(client, flow, last) {
                end_flow(&old);
            }
        }
        let sent = flows
            .get(&client, now)
            .is_some_and(|flow| udp::write_datagram(&mut &**flow, &buf[..n]).is_ok());
        if !sent && let Some(flow) = flows.remove(&client) {
            end_flow(&flow);
        }
    }
    flows.drain().iter().for_each(|f| end_flow(f));
}

/// A new flow to the VM for `client`, whose replies a thread of its own sends back.
fn open_flow(
    port: u16,
    client: std::net::SocketAddr,
    socket: &Arc<std::net::UdpSocket>,
    vm: &Vm,
    last: &udp::LastUse,
) -> Option<Arc<UnixStream>> {
    let guest = Arc::new(open_forward(vm, &format!("UDP {port}"))?);
    // A VM that stops reading must not hold up the port's other clients.
    guest.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    let (reader, socket, last) = (guest.clone(), socket.clone(), last.clone());
    std::thread::spawn(move || {
        let mut buf = vec![0u8; udp::MAX_DATAGRAM];
        while let Ok(Some(n)) = udp::read_datagram(&mut &*reader, &mut buf) {
            last.touch(Instant::now());
            let _ = socket.send_to(&buf[..n], client);
        }
    });
    Some(guest)
}

/// End a flow: the VM's side ends, and so does the thread of its replies.
fn end_flow(flow: &UnixStream) {
    let _ = flow.shutdown(std::net::Shutdown::Both);
}

/// Docker clients send no clock: before one's request, the VM gets the Mac's when it is
/// due (see [`clock_due`]).
fn set_clock_if_due(vm: &Vm, last: &Mutex<(std::time::SystemTime, Instant)>) {
    let mut last = last.lock().unwrap_or_else(|p| p.into_inner());
    let now = (std::time::SystemTime::now(), Instant::now());
    if clock_due(*last, now) && guest_command(vm, "__vm-check").is_ok() {
        *last = now;
    }
}

/// Whether the Mac slept between two looks: its wall clock ran on while the monotonic
/// clock (which stands still in sleep) did not.
fn slept(last: (std::time::SystemTime, Instant), now: (std::time::SystemTime, Instant)) -> bool {
    let mono = now.1.saturating_duration_since(last.1);
    now.0
        .duration_since(last.0)
        .is_ok_and(|wall| wall > mono + Duration::from_secs(5))
}

/// Whether the VM's clock is set again: 10 seconds after the last time by either clock
/// (macOS's monotonic clock stands still while the Mac sleeps; the wall clock does not),
/// when the two disagree by more than a second, or when the wall clock went back.
fn clock_due(
    last: (std::time::SystemTime, Instant),
    now: (std::time::SystemTime, Instant),
) -> bool {
    let ten = Duration::from_secs(10);
    let mono = now.1.saturating_duration_since(last.1);
    match now.0.duration_since(last.0) {
        Ok(wall) => wall > ten || mono > ten || wall.abs_diff(mono) > Duration::from_secs(1),
        Err(_) => true,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn the_guest_comes_from_the_setting_then_the_binary_then_beside_it_then_home() {
        let home = Path::new("/h");
        let set = Some(PathBuf::from("/set"));
        let beside = Some(PathBuf::from("/beside"));
        let carried = || Some(PathBuf::from("/carried"));
        assert_eq!(
            choose_guest(set, carried, beside.clone(), home),
            Path::new("/set")
        );
        assert_eq!(
            choose_guest(None, carried, beside.clone(), home),
            Path::new("/carried")
        );
        assert_eq!(
            choose_guest(None, || None, beside, home),
            Path::new("/beside")
        );
        assert_eq!(choose_guest(None, || None, None, home), Path::new("/h/vm"));
    }

    #[test]
    fn cpu_use_between_two_samples() {
        assert_eq!(cpu_percent((1000, 800), (2000, 1550)), Some(25.0));
        assert_eq!(cpu_percent((1000, 800), (1000, 800)), None);
        assert_eq!(cpu_percent((1000, 800), (900, 700)), None);
        assert_eq!(cpu_percent((0, 0), (3, 2)), Some(33.3));
    }

    #[test]
    fn the_clock_is_due_after_ten_seconds_or_a_sleep() {
        let (wall, mono) = (SystemTime::now(), Instant::now());
        let later = |w: u64, m: u64| (wall + Duration::from_secs(w), mono + Duration::from_secs(m));
        let due = |(w, m): (SystemTime, Instant)| clock_due((wall, mono), (w, m));
        assert!(!due(later(5, 5)));
        assert!(due(later(11, 11)));
        // The Mac slept: its monotonic clock stood still, the wall clock did not.
        assert!(due(later(1800, 7)));
        // The wall clock went back.
        assert!(due((
            wall - Duration::from_secs(60),
            mono + Duration::from_secs(3)
        )));
    }
}

#[cfg(test)]
mod wake_tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn the_mac_slept_when_the_wall_clock_runs_ahead() {
        let (wall, mono) = (SystemTime::now(), Instant::now());
        let at = |w: u64, m: u64| (wall + Duration::from_secs(w), mono + Duration::from_secs(m));
        assert!(!slept((wall, mono), at(1, 1)));
        assert!(!slept((wall, mono), at(3, 1)));
        // Asleep for an hour: the monotonic clock stood still.
        assert!(slept((wall, mono), at(3601, 1)));
    }
}
