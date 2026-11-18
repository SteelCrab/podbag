//! The keeper process: a single-threaded epoll loop.
//!
//! It must not panic on anything it reads: a release build aborts on a panic, and the
//! keeper's end is the end of its containers. It must not wait for a peer either: one
//! slow connection would stop the logs and the requests of every container. Requests are
//! read when they arrive, piece by piece, with a size limit and a deadline.

use super::handover::KEEPER_FD;
use super::proto::{self, Handover, Incoming, Progress, Reply, Request};
use super::{LOCK, SOCKET, UNDER_KEEPER};
use crate::helper::{ShimMode, StatusChannel, StatusMessage};
use crate::paths::Dirs;
use crate::shim::control;
use crate::shim::logbook::{LOG_FILE_LIMIT, LogWriter, Stream};
use crate::{Error, ErrorKind};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const TOKEN_LISTENER: u64 = 0;
const TOKEN_SIGNAL: u64 = 1;
/// The kind of a token is in its top byte; the rest names the thing.
const KIND: u64 = 0xff << 56;
/// A client's connection after its request: the starter's PID.
const TOKEN_CLIENT: u64 = 1 << 56;
/// A starter's handover socket: the starter's PID.
const TOKEN_HANDOVER: u64 = 2 << 56;
/// A kept container's control listener: its slot.
const TOKEN_CONTROL: u64 = 3 << 56;
/// A kept container's output: its slot, then the output's number in the low byte.
const TOKEN_OUTPUT: u64 = 4 << 56;
/// A connection whose request has not arrived in full: its place in `pending`.
const TOKEN_PENDING: u64 = 5 << 56;

/// How long a new keeper waits for the request of the client that started it.
const FIRST_REQUEST: Duration = Duration::from_secs(5);
/// How long kept containers get to end after a SIGTERM for the keeper.
const TERM_GRACE: Duration = Duration::from_secs(10);
/// Exits of children nobody has claimed yet are remembered this long (a container can
/// end before its starter has handed it over).
const EARLY_EXIT_KEPT: Duration = Duration::from_secs(30);
/// How long a connection has to send its whole request.
const REQUEST_LIMIT: Duration = Duration::from_secs(2);
/// The most connections that may be in the middle of a request.
const MAX_PENDING: usize = 128;

/// A fork of the keeper that runs the shim code for one start.
struct Starter {
    pid: u32,
    /// The client that asked for it, while it listens.
    conn: Option<UnixStream>,
    /// Where it hands a detached container over.
    handover: Option<UnixStream>,
}

/// A detached container that the keeper watches: its child, with no process of its own.
struct Kept {
    id: String,
    pid: u32,
    dirs: Dirs,
    /// With a terminal: `outputs[0]` is its controlling side.
    tty: bool,
    /// stdout and stderr (or the terminal), until each ends.
    outputs: [Option<OwnedFd>; 2],
    listener: UnixListener,
    /// The container's shim lock: held while it is watched.
    lock: OwnedFd,
    /// None when the log cannot be written: the output is dropped, the container runs on.
    log: Option<LogWriter>,
    waiters: Vec<UnixStream>,
    /// Connections whose request was on its way when the container ended: the finisher
    /// answers them (a connection closed before the state says "exited" would tell its
    /// client that the container vanished).
    late: Vec<UnixStream>,
    exit: Option<i32>,
    /// After the exit: how long to keep reading output that others still hold open.
    drain_until: Option<Instant>,
    /// Its health checks, while it runs.
    health: Option<crate::health::Schedule>,
    /// The environment its checks get: its client's settings.
    probe_env: Vec<(String, String)>,
}

/// What a connection's request will be for.
#[derive(Clone, Copy)]
enum Purpose {
    /// A start, on the keeper's socket.
    Start,
    /// A request for the container in `slot` (`pid` tells it from a later one there).
    Control { slot: usize, pid: u32 },
}

/// A connection whose request has not arrived in full.
struct Pending {
    conn: UnixStream,
    purpose: Purpose,
    /// A start request so far, with its descriptors.
    incoming: Incoming,
    /// A control request's line so far.
    line: Vec<u8>,
    /// When the connection is dropped, request or not.
    until: Instant,
}

struct Keeper {
    epoll: OwnedFd,
    listener: UnixListener,
    socket_path: PathBuf,
    /// The inode of the socket this keeper bound: another file there means it was replaced.
    socket_inode: u64,
    sig_r: OwnedFd,
    /// The build of the program this keeper runs (`build_of`): clients of another one
    /// are refused.
    build: String,
    starters: Vec<Starter>,
    containers: Vec<Option<Kept>>,
    pending: Vec<Option<Pending>>,
    /// Forks that run the cleanup of ended containers.
    finishers: Vec<u32>,
    /// Children that ended before anyone claimed them: PID, exit code, when.
    early: Vec<(u32, i32, Instant)>,
    /// Health checks killed with their container, not reaped yet.
    dead_probes: Vec<u32>,
    /// One buffer for all output.
    buf: Vec<u8>,
    /// After a SIGTERM: when the containers that still run are killed.
    kill_at: Option<Instant>,
    linger: Duration,
    idle_since: Instant,
    /// Until the first request, the keeper stays at least this long: the client that
    /// started it is on its way.
    first_by: Option<Instant>,
    terminating: bool,
}

fn os_error(what: &str, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot {what} the keeper"),
        &e,
        "check the openpod runtime directory",
    )
}

