//! The shim's Linux side: create the container, then serve it until it ends.
//!
//! One poll loop over: the container's exit (pidfd), its output, the control socket, and
//! signals (through a self-pipe). Two helper threads do the writes that can block for a
//! long time: output to an attached caller (`Echo`) and the caller's stdin into the
//! container's terminal. So a slow reader or a container that does not read its input
//! cannot stop the loop from serving stop, kill and signals.

use super::control::{self, Request, Response};
use super::logbook::{LogWriter, Stream};
use crate::helper::{StatusChannel, StatusMessage};
use crate::paths::Dirs;
use crate::spec::ContainerSpec;
use crate::state::{self, State, Status};
use crate::{Error, Result, cgroup, linux};
use rustix::pipe::{PipeFlags, pipe_with};
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Signals the shim handles: forwarded to the container, or SIGWINCH for terminal size.
pub(super) const HANDLED: [i32; 7] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGWINCH,
];

/// How long to keep reading output after the container exits.
pub(crate) const DRAIN: Duration = Duration::from_secs(2);

/// While this much output waits for a slow caller, the shim stops reading the container's
/// output, so the container waits too (as it would writing to a slow terminal).
const ECHO_LIMIT: usize = 1 << 20;

pub(super) struct Outcome {
    pub code: i32,
    pub started: bool,
    /// The keeper watches the container from here on: nothing to clean up in this process.
    pub handed_over: bool,
}

/// Leave the caller: fork (the caller reaps the parent at once), new session, no terminal.
pub(super) fn detach() -> Result<()> {
    // SAFETY: fork in a single-threaded process.
    match unsafe { libc::fork() } {
        -1 => {
            return Err(Error::io(
                "cannot start the container in the background",
                &std::io::Error::last_os_error(),
                "report this bug",
            ));
        }
        0 => {}
        // SAFETY: the parent leaves without running destructors; the child carries on.
        _ => unsafe { libc::_exit(0) },
    }
    // SAFETY: plain system calls on our own descriptors.
    unsafe {
        libc::setsid();
        if let Ok(null) = File::options().read(true).write(true).open("/dev/null") {
            for fd in 0..3 {
                libc::dup2(null.as_raw_fd(), fd);
            }
        }
    }
    Ok(())
}

/// The container's side of its standard streams, and the shim's side.
struct Io {
    stdio: linux::ContainerStdio,
    /// Descriptors only the container needs; the shim closes them after the spawn.
    child_ends: Vec<OwnedFd>,
    outputs: Vec<Output>,
    /// The terminal's controlling side, kept open for the container's life.
    master: Option<OwnedFd>,
    /// With `-t`: where the terminal arrives, whether the caller is attached, and `-i`.
    terminal_from: Option<(OwnedFd, bool, bool)>,
}

struct Output {
    fd: OwnedFd,
    stream: Stream,
    /// Also copy to this descriptor of the shim (1 or 2) when the caller is attached.
    echo: Option<i32>,
}

impl Io {
    fn prepare(spec: &ContainerSpec) -> Result<Io> {
        let attached = !spec.detach;
        if spec.tty {
            // The container creates its terminal in its own /dev/pts and sends the
            // controlling side back on this socket; see `Io::receive_terminal`.
            let (shim_end, child_end) = socket_pair()?;
            return Ok(Io {
                stdio: linux::ContainerStdio {
                    fds: [-1; 3],
                    tty: true,
                    tty_socket: Some(child_end.as_raw_fd()),
                },
                child_ends: vec![child_end],
                outputs: Vec::new(),
                master: None,
                terminal_from: Some((shim_end, attached, spec.interactive)),
            });
        }
        let pipe = || {
            pipe_with(PipeFlags::CLOEXEC).map_err(|e| {
                Error::io(
                    "cannot set up the container's output",
                    &e.into(),
                    "check the open file limit (`ulimit -n`)",
                )
            })
        };
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        // The input: a named pipe (whoever writes to it holds it open), the caller's stdin,
        // or nothing.
        let null: OwnedFd = match &spec.stdin_fifo {
            Some(path) => open_fifo(path)?,
            None => File::open("/dev/null")
                .map_err(|e| Error::io("cannot open /dev/null", &e, "check the system"))?
                .into(),
        };
        let stdin = if spec.interactive && spec.stdin_fifo.is_none() {
            0
        } else {
            null.as_raw_fd()
        };
        Ok(Io {
            stdio: linux::ContainerStdio {
                fds: [stdin, out_w.as_raw_fd(), err_w.as_raw_fd()],
                tty: false,
                tty_socket: None,
            },
            child_ends: vec![null, out_w, err_w],
            outputs: vec![
                Output {
                    fd: out_r,
                    stream: Stream::Stdout,
                    echo: attached.then_some(1),
                },
                Output {
                    fd: err_r,
                    stream: Stream::Stderr,
                    echo: attached.then_some(2),
                },
            ],
            master: None,
            terminal_from: None,
        })
    }

