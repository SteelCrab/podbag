//! `openpod __vm-agent`: the macOS VM's side of the Mac's commands. On vsock port
//! [`CLI_PORT`] it runs `openpod <argv…>` for the Mac's CLI (see [`super::proto`]); on
//! [`API_PORT`] it serves the Docker Engine API, the same handler as
//! `openpod system service`. Only the Mac (vsock CID 2) may connect: a container that
//! reaches vsock gets nothing.

use super::proto::{API_PORT, CLI_PORT, CliRequest, FORWARD_PORT, Frame};
use crate::{Error, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Where PID 1 writes why the VM cannot work (a mount that failed). The agent answers
/// every command with it.
pub const BROKEN: &str = "/run/openpod-vm-error";

pub fn vm_agent_main() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("openpod: {e}");
            1
        }
    }
}

fn run() -> Result<()> {
    // PID 1 blocks the signals it waits for, and a child inherits that mask: the commands
    // run here must get theirs as a local command does (Ctrl+C ends `logs -f`), and the
    // agent itself must end on SIGTERM when the VM powers off.
    crate::signals::unblock_all();
    let api = listen(API_PORT)?;
    let cli = listen(CLI_PORT)?;
    let forwarder = listen(FORWARD_PORT)?;
    let dirs = crate::paths::dirs()?;
    let mut ctx = crate::api::router::Ctx::new(dirs);
    // The Mac's paths: the VM sees only its /Users, at the same place.
    ctx.binds_under = Some("/Users".into());
    let ctx = Arc::new(ctx);
    crate::helper::helpers_in_own_group();
    crate::helper::raise_descriptor_limit();
    std::thread::spawn(keep_lease);
    eprintln!("openpod: VM agent listens");
    std::thread::spawn(move || {
        accept_each(&api, |stream| {
            let ctx = ctx.clone();
            std::thread::spawn(move || crate::api::handle_connection(stream, &ctx));
        })
    });
    std::thread::spawn(move || {
        accept_each(&forwarder, |stream| {
            std::thread::spawn(move || forward(stream));
        })
    });
    accept_each(&cli, |stream| {
        std::thread::spawn(move || {
            if let Err(e) = serve_cli(stream) {
                eprintln!("openpod: a command from the Mac failed: {e}");
            }
        });
    });
    Ok(())
}

/// A listening vsock socket on `port`.
fn listen(port: u32) -> Result<OwnedFd> {
    let fail = |e: std::io::Error| {
        Error::io(
            format!("cannot listen on vsock port {port}"),
            &e,
            "check that the kernel has virtio vsock",
        )
    };
    // SAFETY: plain socket calls on a new descriptor.
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(fail(std::io::Error::last_os_error()));
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        if libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        ) != 0
            || libc::listen(fd.as_raw_fd(), 128) != 0
        {
            return Err(fail(std::io::Error::last_os_error()));
        }
        Ok(fd)
    }
}

/// Accept connections from the Mac forever. A vsock connection works as a `UnixStream`:
/// reads, writes, shutdown and timeouts are the same socket calls (std's
/// `UnixListener::accept` would refuse its address).
fn accept_each(listener: &OwnedFd, mut each: impl FnMut(UnixStream)) {
    loop {
        // SAFETY: an all-zero sockaddr_vm is a valid buffer.
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        // SAFETY: accept4 on our listening socket, with an address buffer of its size.
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                (&mut addr as *mut libc::sockaddr_vm).cast(),
                &mut len,
                libc::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                std::thread::sleep(Duration::from_millis(100));
            }
            continue;
        }
        // SAFETY: a new connected socket that nothing else owns.
        let conn = unsafe { OwnedFd::from_raw_fd(fd) };
        // The Mac is CID 2. Anything else is this VM itself (a container that reached
        // vsock), which must not run commands as root here.
        if addr.svm_cid != libc::VMADDR_CID_HOST {
            eprintln!(
                "openpod: refused a vsock connection from CID {}",
                addr.svm_cid
            );
            continue;
        }
        each(UnixStream::from(conn));
    }
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