/// `openpod __keeper <runtime dir>`: become the keeper of this runtime directory, unless
/// one runs already. Reports `Started` (with PID 0 when another keeper has the lock) once
/// clients can connect. Returns the process exit code.
pub fn keeper_main(runtime: &str) -> i32 {
    let mut status = StatusChannel::take();
    crate::shim::close_inherited_fds_on_exec();
    let dirs = match crate::paths::dirs() {
        Ok(d) => d,
        Err(e) => return status.fail(e),
    };
    if dirs.runtime != std::path::Path::new(runtime) {
        return status.fail(Error::new(
            ErrorKind::InvalidInput,
            "cannot start the keeper",
            format!(
                "it was asked for {runtime}, but this environment's runtime directory is {}",
                dirs.runtime.display()
            ),
            "start it with the same OPENPOD_RUNTIME_DIR",
        ));
    }
    let (lock, listener, socket_path, socket_inode) = match claim(&dirs) {
        Ok(Some(c)) => c,
        // Another keeper serves this directory.
        Ok(None) => {
            status.report(&StatusMessage::Started { pid: 0 });
            return 0;
        }
        Err(e) => return status.fail(e),
    };
    let linger = crate::config::load(&dirs)
        .map(|c| c.keeper_linger_ms)
        .unwrap_or(10_000);
    if let Err(e) = leave_the_caller() {
        return status.fail(os_error("start", e));
    }
    let (sig_r, sig_w) = match rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    ) {
        Ok(p) => p,
        Err(e) => return status.fail(os_error("start", e.into())),
    };
    let mut keeper = match Keeper::new(listener, socket_path, socket_inode, sig_r, linger) {
        Ok(k) => k,
        Err(e) => return status.fail(os_error("start", e)),
    };
    super::cgroups::settle();
    crate::helper::raise_descriptor_limit();
    crate::signals::ignore(&[libc::SIGHUP, libc::SIGINT, libc::SIGPIPE]);
    crate::signals::notify_pipe(sig_w.as_raw_fd(), &[libc::SIGCHLD, libc::SIGTERM]);
    // The write end lives as long as the process: the handler writes to its number.
    std::mem::forget(sig_w);
    status.report(&StatusMessage::Started {
        pid: std::process::id(),
    });
    keeper.run();
    // Gone from the directory before the lock is: a new keeper binds only with the lock.
    if keeper.owns_socket() {
        let _ = std::fs::remove_file(&keeper.socket_path);
    }
    super::cgroups::leave();
    drop(lock);
    0
}

/// Take the keeper's lock and bind its socket. `None`: another keeper holds the lock.
#[allow(clippy::type_complexity)]
fn claim(dirs: &Dirs) -> crate::Result<Option<(File, UnixListener, PathBuf, u64)>> {
    let fail = |e: std::io::Error| os_error("start", e);
    crate::store::perm::private_dir_builder()
        .recursive(true)
        .create(&dirs.runtime)
        .map_err(fail)?;
    // Clients send their settings and their descriptors to whoever listens here.
    if !super::private_dir(&dirs.runtime) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot start the keeper",
            format!(
                "the runtime directory {} belongs to another user, or others can write to it",
                dirs.runtime.display()
            ),
            "make it private (`chmod 700`), or set OPENPOD_RUNTIME_DIR to a private directory",
        ));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dirs.runtime.join(LOCK))
        .map_err(fail)?;
    // The lock is held by a keeper that serves (its socket answers: leave it to that
    // one), or by one that is still starting or already leaving (wait for its lock).
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                let serves =
                    super::with_socket_path(&dirs.runtime, SOCKET, |p| UnixStream::connect(p))
                        .is_ok();
                if serves || Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(fail(e)),
        }
    }
    let path = dirs.runtime.join(SOCKET);
    // Left by a keeper that was killed: nobody listens there (we hold the lock).
    let _ = std::fs::remove_file(&path);
    // The socket is private from the moment it exists.
    // SAFETY: umask changes only this process's file mode mask; it has one thread.
    let old = unsafe { libc::umask(0o077) };
    let bound = super::with_socket_path(&dirs.runtime, SOCKET, |p| UnixListener::bind(p));
    // SAFETY: as above.
    unsafe { libc::umask(old) };
    let listener = bound.map_err(fail)?;
    crate::store::perm::set_mode(&path, 0o600).map_err(fail)?;
    listener.set_nonblocking(true).map_err(fail)?;
    let inode = std::fs::metadata(&path).map_err(fail)?.ino();
    Ok(Some((lock, listener, path, inode)))
}

/// Fork: the first process ends at once (the caller reaps it), the keeper carries on in a
/// session of its own without a terminal. The lock and the socket go along.
fn leave_the_caller() -> std::io::Result<()> {
    // SAFETY: fork in a single-threaded process.
    match unsafe { libc::fork() } {
        -1 => return Err(std::io::Error::last_os_error()),
        0 => {}
        // SAFETY: the parent leaves without running destructors; the child carries on.
        _ => unsafe { libc::_exit(0) },
    }
    // SAFETY: plain system calls on our own descriptors.
    unsafe {
        libc::setsid();
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
    Ok(())
}

/// The exit code of a waitpid status, or 128 + signal; None for a stop or continue.
fn exit_code(status: i32) -> Option<i32> {
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Some(128 + libc::WTERMSIG(status))
    } else {
        None
    }
}

/// The first free place in `places`, or a new one at its end.
fn free_place<T>(places: &mut Vec<Option<T>>) -> usize {
    match places.iter().position(Option::is_none) {
        Some(free) => free,
        None => {
            places.push(None);
            places.len().saturating_sub(1)
        }
    }
}