    /// After the spawn: take the terminal the container sent, and serve it.
    fn receive_terminal(&mut self) -> Result<()> {
        let Some((socket, attached, interactive)) = self.terminal_from.take() else {
            return Ok(());
        };
        let master = linux::pty::recv_fd(socket.as_raw_fd()).map_err(|e| {
            Error::io(
                "cannot set up the container terminal",
                &e,
                "report this bug",
            )
        })?;
        // A terminal of the caller's own is the better guess (the container's init set
        // the size the settings ask for).
        if attached {
            linux::pty::copy_size(0, master.as_raw_fd());
        }
        let reader = master.try_clone().map_err(|e| {
            Error::io(
                "cannot set up the container terminal",
                &e,
                "report this bug",
            )
        })?;
        self.outputs.push(Output {
            fd: reader,
            stream: Stream::Stdout,
            echo: attached.then_some(1),
        });
        if attached && interactive {
            let to = master.try_clone().map_err(|e| {
                Error::io(
                    "cannot set up the container terminal",
                    &e,
                    "report this bug",
                )
            })?;
            relay_stdin(to);
        }
        self.master = Some(master);
        Ok(())
    }
}

/// Copy stdin into the container's terminal until stdin ends. On its own thread, because
/// the write blocks while the container does not read its input.
fn relay_stdin(to: OwnedFd) {
    std::thread::spawn(move || {
        let mut to = File::from(to);
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            // SAFETY: reading into a valid buffer from our stdin.
            let got = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if got < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            if got <= 0 || to.write_all(&buf[..got as usize]).is_err() {
                return;
            }
        }
    });
}

/// Output on its way to an attached caller, written by a thread of its own.
struct Echo {
    shared: Arc<(Mutex<EchoQueue>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct EchoQueue {
    chunks: VecDeque<(i32, Vec<u8>)>,
    bytes: usize,
    closed: bool,
}

impl Echo {
    fn start() -> Echo {
        let shared = Arc::new((Mutex::new(EchoQueue::default()), Condvar::new()));
        let queue = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            let (lock, ready) = &*queue;
            // A caller that went away (EPIPE) gets no more output; the rest is dropped.
            let mut gone = [false; 3];
            loop {
                let (fd, data) = {
                    let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(chunk) = q.chunks.pop_front() {
                            q.bytes -= chunk.1.len();
                            break chunk;
                        }
                        if q.closed {
                            return;
                        }
                        q = ready.wait(q).unwrap_or_else(|e| e.into_inner());
                    }
                };
                let slot = fd as usize % 3;
                if !gone[slot] {
                    // SAFETY: borrowing a descriptor the caller passed us, without closing it.
                    let mut out = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
                    gone[slot] = out.write_all(&data).is_err();
                }
            }
        });
        Echo {
            shared,
            thread: Some(thread),
        }
    }

    fn push(&self, fd: i32, data: &[u8]) {
        let (lock, ready) = &*self.shared;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        q.bytes += data.len();
        q.chunks.push_back((fd, data.to_vec()));
        ready.notify_one();
    }

    fn full(&self) -> bool {
        let q = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        q.bytes >= ECHO_LIMIT
    }

    /// Write out everything queued, however long the caller takes to read it.
    fn finish(mut self) {
        {
            let (lock, ready) = &*self.shared;
            lock.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
            ready.notify_one();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn socket_pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: socketpair writes two new descriptors into a valid array.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(Error::io(
            "cannot set up the container terminal",
            &std::io::Error::last_os_error(),
            "report this bug",
        ));
    }
    // SAFETY: two fresh descriptors that nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The memory budget in bytes (`None`: no limit), from the settings and the host's memory.
fn memory_budget(dirs: &Dirs) -> Result<Option<u64>> {
    let setting = crate::config::load(dirs)?.memory_budget;
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| cgroup::mem_total(&m))
        .unwrap_or(0);
    Ok(setting.bytes(total).filter(|&b| b > 0))
}