/// Why the VM cannot work, when PID 1 found a reason.
fn broken() -> Option<String> {
    std::fs::read_to_string(BROKEN)
        .ok()
        .map(|s| s.trim_end().to_string())
}

/// Run one command for the Mac and stream it.
fn serve_cli(stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let req: CliRequest = serde_json::from_str(line.trim_end())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let out = Out(Arc::new(Mutex::new(stream.try_clone()?)));
    sync_clock(req.time);
    let first = req.argv.first().map(String::as_str);
    match first {
        Some("__vm-poweroff") => {
            out.send(Frame::Exit(0))?;
            // SAFETY: a signal to PID 1, which powers off.
            unsafe { libc::kill(1, libc::SIGTERM) };
            return Ok(());
        }
        Some("__vm-agent-exit") => {
            // For tests and debugging: the agent ends as if it had crashed.
            out.send(Frame::Exit(0))?;
            std::process::exit(1);
        }
        _ => {}
    }
    let code = if let Some(why) = broken() {
        out.send(Frame::Stderr(format!("openpod: {why}\n").into_bytes()))?;
        125
    } else {
        match first {
            Some("__vm-check") => 0,
            Some("__vm-renew") => match super::dhcp::renew() {
                Ok(secs) => {
                    out.send(Frame::Stdout(format!("{secs}\n").into_bytes()))?;
                    0
                }
                Err(e) => {
                    out.send(Frame::Stderr(format!("openpod: {e}\n").into_bytes()))?;
                    1
                }
            },
            Some("__vm-busy") => i32::from(!busy()),
            _ => run_command(&req, reader, &out)?,
        }
    };
    out.send(Frame::Exit(code))?;
    // The thread that reads the Mac's frames learns from this that the command is over.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// A connection the Mac forwards from one of its ports: `TCP <port>` names the port
/// published in this VM, where the shim's proxy listens on every address.
fn forward(mut stream: UnixStream) {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 32 {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => line.push(byte[0]),
            _ => return,
        }
    }
    let port: Option<u16> = String::from_utf8_lossy(&line)
        .strip_prefix("TCP ")
        .and_then(|p| p.trim().parse().ok());
    let target = port.map(|p| {
        std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], p)),
            Duration::from_secs(5),
        )
    });
    match target {
        Some(Ok(target)) => {
            if stream.write_all(b"OK\n").is_ok() {
                carry(stream, target);
            }
        }
        Some(Err(e)) => {
            let _ = stream.write_all(format!("ERR {e}\n").as_bytes());
        }
        None => {
            let _ = stream.write_all(b"ERR bad request\n");
        }
    }
}

/// Bytes both ways between the Mac and a TCP connection, until both directions end.
fn carry(mac: UnixStream, target: std::net::TcpStream) {
    let (Ok(mut mac_r), Ok(mut target_w)) = (mac.try_clone(), target.try_clone()) else {
        return;
    };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut mac_r, &mut target_w);
        let _ = target_w.shutdown(std::net::Shutdown::Write);
    });
    let (mut target_r, mut mac_w) = (target, mac);
    let _ = std::io::copy(&mut target_r, &mut mac_w);
    let _ = mac_w.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

/// The VM's DHCP lease, which the kernel took at boot and never renews: renewed at half
/// its time (20 minutes until the first answer tells it), a minute after a failure.
fn keep_lease() {
    let mut wait = Duration::from_secs(20 * 60);
    loop {
        std::thread::sleep(wait);
        wait = match super::dhcp::renew() {
            Ok(secs) => Duration::from_secs(u64::from(secs.max(120)) / 2),
            Err(e) => {
                eprintln!("openpod: cannot renew the VM's address lease: {e}");
                Duration::from_secs(60)
            }
        };
    }
}