impl Keeper {
    fn new(
        listener: UnixListener,
        socket_path: PathBuf,
        socket_inode: u64,
        sig_r: OwnedFd,
        linger_ms: u64,
    ) -> std::io::Result<Keeper> {
        // SAFETY: epoll_create1 has no memory arguments.
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor that nothing else owns.
        let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };
        let keeper = Keeper {
            epoll,
            listener,
            socket_path,
            socket_inode,
            sig_r,
            // Unreadable: no client matches, and every one of them starts its own shim.
            build: super::build_of(std::path::Path::new("/proc/self/exe")).unwrap_or_default(),
            starters: Vec::new(),
            containers: Vec::new(),
            pending: Vec::new(),
            finishers: Vec::new(),
            early: Vec::new(),
            dead_probes: Vec::new(),
            buf: vec![0u8; 64 * 1024],
            kill_at: None,
            linger: Duration::from_millis(linger_ms),
            idle_since: Instant::now(),
            first_by: Some(Instant::now() + FIRST_REQUEST),
            terminating: false,
        };
        keeper.watch(keeper.listener.as_raw_fd(), TOKEN_LISTENER, libc::EPOLLIN)?;
        keeper.watch(keeper.sig_r.as_raw_fd(), TOKEN_SIGNAL, libc::EPOLLIN)?;
        Ok(keeper)
    }

    fn watch(&self, fd: RawFd, token: u64, events: i32) -> std::io::Result<()> {
        let mut ev = libc::epoll_event {
            events: events as u32,
            u64: token,
        };
        // SAFETY: a valid event for a descriptor we own.
        let rc =
            unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut ev) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Stop watching `fd`. Closing a descriptor takes it out of the epoll set only when
    /// no copy of it is left anywhere; a fork (a finisher) holds copies, and their events
    /// would come here under the name of whatever uses the slot next.
    fn unwatch(&self, fd: RawFd) {
        // SAFETY: epoll_ctl with EPOLL_CTL_DEL takes no event.
        unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                fd,
                std::ptr::null_mut(),
            )
        };
    }

    fn busy(&self) -> bool {
        !self.starters.is_empty()
            || !self.finishers.is_empty()
            || self.containers.iter().any(Option::is_some)
            || self.pending.iter().any(Option::is_some)
    }

    fn owns_socket(&self) -> bool {
        std::fs::metadata(&self.socket_path).is_ok_and(|m| m.ino() == self.socket_inode)
    }

    /// Serve until nothing ran for `linger`, the socket was taken away, or a SIGTERM
    /// arrived and everything it watched is gone.
    fn run(&mut self) {
        loop {
            let now = Instant::now();
            // The time left before an idle keeper may go.
            let left = match self.first_by {
                Some(by) => by.saturating_duration_since(now),
                None => self.linger.saturating_sub(self.idle_since.elapsed()),
            };
            if !self.busy() && (self.terminating || !self.owns_socket() || left.is_zero()) {
                return;
            }
            let timeout = if self.busy() {
                // The next deadline: the end of a drain, the kill after a SIGTERM, or a
                // connection that has had its time.
                self.containers
                    .iter()
                    .flatten()
                    .filter_map(|c| c.drain_until)
                    // The next health check, or one that hangs.
                    .chain(
                        self.containers
                            .iter()
                            .flatten()
                            .filter(|c| c.exit.is_none())
                            .filter_map(|c| {
                                c.health.as_ref().map(crate::health::Schedule::next_at)
                            }),
                    )
                    .chain(self.kill_at)
                    .chain(self.pending.iter().flatten().map(|p| p.until))
                    .min()
                    .map_or(-1, |at| {
                        at.saturating_duration_since(now)
                            .as_millis()
                            .clamp(1, 60_000) as i32
                    })
            } else {
                // Wake up at least once a second to see whether the socket is still ours.
                left.as_millis().clamp(1, 1000) as i32
            };
            // SAFETY: an all-zero epoll_event is valid.
            let mut events: [libc::epoll_event; 32] = unsafe { std::mem::zeroed() };
            // SAFETY: the array holds 32 events.
            let n = unsafe {
                libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), 32, timeout)
            };
            for ev in events.iter().take(n.max(0) as usize) {
                let token = ev.u64;
                let name = token & !KIND;
                match token & KIND {
                    0 if token == TOKEN_LISTENER => self.accept(),
                    0 => self.signals(),
                    TOKEN_CLIENT => self.client_left(name as u32),
                    TOKEN_HANDOVER => self.handover(name as u32),
                    TOKEN_CONTROL => self.control(name as usize),
                    TOKEN_OUTPUT => self.output((name >> 8) as usize, (name & 0xff) as usize),
                    TOKEN_PENDING => self.request(name as usize),
                    _ => {}
                }
            }
            self.after_events();
        }
    }

    /// At the end of a round of events: finish the containers that ended and whose output
    /// is read (or whose drain time is over), kill what outlived a SIGTERM's grace, and
    /// drop the connections that did not send their request in time.
    fn after_events(&mut self) {
        let now = Instant::now();
        if self.kill_at.is_some_and(|at| now >= at) {
            self.kill_at = None;
            for c in self
                .containers
                .iter()
                .flatten()
                .filter(|c| c.exit.is_none())
            {
                // SAFETY: a signal to our own child, not reaped yet.
                unsafe { libc::kill(c.pid as libc::pid_t, libc::SIGKILL) };
            }
        }
        for c in self.containers.iter_mut().flatten() {
            if c.exit.is_some() {
                continue;
            }
            let Some(schedule) = c.health.as_mut() else {
                continue;
            };
            let (id, env) = (&c.id, &c.probe_env);
            let hung = schedule.tick(now, |in_start_period| {
                crate::health::spawn_probe(id, in_start_period, crate::health::probe_env(env))
            });
            if let Some(pid) = hung {
                // SAFETY: a signal to our own child, not reaped yet.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
        for slot in 0..self.containers.len() {
            let done = self
                .containers
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|c| {
                    c.exit.is_some()
                        && (c.outputs.iter().all(Option::is_none)
                            || c.drain_until.is_some_and(|at| now >= at))
                });
            if done {
                self.finish(slot);
            }
        }
        for index in 0..self.pending.len() {
            let late = self
                .pending
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|p| now >= p.until);
            if late {
                self.drop_pending(index);
            }
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((conn, _)) => self.pend(conn, Purpose::Start),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    /// Keep a new connection until its request has come. Nothing is read here: the peer
    /// may send it whenever it likes, and the keeper does not wait.
    fn pend(&mut self, conn: UnixStream, purpose: Purpose) {
        // Only this user's processes: the directory's mode alone is no proof.
        if !super::peer_is_us(&conn) || conn.set_nonblocking(true).is_err() {
            return;
        }
        if self.pending.iter().flatten().count() >= MAX_PENDING {
            return;
        }
        let index = free_place(&mut self.pending);
        let token = TOKEN_PENDING | index as u64;
        if self
            .watch(conn.as_raw_fd(), token, libc::EPOLLIN | libc::EPOLLRDHUP)
            .is_err()
        {
            return;
        }
        if let Some(place) = self.pending.get_mut(index) {
            *place = Some(Pending {
                conn,
                purpose,
                incoming: Incoming::default(),
                line: Vec::new(),
                until: Instant::now() + REQUEST_LIMIT,
            });
        }
    }

    fn drop_pending(&mut self, index: usize) {
        if let Some(p) = self.pending.get_mut(index).and_then(Option::take) {
            self.unwatch(p.conn.as_raw_fd());
        }
    }

    /// The connection in place `index` of `pending` has data (or was closed): take what
    /// is there, and act when the request is whole.
    fn request(&mut self, index: usize) {
        let Some(mut p) = self.pending.get_mut(index).and_then(Option::take) else {
            return;
        };
        // None: not whole yet. Some(None): it never will be.
        let whole: Option<Option<(Vec<u8>, Vec<OwnedFd>)>> = match p.purpose {
            Purpose::Start => match p.incoming.read(&p.conn, 4) {
                Ok(Progress::More) => None,
                Ok(Progress::Done(line, fds)) => Some(Some((line, fds))),
                Err(_) => Some(None),
            },
            Purpose::Control { .. } => loop {
                let room = (control::MAX_REQUEST + 1)
                    .saturating_sub(p.line.len())
                    .min(self.buf.len());
                if room == 0 {
                    // A line longer than any request.
                    break Some(None);
                }
                // SAFETY: receiving into a valid buffer, without waiting.
                let got = unsafe {
                    libc::recv(
                        p.conn.as_raw_fd(),
                        self.buf.as_mut_ptr().cast(),
                        room,
                        libc::MSG_DONTWAIT,
                    )
                };
                if got < 0 {
                    match std::io::Error::last_os_error().kind() {
                        std::io::ErrorKind::Interrupted => continue,
                        std::io::ErrorKind::WouldBlock => break None,
                        _ => break Some(None),
                    }
                }
                p.line
                    .extend_from_slice(self.buf.get(..got as usize).unwrap_or_default());
                // The line's end, or the peer's (a request may come without one).
                if got == 0 || p.line.contains(&b'\n') {
                    let line = std::mem::take(&mut p.line);
                    break Some((!line.is_empty()).then_some((line, Vec::new())));
                }
            },
        };
        let Some(whole) = whole else {
            if let Some(place) = self.pending.get_mut(index) {
                *place = Some(p);
            }
            return;
        };
        self.unwatch(p.conn.as_raw_fd());
        let Some((line, fds)) = whole else {
            return;
        };
        // The answer is a short line: it fits in the socket's buffer. The limit is for a
        // peer that has filled it.
        let _ = p.conn.set_nonblocking(false);
        let _ = p.conn.set_write_timeout(Some(Duration::from_secs(1)));
        match p.purpose {
            Purpose::Start => self.serve(p.conn, &line, fds),
            Purpose::Control { slot, pid } => {
                let kept = self
                    .containers
                    .get_mut(slot)
                    .and_then(Option::as_mut)
                    .filter(|c| c.pid == pid);
                if let Some(c) = kept {
                    let master = c
                        .tty
                        .then(|| c.outputs.first().and_then(Option::as_ref))
                        .flatten()
                        .map(|m| m.as_raw_fd());
                    crate::shim::handle_request(
                        p.conn,
                        control::parse_request(&line),
                        &c.id,
                        c.pid,
                        c.exit,
                        master,
                        &mut c.waiters,
                    );
                }
            }
        }
    }

    /// One start request: fork a starter with the client's descriptors.
    fn serve(&mut self, conn: UnixStream, line: &[u8], fds: Vec<OwnedFd>) {
        let refuse = |conn: &UnixStream, why: &str| {
            let reply = Reply::Refused {
                why: why.to_string(),
            };
            if let Ok(line) = serde_json::to_vec(&reply) {
                let _ = proto::send_with_fds(conn, &line, &[]);
            }
        };
        let Ok(Request::Start {
            id,
            mode,
            keep_on_failure,
            env,
            build,
            scope,
        }) = serde_json::from_slice::<Request>(line)
        else {
            return refuse(&conn, "the keeper does not know this request");
        };
        if self.terminating {
            return refuse(&conn, "the keeper is stopping");
        }
        // After an upgrade the old keeper would start new containers with its old code.
        if build != self.build {
            return refuse(&conn, "the keeper runs another build of openpod");
        }
        // The container wants a scope of its own for its limits, and this keeper has no
        // cgroup to make one in: the client's own shim gets a scope.
        if scope && super::cgroups::parent().is_none() {
            return refuse(&conn, "the keeper has no cgroup for containers");
        }
        if fds.len() != 4 || crate::id::check(&id).is_err() {
            return refuse(&conn, "a start needs a container ID and four descriptors");
        }
        let mode = match mode.as_str() {
            "attached" => ShimMode::Attached,
            "detached" => ShimMode::Detached,
            _ => return refuse(&conn, "the mode is attached or detached"),
        };
        let Ok((ours, theirs)) = UnixStream::pair() else {
            return refuse(&conn, "the keeper is out of descriptors");
        };
        // SAFETY: getpid has no preconditions.
        let keeper = unsafe { libc::getpid() };
        // SAFETY: fork in a single-threaded process; the child runs `starter` and exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            starter(keeper, fds, theirs, &id, mode, keep_on_failure, env);
        }
        if pid < 0 {
            return refuse(&conn, "the keeper cannot make another process");
        }
        drop(fds);
        drop(theirs);
        let pid = pid as u32;
        self.idle_since = Instant::now();
        self.first_by = None;
        let handover = self
            .watch(
                ours.as_raw_fd(),
                TOKEN_HANDOVER | u64::from(pid),
                libc::EPOLLIN,
            )
            .ok()
            .map(|()| ours);
        let started = serde_json::to_vec(&Reply::Started { pid })
            .ok()
            .and_then(|line| proto::send_with_fds(&conn, &line, &[]).ok());
        // A client that left already still gets its container; nobody hears the exit.
        let conn = started
            .and_then(|()| {
                self.watch(
                    conn.as_raw_fd(),
                    TOKEN_CLIENT | u64::from(pid),
                    libc::EPOLLRDHUP,
                )
                .ok()
            })
            .map(|()| conn);
        self.starters.push(Starter {
            pid,
            conn,
            handover,
        });
    }

    /// The client of starter `pid` closed its connection: the starter goes on.
    fn client_left(&mut self, pid: u32) {
        let conn = self
            .starters
            .iter_mut()
            .find(|s| s.pid == pid)
            .and_then(|s| s.conn.take());
        if let Some(conn) = conn {
            self.unwatch(conn.as_raw_fd());
        }
    }

    /// Starter `pid` hands its container over (or closed its socket without doing so).
    fn handover(&mut self, pid: u32) {
        let Some(conn) = self
            .starters
            .iter_mut()
            .find(|s| s.pid == pid)
            .and_then(|s| s.handover.take())
        else {
            return;
        };
        self.unwatch(conn.as_raw_fd());
        // The starter is this program's own fork: its message comes in one piece.
        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
        let taken = self.adopt(&conn);
        let _ = proto::send_with_fds(&conn, if taken { b"ok" } else { b"no" }, &[]);
    }

    /// Take a container from its starter. False: the message was not a valid handover.
    fn adopt(&mut self, conn: &UnixStream) -> bool {
        let Ok((line, fds)) = proto::recv_with_fds(conn, proto::MAX_FDS) else {
            return false;
        };
        let Ok(h) = serde_json::from_slice::<Handover>(&line) else {
            return false;
        };
        let mut fds = fds.into_iter();
        let (Some(listener), Some(lock)) = (fds.next(), fds.next()) else {
            return false;
        };
        let (first, second) = (fds.next(), fds.next());
        // A terminal is one descriptor; otherwise stdout and stderr.
        if crate::id::check(&h.id).is_err() || first.is_none() || h.tty != second.is_none() {
            return false;
        }
        let dirs = Dirs {
            data: PathBuf::from(&h.data),
            runtime: PathBuf::from(&h.runtime),
            config: PathBuf::from(&h.config),
        };
        // It may have ended already: its exit was kept for this moment.
        let now = Instant::now();
        self.early
            .retain(|(_, _, at)| now.duration_since(*at) < EARLY_EXIT_KEPT);
        let exit = self
            .early
            .iter()
            .position(|(pid, _, _)| *pid == h.pid)
            .map(|at| self.early.swap_remove(at).1);
        let log = LogWriter::open(&dirs.container_dir(&h.id), LOG_FILE_LIMIT).ok();
        let pid = h.pid;
        let health = crate::state::read_config(&dirs, &h.id)
            .ok()
            .and_then(|spec| spec.health)
            .filter(|_| exit.is_none())
            .map(|config| crate::health::Schedule::new(&config, now));
        let probe_env = if health.is_some() {
            super::handover::saved_env(&dirs, &h.id)
        } else {
            Vec::new()
        };
        let taken = self.take_in(Kept {
            id: h.id,
            pid,
            dirs,
            tty: h.tty,
            outputs: [first, second],
            listener: UnixListener::from(listener),
            lock,
            log,
            waiters: Vec::new(),
            late: Vec::new(),
            exit,
            drain_until: exit.map(|_| now + crate::shim::DRAIN),
            health,
            probe_env,
        });
        if taken {
            self.idle_since = now;
            if self.terminating && exit.is_none() {
                // SAFETY: a signal to our own child, not reaped yet.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            }
        }
        taken
    }

    /// Give `kept` a slot and watch its descriptors. Nothing of it is ever read with a
    /// wait. False: it cannot be watched (no memory for the epoll set).
    fn take_in(&mut self, kept: Kept) -> bool {
        let _ = kept.listener.set_nonblocking(true);
        let slot = free_place(&mut self.containers);
        let mut watched = self
            .watch(
                kept.listener.as_raw_fd(),
                TOKEN_CONTROL | slot as u64,
                libc::EPOLLIN,
            )
            .is_ok();
        for (n, out) in kept.outputs.iter().enumerate() {
            if let Some(fd) = out {
                // SAFETY: fcntl on a descriptor we own; the flags are plain integers.
                unsafe {
                    let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                    libc::fcntl(
                        fd.as_raw_fd(),
                        libc::F_SETFL,
                        flags.max(0) | libc::O_NONBLOCK,
                    );
                }
                let token = TOKEN_OUTPUT | (slot as u64) << 8 | n as u64;
                watched &= self.watch(fd.as_raw_fd(), token, libc::EPOLLIN).is_ok();
            }
        }
        if !watched {
            self.forget(&kept);
            return false;
        }
        if let Some(place) = self.containers.get_mut(slot) {
            *place = Some(kept);
        }
        true
    }

    /// Stop watching everything of `kept`, before its descriptors are closed here.
    fn forget(&self, kept: &Kept) {
        self.unwatch(kept.listener.as_raw_fd());
        for out in kept.outputs.iter().flatten() {
            self.unwatch(out.as_raw_fd());
        }
    }

    /// The control listener of the container in `slot` has connections: keep each until
    /// its request has come.
    fn control(&mut self, slot: usize) {
        let Some(c) = self.containers.get(slot).and_then(Option::as_ref) else {
            return;
        };
        let pid = c.pid;
        let mut new = Vec::new();
        while new.len() < 64 {
            match c.listener.accept() {
                Ok((conn, _)) => new.push(conn),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        for conn in new {
            self.pend(conn, Purpose::Control { slot, pid });
        }
    }

    /// Output `n` of the container in `slot` can be read: into its log.
    fn output(&mut self, slot: usize, n: usize) {
        let Some(c) = self.containers.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        let Some(fd) = c
            .outputs
            .get(n)
            .and_then(Option::as_ref)
            .map(|f| f.as_raw_fd())
        else {
            return;
        };
        // SAFETY: reading into a valid buffer from a descriptor we own.
        let got = unsafe { libc::read(fd, self.buf.as_mut_ptr().cast(), self.buf.len()) };
        if got <= 0 {
            let nothing_yet = got < 0
                && matches!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                );
            if !nothing_yet {
                // The end, or EIO from a terminal whose other side closed. Out of the
                // epoll set before the close (see `unwatch`).
                // SAFETY: epoll_ctl with EPOLL_CTL_DEL takes no event.
                unsafe {
                    libc::epoll_ctl(
                        self.epoll.as_raw_fd(),
                        libc::EPOLL_CTL_DEL,
                        fd,
                        std::ptr::null_mut(),
                    )
                };
                if let Some(out) = c.outputs.get_mut(n) {
                    *out = None;
                }
            }
            return;
        }
        let stream = if n == 0 {
            Stream::Stdout
        } else {
            Stream::Stderr
        };
        if let (Some(log), Some(data)) = (c.log.as_mut(), self.buf.get(..got as usize)) {
            // A full disk loses output, not the container or the keeper.
            let _ = log.write(stream, data);
        }
    }

    /// The container in `slot` ended and its output is read: a fork does the cleanup (the
    /// state, the address, the waiters, the cgroup, `--rm`), so the keeper stays small
    /// and free for the others.
    fn finish(&mut self, slot: usize) {
        let Some(mut kept) = self.containers.get_mut(slot).and_then(Option::take) else {
            return;
        };
        if let Some(log) = kept.log.as_mut() {
            let _ = log.flush_partial();
        }
        // Requests for it that have not come in full go along to the finisher.
        let late = self.late_requests(slot, kept.pid);
        kept.late.extend(late);
        // SAFETY: fork in a single-threaded process; the child runs `finisher` and exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            self.shed();
            // A keeper that is shutting down starts nothing again.
            let relaunch = if self.terminating {
                crate::restart::Relaunch::Never
            } else {
                crate::restart::Relaunch::Direct
            };
            finisher(kept, relaunch);
        }
        if pid < 0 {
            // No process now: try again shortly.
            kept.drain_until = Some(Instant::now() + Duration::from_millis(200));
            if let Some(entry) = self.containers.get_mut(slot) {
                *entry = Some(kept);
            }
            return;
        }
        // The finisher holds copies of the container's descriptors from here on: none of
        // them may stay in the epoll set.
        self.forget(&kept);
        self.finishers.push(pid as u32);
        self.idle_since = Instant::now();
        // The keeper's copies of the container's descriptors close here; the finisher
        // holds the lock until it is done.
        drop(kept);
    }

    /// Take the connections that are in the middle of a request for container `pid` in
    /// `slot`. They stay open: their clients must not see an end before the state says
    /// "exited".
    fn late_requests(&mut self, slot: usize, pid: u32) -> Vec<UnixStream> {
        let mut late = Vec::new();
        for index in 0..self.pending.len() {
            let theirs = self
                .pending
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|p| {
                    matches!(p.purpose, Purpose::Control { slot: s, pid: owner }
                        if s == slot && owner == pid)
                });
            if let Some(p) = theirs
                .then(|| self.pending.get_mut(index).and_then(Option::take))
                .flatten()
            {
                self.unwatch(p.conn.as_raw_fd());
                late.push(p.conn);
            }
        }
        late
    }

    /// In a fork: close what belongs to the keeper and to its other containers.
    fn shed(&mut self) {
        self.containers.clear();
        self.starters.clear();
        self.pending.clear();
        // SAFETY: closing the fork's copies of the keeper's own descriptors; the fork
        // never uses the keeper's fields again.
        unsafe {
            libc::close(self.epoll.as_raw_fd());
            libc::close(self.listener.as_raw_fd());
            libc::close(self.sig_r.as_raw_fd());
        }
    }

    fn signals(&mut self) {
        let mut bytes = [0u8; 64];
        // SAFETY: reading into a valid buffer from our non-blocking pipe.
        let got = unsafe {
            libc::read(
                self.sig_r.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        let term = bytes
            .iter()
            .take(got.max(0) as usize)
            .any(|&b| i32::from(b) == libc::SIGTERM);
        if term && !self.terminating {
            // Stop what runs, the way `stop` does: SIGTERM, and SIGKILL after a grace.
            self.terminating = true;
            self.kill_at = Some(Instant::now() + TERM_GRACE);
            let running = self
                .containers
                .iter()
                .flatten()
                .filter(|c| c.exit.is_none());
            for pid in self
                .starters
                .iter()
                .map(|s| s.pid)
                .chain(running.map(|c| c.pid))
            {
                // SAFETY: a signal to our own child, not reaped yet.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            }
        }
        self.reap();
    }

    /// Collect every child that ended: a starter (tell its client the exit code), a kept
    /// container (start its drain), a finisher, or a container nobody has claimed yet.
    fn reap(&mut self) {
        loop {
            let mut status = 0;
            // SAFETY: status points to a valid int.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                return;
            }
            let Some(code) = exit_code(status) else {
                continue;
            };
            let pid = pid as u32;
            let now = Instant::now();
            self.idle_since = now;
            if let Some(at) = self.starters.iter().position(|s| s.pid == pid) {
                // A handover it sent just before it ended is still in its socket.
                self.handover(pid);
                let starter = self.starters.swap_remove(at);
                if let Some(conn) = starter.conn {
                    self.unwatch(conn.as_raw_fd());
                    if let Ok(line) = serde_json::to_vec(&Reply::Exited { code }) {
                        let _ = proto::send_with_fds(&conn, &line, &[]);
                    }
                }
            } else if let Some(c) = self
                .containers
                .iter_mut()
                .flatten()
                .find(|c| c.pid == pid && c.exit.is_none())
            {
                c.exit = Some(code);
                c.drain_until = Some(now + crate::shim::DRAIN);
                // The container ended: its check goes with it.
                if let Some(probe) = c.health.take().and_then(|h| h.probe()) {
                    // SAFETY: a signal to our own child, not reaped yet.
                    unsafe { libc::kill(probe as libc::pid_t, libc::SIGKILL) };
                    self.dead_probes.push(probe);
                }
            } else if let Some(at) = self.finishers.iter().position(|f| *f == pid) {
                self.finishers.swap_remove(at);
            } else if let Some(schedule) = self
                .containers
                .iter_mut()
                .flatten()
                .filter_map(|c| c.health.as_mut())
                .find(|h| h.probe() == Some(pid))
            {
                schedule.ended(pid, now);
            } else if let Some(at) = self.dead_probes.iter().position(|p| *p == pid) {
                self.dead_probes.swap_remove(at);
            } else {
                // A container whose starter has not handed it over yet (or never will).
                self.early
                    .retain(|(_, _, at)| now.duration_since(*at) < EARLY_EXIT_KEPT);
                if self.early.len() < 1024 {
                    self.early.push((pid, code, now));
                }
            }
        }
    }
}

/// Replace this fork's environment with `env` (a client's settings). Callers are
/// single-threaded forks of the keeper.
fn apply_env(env: Vec<(String, String)>) {
    // SAFETY: no other thread exists in this fork, so nothing reads the environment
    // while it changes.
    unsafe {
        for (key, _) in std::env::vars_os() {
            std::env::remove_var(key);
        }
        for (key, value) in env {
            if !key.is_empty() && !key.contains('=') && !key.contains('\0') && !value.contains('\0')
            {
                std::env::set_var(key, value);
            }
        }
    }
}

/// The starter: a fork of the keeper (process `keeper`) that becomes the shim of one
/// container, with the client's descriptors as 0–3, the socket to the keeper as 4, and
/// the client's settings. Never returns.
fn starter(
    keeper: libc::pid_t,
    fds: Vec<OwnedFd>,
    handover: UnixStream,
    id: &str,
    mode: ShimMode,
    keep_on_failure: bool,
    env: Vec<(String, String)>,
) -> ! {
    // It ends with its keeper, like everything the keeper watches: without the keeper
    // nobody would tell the client how its container ended, or pass its signals on.
    // SAFETY: prctl and getppid have no memory arguments here.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
        // The keeper died before the line above: it never will send the signal.
        if libc::getppid() != keeper {
            libc::_exit(125);
        }
    }
    // The keeper's handlers write to its pipe, which this process is about to close.
    crate::signals::restore_default(&[libc::SIGCHLD, libc::SIGTERM, libc::SIGHUP, libc::SIGINT]);
    // SAFETY: plain system calls. The descriptors move above the range they are bound for
    // first, so that none is overwritten before it is copied; then everything that is
    // the keeper's (its socket, other containers' descriptors) is closed.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"openpod-shim".as_ptr());
        let mut high = [-1; 5];
        let all = fds
            .into_iter()
            .map(IntoRawFd::into_raw_fd)
            .chain([handover.into_raw_fd()]);
        for (slot, fd) in high.iter_mut().zip(all) {
            *slot = libc::fcntl(fd, libc::F_DUPFD, 100);
        }
        // 0–2: the streams, 3: the status pipe, 4: the socket to the keeper.
        debug_assert_eq!(KEEPER_FD, 4);
        for (target, fd) in high.iter().enumerate() {
            if *fd < 0 || libc::dup2(*fd, target as i32) < 0 {
                libc::_exit(125);
            }
        }
        libc::syscall(
            libc::SYS_close_range,
            5 as libc::c_uint,
            libc::c_uint::MAX,
            0,
        );
    }
    // The shim runs as the client would have started it: with the client's settings.
    apply_env(env);
    UNDER_KEEPER.store(true, Ordering::Relaxed);
    crate::helper::restore_descriptor_limit();
    let code = crate::shim::shim_main(id, Some(mode), keep_on_failure);
    std::process::exit(code)
}