pub(super) fn run(
    spec: &ContainerSpec,
    dirs: &Dirs,
    status: &mut StatusChannel,
    lock: &File,
) -> Outcome {
    let failed = |status: &mut StatusChannel, e: Error| Outcome {
        code: status.fail(e),
        started: false,
        handed_over: false,
    };
    let id = spec.id.as_str();
    // A detached container of the keeper needs no process of its own: the keeper becomes
    // its parent and takes over the watching once it runs. Published ports
    // are relayed by a shim, so such a container keeps this process.
    let hand_over = spec.detach && crate::keeper::under_keeper() && spec.publish.is_empty();
    let mut io = match Io::prepare(spec) {
        Ok(io) => io,
        Err(e) => return failed(status, e),
    };
    let mut log = match LogWriter::for_container(dirs, spec) {
        Ok(l) => l,
        Err(e) => return failed(status, e),
    };
    let listener = match control::bind(dirs, id) {
        Ok(l) => l,
        Err(e) => return failed(status, e),
    };
    let (sig_r, sig_w) = match pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK) {
        Ok(p) => p,
        Err(e) => {
            return failed(
                status,
                Error::io(
                    "cannot set up signal handling",
                    &e.into(),
                    "report this bug",
                ),
            );
        }
    };

    // After a reboot the image's mount is gone (it lives in the runtime directory).
    if let Err(e) = crate::store::ensure_image_mount(dirs, &spec.rootfs) {
        remove_control(dirs, id);
        return failed(status, e);
    }
    let budget = match memory_budget(dirs) {
        Ok(b) => b,
        Err(e) => {
            remove_control(dirs, id);
            return failed(status, e);
        }
    };
    let group = match crate::keeper::cgroups::create(&spec.cgroup, id, &spec.limits, budget) {
        Ok(g) => g,
        Err(e) => {
            remove_control(dirs, id);
            return failed(status, e);
        }
    };
    // SAFETY: geteuid has no preconditions. (Read before joining a user namespace.)
    let rootless = unsafe { libc::geteuid() } != 0;
    // As root, the container may open only the usual devices (a privileged one: all).
    // Rootless, the kernel refuses device nodes to the user namespace already.
    if let (false, false, Some(g)) = (rootless, spec.security.privileged, &group) {
        let rules = crate::cgroup::device_rules(&spec.devices);
        let restricted = rules.and_then(|rules| {
            crate::cgroup::restrict_devices(g.fd(), &rules).map_err(|e| {
                Error::io(
                    "cannot limit the devices the container may open",
                    &e,
                    "check that the kernel has cgroup BPF (CONFIG_CGROUP_BPF)",
                )
            })
        });
        if let Err(e) = restricted {
            remove_control(dirs, id);
            if let Some(g) = group {
                g.remove();
            }
            return failed(status, e);
        }
    }
    let bridged = matches!(spec.network, crate::network::NetworkMode::Bridge { .. });
    let session = if rootless && bridged {
        match super::net::join_session(dirs, spec) {
            Ok(pid) => Some(pid),
            Err(e) => {
                remove_control(dirs, id);
                if let Some(g) = group {
                    g.remove();
                }
                return failed(status, e);
            }
        }
    } else {
        None
    };
    let userns = match (rootless, session) {
        (false, _) => linux::UserNs::Host,
        (true, Some(_)) => linux::UserNs::Joined,
        (true, None) => linux::UserNs::New,
    };
    // Undo what a failed start made.
    let undo = |group: Option<cgroup::Group>| {
        remove_control(dirs, id);
        if let Some(g) = group {
            g.remove();
        }
        super::net::detach(dirs, spec, session.is_some());
    };
    // A bridge container's addresses, then its /etc files (first among its mounts).
    let extra_hosts = match super::net::extra_hosts(dirs, spec, rootless) {
        Ok(e) => e,
        Err(e) => {
            undo(group);
            return failed(status, e);
        }
    };
    let addresses = match super::net::bridge_addresses(dirs, spec, &extra_hosts) {
        Ok(a) => a,
        Err(e) => {
            undo(group);
            return failed(status, e);
        }
    };
    let own = match super::net::own(dirs, spec, &addresses, &extra_hosts) {
        Ok(own) => own,
        Err(e) => {
            undo(group);
            return failed(status, e);
        }
    };
    let mut run_spec = spec.clone();
    match super::net::files(spec, dirs, rootless, own) {
        Ok(mut files) => {
            files.append(&mut run_spec.mounts);
            run_spec.mounts = files;
            run_spec.mounts.extend(device_mounts(&spec.devices));
        }
        Err(e) => {
            undo(group);
            return failed(status, e);
        }
    }
    let mut attach = |pid: u32| super::net::attach(spec, &addresses, session, pid);
    let child = match linux::spawn(
        &run_spec,
        io.stdio,
        group.as_ref().map(|g| g.fd()),
        userns,
        if hand_over {
            linux::Parent::Grandparent
        } else {
            linux::Parent::Caller
        },
        &mut attach,
    ) {
        Ok(c) => c,
        Err(e) => {
            undo(group);
            return failed(status, e);
        }
    };
    io.child_ends.clear();
    if let Err(e) = io.receive_terminal() {
        // SAFETY: killing our own child, which runs but has no terminal to talk on.
        unsafe { libc::kill(child.pid as libc::pid_t, libc::SIGKILL) };
        let _ = linux::wait(child.pid);
        undo(group);
        return failed(status, e);
    }
    // Published ports: bound now, so a taken port fails the start.
    let published = match super::proxy::open(&spec.publish) {
        Ok(p) => p,
        Err(e) => {
            // SAFETY: killing our own child; nothing is published for it.
            unsafe { libc::kill(child.pid as libc::pid_t, libc::SIGKILL) };
            let _ = linux::wait(child.pid);
            undo(group);
            return failed(status, e);
        }
    };
    // SAFETY: pidfd_open on our own child; the result is a new descriptor or -1.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.pid as libc::pid_t, 0) };
    // SAFETY: a fresh descriptor that nothing else owns.
    let pidfd = (pidfd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(pidfd as i32) });

    let created = state::read_state(dirs, id)
        .map(|s| s.created)
        .unwrap_or_else(|_| state::now());
    // Each start begins a new health.
    crate::health::reset(dirs, id);
    let st = State {
        status: Status::Running,
        pid: Some(child.pid),
        // The process that watches it: this one, or the keeper (this starter's parent).
        shim_pid: Some(if hand_over {
            // SAFETY: getppid has no preconditions.
            unsafe { libc::getppid() as u32 }
        } else {
            std::process::id()
        }),
        exit_code: None,
        created,
        started: Some(state::now()),
        finished: None,
        cgroup: group
            .as_ref()
            .map(|g| g.path().to_string_lossy().into_owned()),
        oom: None,
        ip: addresses.first().map(|(_, ip)| ip.ipv4.to_string()),
        network_inode: if addresses.is_empty() {
            None
        } else {
            use std::os::unix::fs::MetadataExt;
            linux::net::namespace(child.pid)
                .and_then(|ns| ns.metadata())
                .ok()
                .map(|metadata| metadata.ino())
        },
        ports: published.iter().map(|(m, _)| *m).collect(),
        paused: false,
    };
    let _ = state::write_state(dirs, id, &st);
    super::net::announce(dirs, spec);
    if hand_over {
        // The listener, the lock and the output go to the keeper; this process ends.
        let mut fds = vec![listener.as_raw_fd(), lock.as_raw_fd()];
        match &io.master {
            Some(master) => fds.push(master.as_raw_fd()),
            None => fds.extend(io.outputs.iter().map(|o| o.fd.as_raw_fd())),
        }
        return match crate::keeper::handover::send(dirs, id, child.pid, spec.tty, &fds) {
            Ok(()) => {
                status.report(&StatusMessage::Started { pid: child.pid });
                Outcome {
                    code: 0,
                    started: true,
                    handed_over: true,
                }
            }
            Err(e) => {
                // The keeper is gone or cannot take it: the container cannot stay.
                // SAFETY: a signal to the container this process just made.
                unsafe { libc::kill(child.pid as libc::pid_t, libc::SIGKILL) };
                finish(dirs, spec, st, 137, None, session.is_some(), listener, &[]);
                if let Some(g) = group {
                    g.remove();
                }
                failed(status, e)
            }
        };
    }
    let relays = addresses
        .first()
        .map(|(_, ip)| super::proxy::serve(published, ip.ipv4, session));

    let echo = io
        .outputs
        .iter()
        .any(|o| o.echo.is_some())
        .then(Echo::start);
    crate::signals::notify_pipe(sig_w.as_raw_fd(), &HANDLED);
    status.report(&StatusMessage::Started { pid: child.pid });

    let (code, waiters) = serve(
        id,
        child.pid,
        pidfd.as_ref(),
        &mut io,
        &mut log,
        echo.as_ref(),
        &listener,
        &sig_r,
        spec.health.as_ref(),
    );
    // Published ports close first: the address goes back to the network below.
    if let Some(relays) = relays {
        relays.close();
    }
    let _ = log.flush_partial();
    let oom = group.as_ref().and_then(|g| g.oom_reason());
    // Without a PID namespace of its own, its other processes outlive it: they go too.
    if let (Some(g), false) = (&group, spec.namespaces.pid == crate::spec::Namespace::Own) {
        cgroup::kill_all(g.path());
    }
    finish(
        dirs,
        spec,
        st,
        code,
        oom,
        session.is_some(),
        listener,
        &waiters,
    );
    if let Some(echo) = echo {
        echo.finish();
    }
    if let Some(group) = group {
        let path = group.path().to_string_lossy().into_owned();
        if group.remove() {
            state::forget_cgroup(dirs, id, &path);
        }
    }
    // Rootless, this process may be in the network session's user namespace and in the
    // container's systemd scope: what it starts would stay in both.
    let relaunch = if rootless
        && (session.is_some() || matches!(spec.cgroup, cgroup::Placement::Scope { .. }))
    {
        crate::restart::Relaunch::UserManager
    } else {
        crate::restart::Relaunch::Direct
    };
    crate::restart::after_exit(dirs, spec, code, relaunch);
    Outcome {
        code,
        started: true,
        handed_over: false,
    }
}