/// The Mac's clock wins when they drift apart (the VM may have slept). A request without
/// a time leaves the clock.
fn sync_clock(time: (u64, u32)) {
    if time.0 == 0 {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    if now.as_secs().abs_diff(time.0) > 1 {
        super::guest::set_clock(time);
    }
}

/// Whether the VM has work: a running container, or one waiting to be started again by
/// its restart policy. When it cannot tell, it has.
fn busy() -> bool {
    let running = crate::containers::ContainerHandle::list(false).map_or(true, |l| !l.is_empty());
    running || restart_pending()
}

/// A restarter (`openpod __restart`) lives while a container waits to start again.
fn restart_pending() -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    procs.flatten().any(|p| {
        p.file_name()
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            && std::fs::read(p.path().join("cmdline"))
                .is_ok_and(|c| c.split(|&b| b == 0).nth(1) == Some(b"__restart".as_slice()))
    })
}

fn command(req: &CliRequest) -> std::process::Command {
    let mut cmd = std::process::Command::new("/init");
    cmd.args(&req.argv)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("HOME", "/root")
        .envs(req.env.iter().map(|(k, v)| (k, v)))
        // Last: openpod is /init here, whatever the Mac sent.
        .env("OPENPOD_BIN", "/init");
    let cwd = std::path::Path::new(&req.cwd);
    cmd.current_dir(if cwd.is_dir() {
        cwd
    } else {
        std::path::Path::new("/")
    });
    cmd
}

/// The command's process. Signals reach it only until it is reaped, so never a process
/// that took over its pid.
struct Proc {
    pid: libc::pid_t,
    /// A session leader with a terminal: a hangup goes to its process group.
    leader: bool,
    ended: Mutex<bool>,
}

impl Proc {
    fn new(pid: u32, leader: bool) -> Proc {
        Proc {
            pid: pid as libc::pid_t,
            leader,
            ended: Mutex::new(false),
        }
    }

    fn ended(&self) -> bool {
        *self.ended.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn signal(&self, sig: i32) {
        let ended = self.ended.lock().unwrap_or_else(|p| p.into_inner());
        if !*ended {
            // SAFETY: not reaped yet (`ended` is held), so the pid is still this process's.
            unsafe { libc::kill(self.pid, sig) };
        }
    }

    /// What a lost Mac is to the command: a hangup, as when its terminal closes. False
    /// when it had ended already.
    fn hang_up(&self) -> bool {
        let ended = self.ended.lock().unwrap_or_else(|p| p.into_inner());
        if *ended {
            return false;
        }
        // SAFETY: as in `signal`; a leader's process group has its pid as its id.
        unsafe {
            if self.leader {
                libc::kill(-self.pid, libc::SIGHUP);
                libc::kill(-self.pid, libc::SIGCONT);
            } else {
                libc::kill(self.pid, libc::SIGHUP);
            }
        }
        true
    }

    /// Wait for the end, mark it, and only then reap.
    fn wait(&self) -> std::io::Result<i32> {
        loop {
            // SAFETY: waitid fills a siginfo on the stack; WNOWAIT leaves it unreaped.
            let r = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if r == 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        *self.ended.lock().unwrap_or_else(|p| p.into_inner()) = true;
        let mut status = 0;
        // SAFETY: reaps our own child, which has ended.
        unsafe { libc::waitpid(self.pid, &mut status, 0) };
        Ok(if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            1
        })
    }
}

/// A started command and its descriptors.
struct Session {
    proc: Proc,
    /// Where the Mac's input goes.
    input: std::fs::File,
    /// What goes back to the Mac, and as which frame.
    outputs: Vec<(OwnedFd, fn(Vec<u8>) -> Frame)>,
    /// The terminal (its master side), for its size.
    terminal: Option<OwnedFd>,
}

/// Without a terminal: stdin, stdout and stderr through pipes.
fn with_pipes(req: &CliRequest) -> std::io::Result<Session> {
    use std::process::Stdio;
    let mut child = command(req)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let taken = "piped above";
    Ok(Session {
        proc: Proc::new(child.id(), false),
        input: std::fs::File::from(OwnedFd::from(child.stdin.take().expect(taken))),
        outputs: vec![
            (
                OwnedFd::from(child.stdout.take().expect(taken)),
                Frame::Stdout,
            ),
            (
                OwnedFd::from(child.stderr.take().expect(taken)),
                Frame::Stderr,
            ),
        ],
        terminal: None,
    })
}

/// With a terminal: the command gets a new pty in a session of its own; its output comes
/// back as stdout.
fn with_terminal(req: &CliRequest) -> std::io::Result<Session> {
    use std::os::unix::process::CommandExt;
    let (master, slave) =
        crate::linux::pty::open(None).map_err(|e| std::io::Error::other(e.to_string()))?;
    crate::linux::pty::set_size(master.as_raw_fd(), req.rows, req.cols);
    let slave_fd = slave.as_raw_fd();
    let mut cmd = command(req);
    cmd.stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave.try_clone()?);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(slave);
    Ok(Session {
        proc: Proc::new(child.id(), true),
        input: std::fs::File::from(master.try_clone()?),
        outputs: vec![(master.try_clone()?, Frame::Stdout)],
        terminal: Some(master),
    })
}