/// The finisher: a fork of the keeper that does what follows a kept container's end, with
/// the settings of the client that started the container. Never returns.
fn finisher(kept: Kept, relaunch: crate::restart::Relaunch) -> ! {
    crate::signals::restore_default(&[libc::SIGCHLD, libc::SIGTERM, libc::SIGHUP, libc::SIGINT]);
    // SAFETY: prctl with a valid name.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"openpod-finish".as_ptr()) };
    let env = super::handover::saved_env(&kept.dirs, &kept.id);
    if !env.is_empty() {
        apply_env(env);
    }
    crate::helper::restore_descriptor_limit();
    let Kept {
        id,
        pid,
        dirs,
        outputs,
        listener,
        lock,
        mut waiters,
        late,
        exit,
        ..
    } = kept;
    // Read to their end (or given up) by the keeper: nothing here needs them.
    drop(outputs);
    let code = exit.unwrap_or(-1);
    // Requests that were on their way when the container ended: a `wait` joins the
    // waiters, anything else hears that the container does not run. Connections that
    // sent nothing stay open until the state says "exited", like the listener.
    let mut silent = Vec::new();
    for stream in late {
        let _ = stream.set_nonblocking(false);
        match control::read_request_within(&stream, Duration::from_millis(250)) {
            Some(request) => crate::shim::handle_request(
                stream,
                Some(request),
                &id,
                pid,
                Some(code),
                None,
                &mut waiters,
            ),
            None => silent.push(stream),
        }
    }
    match (
        crate::state::read_config(&dirs, &id),
        crate::state::read_state(&dirs, &id),
    ) {
        (Ok(spec), Ok(st)) => {
            let group = st.cgroup.clone().map(PathBuf::from);
            let oom = group.as_ref().and_then(|g| {
                let events = std::fs::read_to_string(g.join("memory.events")).ok()?;
                crate::cgroup::oom_reason(&events)
            });
            // SAFETY: geteuid has no preconditions.
            let rootless = unsafe { libc::geteuid() } != 0;
            let session =
                rootless && matches!(spec.network, crate::network::NetworkMode::Bridge { .. });
            crate::shim::finish(&dirs, &spec, st, code, oom, session, listener, &waiters);
            if let Some(g) = group.filter(|g| g.starts_with(crate::cgroup::ROOT)) {
                if crate::cgroup::remove_dir(&g) {
                    crate::state::forget_cgroup(&dirs, &id, &g.to_string_lossy());
                }
            }
            if spec.auto_remove {
                crate::shim::remove_container(&dirs, &spec);
            }
            crate::restart::after_exit(&dirs, &spec, code, relaunch);
        }
        // The record is gone or unreadable: at least tell those who wait.
        _ => {
            for w in &waiters {
                control::reply(w, &control::Response::Exited { exit_code: code });
            }
            drop(listener);
            control::remove(&dirs, &id);
        }
    }
    drop(silent);
    drop(lock);
    // SAFETY: leave the fork without running the keeper's destructors.
    unsafe { libc::_exit(0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A keeper over `dir` that serves nothing yet.
    fn keeper(dir: &std::path::Path) -> Keeper {
        let path = dir.join("keeper.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (sig_r, sig_w) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .unwrap();
        // As in the keeper: the writing end stays open for the life of the process.
        std::mem::forget(sig_w);
        Keeper::new(listener, path, 0, sig_r, 1000).unwrap()
    }

    /// A container as a starter hands it over, and the writing ends of its outputs.
    fn container(dir: &std::path::Path, name: &str) -> (Kept, [std::io::PipeWriter; 2]) {
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (err_r, err_w) = std::io::pipe().unwrap();
        let kept = Kept {
            id: "c".repeat(64),
            pid: 1,
            dirs: Dirs {
                data: dir.join("data"),
                runtime: dir.join("run"),
                config: dir.join("config"),
            },
            tty: false,
            outputs: [Some(out_r.into()), Some(err_r.into())],
            listener: UnixListener::bind(dir.join(name)).unwrap(),
            lock: File::create(dir.join(format!("{name}.lock")))
                .unwrap()
                .into(),
            log: None,
            waiters: Vec::new(),
            late: Vec::new(),
            exit: None,
            drain_until: None,
            health: None,
            probe_env: Vec::new(),
        };
        (kept, [out_w, err_w])
    }

    /// How many events wait in the keeper's epoll set.
    fn events(keeper: &Keeper) -> i32 {
        // SAFETY: an all-zero epoll_event is valid; the array holds 8 events.
        unsafe {
            let mut events: [libc::epoll_event; 8] = std::mem::zeroed();
            libc::epoll_wait(keeper.epoll.as_raw_fd(), events.as_mut_ptr(), 8, 100)
        }
    }

    #[test]
    fn a_container_that_left_wakes_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let mut k = keeper(dir.path());
        let (kept, _writers) = container(dir.path(), "c.sock");
        assert!(k.take_in(kept));
        assert_eq!(events(&k), 0);
        // A finisher holds copies of the container's descriptors (here: a clone) ...
        let kept = k.containers.first_mut().and_then(Option::take).unwrap();
        let finishers_copy = kept.listener.try_clone().unwrap();
        k.forget(&kept);
        drop(kept);
        // ... and a late `wait` reaches the copy: no business of the keeper's any more.
        let _late = UnixStream::connect(dir.path().join("c.sock")).unwrap();
        assert_eq!(
            events(&k),
            0,
            "the keeper still watches a container it gave away"
        );
        drop(finishers_copy);
    }

    #[test]
    fn a_request_on_its_way_at_the_end_is_kept_for_the_finisher() {
        let dir = tempfile::tempdir().unwrap();
        let mut k = keeper(dir.path());
        let (kept, _writers) = container(dir.path(), "c.sock");
        assert!(k.take_in(kept));
        // A `wait` arrives; the container ends before the keeper has read it.
        let client = UnixStream::connect(dir.path().join("c.sock")).unwrap();
        (&client).write_all(b"{\"op\":\"wait\"}\n").unwrap();
        k.control(0);
        assert_eq!(k.pending.iter().flatten().count(), 1);
        let late = k.late_requests(0, 1);
        assert_eq!((late.len(), k.pending.iter().flatten().count()), (1, 0));
        // Its client hears nothing yet: an end now would mean "the container vanished".
        client.set_nonblocking(true).unwrap();
        let mut byte = [0u8; 1];
        let heard = std::io::Read::read(&mut &client, &mut byte);
        assert!(
            heard.is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock),
            "the connection was closed"
        );
        // The finisher reads the request from the connection it was given.
        let stream = late.into_iter().next().unwrap();
        stream.set_nonblocking(false).unwrap();
        assert_eq!(
            control::read_request_within(&stream, Duration::from_millis(250)),
            Some(control::Request::Wait)
        );
        // Requests for another container (or a later one in the slot) stay.
        let other = UnixStream::connect(dir.path().join("c.sock")).unwrap();
        k.control(0);
        assert!(k.late_requests(0, 2).is_empty());
        assert!(k.late_requests(1, 1).is_empty());
        assert_eq!(k.pending.iter().flatten().count(), 1);
        drop(other);
    }

    #[test]
    fn output_is_read_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let mut k = keeper(dir.path());
        let (kept, [mut out_w, _err_w]) = container(dir.path(), "c.sock");
        assert!(k.take_in(kept));
        // An event for an output that has nothing (it can come for a slot's earlier
        // user): the read must come back, not wait for this container to write.
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            out_w.write_all(b"late").unwrap();
            out_w
        });
        let started = Instant::now();
        k.output(0, 0);
        let took = started.elapsed();
        let _out_w = late.join().unwrap();
        assert!(
            took < Duration::from_millis(500),
            "the read waited {took:?}"
        );
    }

    #[test]
    fn a_connection_gets_its_time_and_no_more() {
        let dir = tempfile::tempdir().unwrap();
        let mut k = keeper(dir.path());
        let silent = UnixStream::connect(dir.path().join("keeper.sock")).unwrap();
        k.accept();
        assert_eq!(k.pending.iter().flatten().count(), 1);
        // Half a request: kept, not waited for.
        (&silent).write_all(&[0, 0, 1]).unwrap();
        let started = Instant::now();
        k.request(0);
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(k.pending.iter().flatten().count(), 1);
        // Its time is over: dropped, and the peer sees the end.
        if let Some(p) = k.pending.first_mut().and_then(Option::as_mut) {
            p.until = Instant::now();
        }
        k.after_events();
        assert_eq!(k.pending.iter().flatten().count(), 0);
        let mut end = [0u8; 1];
        assert_eq!(std::io::Read::read(&mut &silent, &mut end).unwrap(), 0);
        // No more than MAX_PENDING at a time.
        let many: Vec<_> = (0..MAX_PENDING + 8)
            .map(|_| UnixStream::connect(dir.path().join("keeper.sock")).unwrap())
            .collect();
        k.accept();
        assert_eq!(k.pending.iter().flatten().count(), MAX_PENDING);
        drop(many);
    }
}