/// Open a named pipe for reading without waiting for a writer, then make reads wait.
fn open_fifo(path: &str) -> Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| {
            Error::io(
                format!("cannot open the input pipe {path}"),
                &e,
                "report this bug",
            )
        })?;
    // SAFETY: fcntl on a descriptor we own; the flags are plain integers.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_SETFL,
            flags.max(0) & !libc::O_NONBLOCK,
        );
    }
    Ok(file.into())
}

/// The device nodes a container was given, bound from the host's.
fn device_mounts(devices: &[crate::spec::Device]) -> Vec<crate::volume::Mount> {
    devices
        .iter()
        .map(|d| crate::volume::Mount {
            source: crate::volume::MountSource::Bind {
                path: d.host.clone(),
                propagation: Default::default(),
                shallow_ro: false,
            },
            target: d.container.clone(),
            read_only: false,
            copy_up: false,
        })
        .collect()
}

/// What follows a container's end, for the shim and for the keeper's finisher: the state,
/// the network address, the waiters, the control socket.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish(
    dirs: &Dirs,
    spec: &ContainerSpec,
    mut st: State,
    code: i32,
    oom: Option<cgroup::OomReason>,
    session: bool,
    listener: UnixListener,
    waiters: &[UnixStream],
) {
    st.status = Status::Exited;
    st.network_inode = None;
    st.paused = false;
    st.exit_code = Some(code);
    st.oom = oom;
    st.finished = Some(state::now());
    {
        // Under the state lock: a pause that read "running" cannot write it back now.
        let _lock = state::lock_state(dirs, &spec.id);
        let _ = state::write_state(dirs, &spec.id, &st);
    }
    // Give the address back before anyone hears of the exit: they may reuse it at once.
    super::net::detach(dirs, spec, session);
    // Answer waiters only after the state says "exited": they may act on it at once.
    for w in waiters {
        control::reply(w, &Response::Exited { exit_code: code });
    }
    drop(listener);
    remove_control(dirs, &spec.id);
}