fn run_command(req: &CliRequest, reader: BufReader<UnixStream>, out: &Out) -> std::io::Result<i32> {
    let session = if req.tty {
        with_terminal(req)?
    } else {
        with_pipes(req)?
    };
    let proc = Arc::new(session.proc);
    let done = Arc::new(AtomicBool::new(false));
    let pumps: Vec<_> = session
        .outputs
        .into_iter()
        .map(|(fd, wrap)| pump(fd, wrap, out.clone(), done.clone()))
        .collect();
    let feed = feeder(session.input, req.tty, out.clone());
    {
        let proc = proc.clone();
        let terminal = session.terminal;
        std::thread::spawn(move || listen_to_mac(reader, &proc, feed, terminal.as_ref()));
    }
    let code = proc.wait()?;
    done.store(true, Ordering::SeqCst);
    for p in pumps {
        let _ = p.join();
    }
    Ok(code)
}

/// The Mac's frames until it goes. Then the command gets a hangup, as when a terminal
/// closes, and a kill if it outlives that by ten seconds.
fn listen_to_mac(
    mut reader: BufReader<UnixStream>,
    proc: &Proc,
    feed: mpsc::Sender<Option<Vec<u8>>>,
    terminal: Option<&OwnedFd>,
) {
    loop {
        match Frame::read_from(&mut reader) {
            Ok(Some(Frame::Stdin(data))) => {
                let _ = feed.send(Some(data));
            }
            Ok(Some(Frame::StdinEnd)) => {
                let _ = feed.send(None);
            }
            Ok(Some(Frame::Resize { rows, cols })) => {
                if let Some(t) = terminal {
                    crate::linux::pty::set_size(t.as_raw_fd(), rows, cols);
                }
            }
            Ok(Some(Frame::Signal(sig))) => proc.signal(sig),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    // No more input: a pipe's reader sees its end.
    drop(feed);
    if proc.hang_up() {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !proc.ended() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        proc.signal(libc::SIGKILL);
    }
}

/// Writes the Mac's input to the command in a thread of its own, so a command that does
/// not read never holds up the Mac's other frames. Each chunk is acknowledged: the Mac
/// sends at most a window ahead.
fn feeder(input: std::fs::File, tty: bool, out: Out) -> mpsc::Sender<Option<Vec<u8>>> {
    let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
    std::thread::spawn(move || {
        let mut input = Some(input);
        for item in rx {
            match item {
                Some(data) => {
                    if input.as_mut().is_some_and(|w| w.write_all(&data).is_err()) {
                        input = None;
                    }
                    if out.send(Frame::StdinTaken(data.len() as u32)).is_err() {
                        break;
                    }
                }
                // The end of input; on a terminal, an end-of-file character.
                None if tty => {
                    if let Some(w) = input.as_mut() {
                        let _ = w.write_all(&[4]);
                    }
                }
                None => input = None,
            }
        }
    });
    tx
}

/// Copies `fd` to the Mac until it ends. Once the command ended (`done`), only until it
/// is empty: a process the command left in the background may keep it open.
fn pump(
    fd: OwnedFd,
    wrap: fn(Vec<u8>) -> Frame,
    out: Out,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut p = libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd on the stack.
            let ready = unsafe { libc::poll(&mut p, 1, 100) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if ready == 0 {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            match file.read(&mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // The end; on a terminal, EIO once its last process left.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.send(wrap(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    })
}