#[derive(Clone, Copy)]
enum Source {
    Exit,
    Signal,
    Control,
    Output(usize),
    /// The health check that runs now ended.
    Probe,
}

/// The poll loop. Returns the container's exit code and the connections waiting for it.
#[allow(clippy::too_many_arguments)]
fn serve(
    id: &str,
    pid: u32,
    pidfd: Option<&OwnedFd>,
    io: &mut Io,
    log: &mut LogWriter,
    echo: Option<&Echo>,
    listener: &UnixListener,
    sig_r: &OwnedFd,
    health: Option<&crate::health::HealthConfig>,
) -> (i32, Vec<UnixStream>) {
    let mut exit: Option<i32> = None;
    let mut drain_until: Option<Instant> = None;
    let mut waiters: Vec<UnixStream> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut schedule = health.map(|c| crate::health::Schedule::new(c, Instant::now()));
    // The running probe's pidfd: its end wakes the loop.
    let mut probe_fd: Option<OwnedFd> = None;

    loop {
        if exit.is_some()
            && (io.outputs.is_empty() || drain_until.is_some_and(|d| Instant::now() >= d))
        {
            break;
        }
        let mut fds: Vec<libc::pollfd> = Vec::new();
        let mut sources: Vec<Source> = Vec::new();
        let mut watch = |fd: i32, s: Source| {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            sources.push(s);
        };
        if exit.is_none() {
            match pidfd {
                Some(p) => watch(p.as_raw_fd(), Source::Exit),
                None => {
                    // No pidfd (old kernel): check with waitpid on every wake-up.
                    exit = try_reap(pid);
                }
            }
            watch(sig_r.as_raw_fd(), Source::Signal);
        }
        watch(listener.as_raw_fd(), Source::Control);
        // A slow caller holds back reading while the container runs. After the exit,
        // read the rest anyway: it is bounded by the pipe size.
        let held = exit.is_none() && echo.is_some_and(Echo::full);
        if !held {
            for (i, o) in io.outputs.iter().enumerate() {
                watch(o.fd.as_raw_fd(), Source::Output(i));
            }
        }
        if let Some(fd) = &probe_fd {
            watch(fd.as_raw_fd(), Source::Probe);
        }
        let probing = schedule.as_ref().and_then(|s| s.probe()).is_some();
        let mut timeout = if held {
            20
        } else if exit.is_some() || pidfd.is_none() || (probing && probe_fd.is_none()) {
            100
        } else {
            -1
        };
        // The next check, or a check that hangs.
        if let (None, Some(s)) = (exit, &schedule) {
            let wait = s
                .next_at()
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, 60_000) as i32;
            timeout = if timeout < 0 { wait } else { timeout.min(wait) };
        }
        // SAFETY: fds is a valid array of pollfd of the given length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            continue; // EINTR
        }

        let mut closed = Vec::new();
        for (p, s) in fds.iter().zip(&sources) {
            if p.revents == 0 {
                continue;
            }
            match *s {
                Source::Exit => {
                    exit = Some(linux::wait(pid).unwrap_or(-1));
                    drain_until = Some(Instant::now() + DRAIN);
                }
                Source::Signal => {
                    let mut sigs = [0u8; 64];
                    // SAFETY: reading into a valid buffer from our non-blocking pipe.
                    let got = unsafe {
                        libc::read(sig_r.as_raw_fd(), sigs.as_mut_ptr().cast(), sigs.len())
                    };
                    for &sig in sigs.iter().take(got.max(0) as usize) {
                        let sig = i32::from(sig);
                        if sig == libc::SIGWINCH {
                            if let Some(master) = &io.master {
                                linux::pty::copy_size(0, master.as_raw_fd());
                            }
                        } else if exit.is_none() {
                            let _ = kill_container(pid, sig);
                        }
                    }
                }
                Source::Control => {
                    let master = io.master.as_ref().map(|m| m.as_raw_fd());
                    accept_requests(listener, id, pid, exit, master, &mut waiters)
                }
                Source::Probe => probe_fd = None,
                Source::Output(i) => {
                    let o = &io.outputs[i];
                    // SAFETY: reading into a valid buffer from a descriptor we own.
                    let got =
                        unsafe { libc::read(o.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                    if got <= 0 {
                        // End of output, or EIO from a terminal whose other side closed.
                        closed.push(i);
                        continue;
                    }
                    let data = &buf[..got as usize];
                    let _ = log.write(o.stream, data);
                    if let (Some(fd), Some(echo)) = (o.echo, echo) {
                        echo.push(fd, data);
                    }
                }
            }
        }
        for i in closed.into_iter().rev() {
            io.outputs.remove(i);
        }
        if let Some(s) = schedule.as_mut() {
            let now = Instant::now();
            if let Some(probe) = s.probe() {
                if exit.is_some() {
                    // The container ended: its check goes with it.
                    // SAFETY: a signal to our own child, not reaped yet.
                    unsafe { libc::kill(probe as libc::pid_t, libc::SIGKILL) };
                    let _ = linux::wait(probe);
                    s.ended(probe, now);
                    probe_fd = None;
                } else if try_reap(probe).is_some() {
                    s.ended(probe, now);
                    probe_fd = None;
                }
            }
            if exit.is_none() {
                let hung = s.tick(now, |in_start_period| {
                    crate::health::spawn_probe(id, in_start_period, None)
                });
                if let Some(pid) = hung {
                    // SAFETY: a signal to our own child, not reaped yet.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                }
                if let (Some(pid), None) = (s.probe(), &probe_fd) {
                    // SAFETY: pidfd_open on our own child; a new descriptor or -1.
                    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
                    // SAFETY: a fresh descriptor that nothing else owns.
                    probe_fd = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) });
                }
            }
        }
    }

    (exit.unwrap_or(-1), waiters)
}

fn try_reap(pid: u32) -> Option<i32> {
    let mut status = 0;
    // SAFETY: non-blocking waitpid on our own child.
    let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    (rc == pid as libc::pid_t).then(|| {
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            128 + libc::WTERMSIG(status)
        }
    })
}

pub(crate) fn accept_requests(
    listener: &UnixListener,
    id: &str,
    pid: u32,
    exit: Option<i32>,
    master: Option<i32>,
    waiters: &mut Vec<UnixStream>,
) {
    while let Ok((stream, _)) = listener.accept() {
        let _ = stream.set_nonblocking(false);
        let request = control::read_request(&stream);
        handle_request(stream, request, id, pid, exit, master, waiters);
    }
}

/// Answer one request that came on `stream`. A `wait` is answered when the container has
/// ended: its stream joins `waiters`.
pub(crate) fn handle_request(
    stream: UnixStream,
    request: Option<Request>,
    id: &str,
    pid: u32,
    exit: Option<i32>,
    master: Option<i32>,
    waiters: &mut Vec<UnixStream>,
) {
    match request {
        Some(Request::Kill { signal }) => {
            let response = match exit {
                Some(_) => Response::Failed {
                    error: control::not_running(id),
                },
                None => match kill_container(pid, signal) {
                    Ok(()) => Response::Ok { ok: true },
                    Err(e) => Response::Failed {
                        error: Error::io(
                            "cannot signal the container",
                            &e,
                            "check the signal number",
                        ),
                    },
                },
            };
            control::reply(&stream, &response);
        }
        // Answered when the state says "exited", even if the exit is known already:
        // the caller may read the state and the log as soon as it hears.
        Some(Request::Wait) => waiters.push(stream),
        Some(Request::Resize { rows, cols }) => {
            let response = match (exit, master) {
                (Some(_), _) => Response::Failed {
                    error: control::not_running(id),
                },
                (None, Some(m)) => {
                    linux::pty::set_size(m, rows, cols);
                    Response::Ok { ok: true }
                }
                (None, None) => Response::Ok { ok: true },
            };
            control::reply(&stream, &response);
        }
        None => {}
    }
}

/// Signal the container's main process. The shim created the container's user
/// namespace, so it holds CAP_KILL there and can signal any container user directly.
fn kill_container(pid: u32, sig: i32) -> std::io::Result<()> {
    // SAFETY: kill has no memory arguments.
    if unsafe { libc::kill(pid as libc::pid_t, sig) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn remove_control(dirs: &Dirs, id: &str) {
    control::remove(dirs, id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    // A waiter must hear about the exit only after the state and the log are written
    // (`run` answers the queued waiters then), even when the exit is already known.
    #[test]
    fn wait_during_the_drain_is_answered_later() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"{\"op\":\"wait\"}\n").unwrap();
        let mut waiters = Vec::new();
        accept_requests(&listener, "x", 1, Some(4), None, &mut waiters);
        assert_eq!(waiters.len(), 1);
        client.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 64];
        let early = client.read(&mut buf);
        assert!(
            matches!(&early, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "answered early: {early:?}"
        );
    }
}
