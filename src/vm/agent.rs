//! `podbag __vm-agent`: the macOS VM's side of the Mac's commands. On vsock port
//! [`CLI_PORT`] it runs `podbag <argv…>` for the Mac's CLI (see [`super::proto`]); on
//! [`API_PORT`] it serves the Docker Engine API, the same handler as
//! `podbag system service`. Only the Mac (vsock CID 2) may connect: a container that
//! reaches vsock gets nothing.

use super::proto::{API_PORT, CLI_PORT, CliRequest, FORWARD_PORT, Frame, KIND_API_PORT};
use crate::{Error, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Where PID 1 writes why the VM cannot work (a mount that failed). The agent answers
/// every command with it.
pub const BROKEN: &str = "/run/podbag-vm-error";

pub fn vm_agent_main() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("podbag: {e}");
            1
        }
    }
}

fn run() -> Result<()> {
    // PID 1 blocks the signals it waits for, and a child inherits that mask: the commands
    // run here must get theirs as a local command does (Ctrl+C ends `logs -f`), and the
    // agent itself must end on SIGTERM when the VM powers off.
    crate::signals::unblock_all();
    // An OOM kill never takes the agent: every session and the Docker API go through it.
    // Containers set their own score back to 0 (`linux::security`), and the commands run
    // here get 0 too (`command`).
    if let Err(e) = std::fs::write("/proc/self/oom_score_adj", "-1000") {
        eprintln!("podbag: cannot protect the VM agent from OOM kills: {e}");
    }
    let api = listen(API_PORT)?;
    let kind_api = listen(KIND_API_PORT)?;
    let cli = listen(CLI_PORT)?;
    let forwarder = listen(FORWARD_PORT)?;
    let dirs = crate::paths::dirs()?;
    let sync_dirs = dirs.clone();
    std::thread::spawn(move || super::sync::watch(sync_dirs));
    let mut ctx = crate::api::router::Ctx::new(dirs.clone());
    // Use only folders that the init process mounted successfully.
    ctx.binds_under = Some(super::shares::active().unwrap_or_default());
    let shares = ctx.binds_under.clone().unwrap_or_default();
    let ctx = Arc::new(ctx);
    let mut kind_ctx = crate::api::router::Ctx::new(dirs);
    kind_ctx.binds_under = Some(shares.clone());
    kind_ctx.kind_node_binds = true;
    let kind_ctx = Arc::new(kind_ctx);
    crate::helper::helpers_in_own_group();
    crate::helper::raise_descriptor_limit();
    std::thread::spawn(keep_lease);
    std::thread::spawn(guard_memory);
    eprintln!("podbag: VM agent listens");
    std::thread::spawn(move || {
        accept_each(&api, |stream| {
            let ctx = ctx.clone();
            std::thread::spawn(move || crate::api::handle_connection(stream, &ctx));
        })
    });
    std::thread::spawn(move || {
        accept_each(&kind_api, |stream| {
            let ctx = kind_ctx.clone();
            std::thread::spawn(move || crate::api::handle_connection(stream, &ctx));
        })
    });
    std::thread::spawn(move || {
        accept_each(&forwarder, |stream| {
            std::thread::spawn(move || forward(stream));
        })
    });
    let shares = Arc::new(shares);
    accept_each(&cli, |stream| {
        let shares = shares.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve_cli(stream, &shares) {
                eprintln!("podbag: a command from the Mac failed: {e}");
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
                "podbag: refused a vsock connection from CID {}",
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
fn serve_cli(stream: UnixStream, shares: &[std::path::PathBuf]) -> std::io::Result<()> {
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
        out.send(Frame::Stderr(format!("podbag: {why}\n").into_bytes()))?;
        125
    } else if let Some(path) = req
        .paths
        .iter()
        .find(|path| !super::shares::contains(shares, &Path::new(&req.cwd).join(path)))
    {
        out.send(Frame::Stderr(
            format!("podbag: cannot use {path}: the path is outside the Mac folders shared with the VM\n").into_bytes(),
        ))?;
        125
    } else {
        match first {
            Some("__vm-sync") => match super::sync::command(&req.argv[1..]) {
                Ok(data) => {
                    for part in data.as_bytes().chunks(64 * 1024) {
                        out.send(Frame::Stdout(part.to_vec()))?;
                    }
                    out.send(Frame::Stdout(b"\n".to_vec()))?;
                    0
                }
                Err(error) => {
                    out.send(Frame::Stderr(format!("podbag: {error}\n").into_bytes()))?;
                    1
                }
            },
            Some("__vm-check") => 0,
            Some("__vm-renew") => match super::dhcp::renew() {
                Ok(secs) => {
                    out.send(Frame::Stdout(format!("{secs}\n").into_bytes()))?;
                    0
                }
                Err(e) => {
                    out.send(Frame::Stderr(format!("podbag: {e}\n").into_bytes()))?;
                    1
                }
            },
            Some("__vm-busy") => i32::from(!busy()),
            Some("__vm-stats") => {
                out.send(Frame::Stdout(format!("{}\n", stats()).into_bytes()))?;
                0
            }
            _ => run_command(&req, reader, &out)?,
        }
    };
    out.send(Frame::Exit(code))?;
    // The thread that reads the Mac's frames learns from this that the command is over.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// The VM's CPU time and memory, for `podbag vm status` on the Mac: `/proc/stat`'s
/// first line (jiffies: all, idle) and `/proc/meminfo` (KiB: total, available), as JSON.
fn stats() -> String {
    let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    stats_of(&stat, &meminfo).to_string()
}

fn stats_of(stat: &str, meminfo: &str) -> serde_json::Value {
    let cpu: Vec<u64> = stat
        .lines()
        .find(|l| l.starts_with("cpu "))
        .map(|l| {
            l.split_whitespace()
                .skip(1)
                .filter_map(|n| n.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    // user nice system idle iowait irq softirq steal: idle and iowait are idle time.
    let total: u64 = cpu.iter().take(8).sum();
    let idle = cpu.get(3).copied().unwrap_or(0) + cpu.get(4).copied().unwrap_or(0);
    let cpus = stat
        .lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .count();
    let kib = |name: &str| -> u64 {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    };
    serde_json::json!({
        "cpuTotal": total,
        "cpuIdle": idle,
        "cpus": cpus,
        "memTotalKiB": kib("MemTotal"),
        "memAvailableKiB": kib("MemAvailable"),
    })
}

/// A connection the Mac forwards from one of its ports: `TCP <port>` or `UDP <port>`
/// names the port published in this VM, where the shim's proxy listens on every address.
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
    let line = String::from_utf8_lossy(&line);
    let port = |p: &str| p.trim().parse::<u16>().ok();
    let local = |p: u16| std::net::SocketAddr::from(([127, 0, 0, 1], p));
    if let Some(p) = line.strip_prefix("UDP ").and_then(port) {
        let target = std::net::UdpSocket::bind(("0.0.0.0", 0)).and_then(|s| {
            s.connect(local(p))?;
            Ok(s)
        });
        match target {
            Ok(target) => {
                if stream.write_all(b"OK\n").is_ok() {
                    carry_datagrams(stream, target);
                }
            }
            Err(e) => {
                let _ = stream.write_all(format!("ERR {e}\n").as_bytes());
            }
        }
        return;
    }
    let target = line
        .strip_prefix("TCP ")
        .and_then(port)
        .map(|p| std::net::TcpStream::connect_timeout(&local(p), Duration::from_secs(5)));
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

/// One UDP flow of the Mac: its datagrams to the published port, and the replies back,
/// until the Mac ends the flow.
fn carry_datagrams(mac: UnixStream, target: std::net::UdpSocket) {
    use super::udp::{MAX_DATAGRAM, read_datagram, write_datagram};
    let (mac, target) = (Arc::new(mac), Arc::new(target));
    let done = Arc::new(AtomicBool::new(false));
    // A timeout lets the reply loop see that the flow ended.
    let _ = target.set_read_timeout(Some(Duration::from_secs(1)));
    let up = {
        let (mac, target, done) = (mac.clone(), target.clone(), done.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            while let Ok(Some(n)) = read_datagram(&mut &*mac, &mut buf) {
                // Nothing listens yet: the datagram is lost, as UDP allows.
                let _ = target.send(&buf[..n]);
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    let mut buf = vec![0u8; MAX_DATAGRAM];
    while !done.load(Ordering::SeqCst) {
        match target.recv(&mut buf) {
            Ok(n) => {
                if write_datagram(&mut &*mac, &buf[..n]).is_err() {
                    break;
                }
            }
            // A timeout, or an earlier datagram found no listener.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::ConnectionRefused
                ) => {}
            Err(_) => break,
        }
    }
    let _ = mac.shutdown(std::net::Shutdown::Both);
    let _ = up.join();
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
                eprintln!("podbag: cannot renew the VM's address lease: {e}");
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

/// A restarter (`podbag __restart`) lives while a container waits to start again.
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

/// The thrashing guard: the VM has no swap, so when memory is gone the kernel may evict and
/// read back the code of running processes for minutes instead of ending one. When all
/// tasks have been stalled on memory for 300 ms of a second (PSI), memory is gone, and
/// neither the kernel's OOM killer nor a container's own limit explains it, this ends the
/// process the kernel's OOM killer would choose, then waits for it to go before it looks
/// again. (The kernel's
/// own guard, the multi-gen LRU's min_ttl_ms, called the OOM killer again before the first
/// victim's memory came back, and took a second process.)
fn guard_memory() {
    let psi = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/proc/pressure/memory")
        .and_then(|mut f| f.write_all(b"full 300000 1000000\0").map(|()| f));
    let psi = match psi {
        Ok(f) => f,
        Err(e) => {
            eprintln!("podbag: no thrashing guard (memory pressure): {e}");
            return;
        }
    };
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let cgroups = Path::new("/sys/fs/cgroup");
    let mut kills = oom_kills(&read("/proc/vmstat"));
    let mut limits = limit_events_in(cgroups);
    loop {
        let mut fd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&psi),
            events: libc::POLLPRI,
            revents: 0,
        };
        // SAFETY: one pollfd on the stack.
        if unsafe { libc::poll(&mut fd, 1, -1) } < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fd.revents & libc::POLLERR != 0 {
            return;
        }
        let (kills_now, limits_now) = (oom_kills(&read("/proc/vmstat")), limit_events_in(cgroups));
        let act = guard_should_act(
            &read("/proc/meminfo"),
            (kills, kills_now),
            (limits, limits_now),
        );
        (kills, limits) = (kills_now, limits_now);
        if !act {
            continue;
        }
        let Some(pid) = oom_choice_in(Path::new("/proc")) else {
            continue;
        };
        // This thread stalls too: look again just before, as the kernel may have ended a
        // process (whose oom_score reads 0 as it exits) or memory come back meanwhile.
        if !guard_should_act(
            &read("/proc/meminfo"),
            (kills, oom_kills(&read("/proc/vmstat"))),
            (0, 0),
        ) {
            continue;
        }
        eprintln!("podbag: the VM ran out of memory: ending process {pid}");
        // SAFETY: kill with a pid read from /proc; the worst case is a pid reused meanwhile.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let gone = Instant::now() + Duration::from_secs(10);
        while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Its memory comes back as it exits: look again after a second.
        std::thread::sleep(Duration::from_secs(1));
        (kills, limits) = (oom_kills(&read("/proc/vmstat")), limit_events_in(cgroups));
    }
}

/// Whether the guard ends a process now: memory is gone, the kernel ended no process since
/// the last look (`kills`: then, now; its victim's memory is on its way back), and no
/// container reached its own memory limit (`limits`: then, now; the kernel deals with that
/// cgroup alone, as on any Linux host).
fn guard_should_act(meminfo: &str, kills: (u64, u64), limits: (u64, u64)) -> bool {
    kills.0 == kills.1 && limits.0 == limits.1 && memory_is_gone(meminfo)
}

/// The kernel's count of OOM kills (`oom_kill` in /proc/vmstat).
fn oom_kills(vmstat: &str) -> u64 {
    vmstat
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// How often the cgroups under `root` reached their memory.high or memory.max.
fn limit_events_in(root: &Path) -> u64 {
    let mut sum = 0;
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        if let Ok(events) = std::fs::read_to_string(dir.join("memory.events")) {
            for line in events.lines() {
                let mut field = line.split_whitespace();
                if let (Some("high" | "max"), Some(n)) = (field.next(), field.next()) {
                    sum += n.parse::<u64>().unwrap_or(0);
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            dirs.extend(
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path()),
            );
        }
    }
    sum
}

/// Out of memory, not merely busy: what the kernel could still free (MemAvailable) is under
/// 5% of the VM.
fn memory_is_gone(meminfo: &str) -> bool {
    let field = |name: &str| -> Option<u64> {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    match (field("MemTotal:"), field("MemAvailable:")) {
        (Some(total), Some(available)) => available * 20 < total,
        _ => false,
    }
}

/// The process the kernel's OOM killer would choose: the highest `oom_score` among those it
/// may end (not PID 1, not `oom_score_adj` -1000, not a kernel thread's 0).
fn oom_choice_in(proc: &Path) -> Option<i32> {
    let read = |pid: i32, file: &str| -> Option<i64> {
        std::fs::read_to_string(proc.join(pid.to_string()).join(file))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    std::fs::read_dir(proc)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&pid| pid != 1)
        .filter_map(|pid| Some((read(pid, "oom_score")?, read(pid, "oom_score_adj")?, pid)))
        .filter(|&(score, adj, _)| adj != -1000 && score > 0)
        .max_by_key(|&(score, _, _)| score)
        .map(|(_, _, pid)| pid)
}

/// In the VM, its size is the containers' memory budget: they may use all of it, and the
/// thrashing guard ([`guard_memory`]) ends one that wants more. podbag's usual
/// budget, half the memory, would hold a container at a cgroup limit of half the VM, where
/// it evicts its own code for minutes instead of being killed. A budget set on the Mac
/// (`PODBAG_MEMORY_BUDGET`, forwarded to commands) still applies.
pub const VM_BUDGET: (&str, &str) = ("PODBAG_MEMORY_BUDGET", "0");

/// The VM's settings (`config.toml`) live on the data disk, which survives a restart:
/// root's usual `/etc/podbag` is in the VM's memory.
pub const VM_CONFIG_DIR: (&str, &str) = ("PODBAG_CONFIG_DIR", "/var/lib/podbag/config");

fn command(req: &CliRequest) -> std::process::Command {
    let mut cmd = std::process::Command::new("/init");
    cmd.args(&req.argv)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("HOME", "/root")
        .env(VM_BUDGET.0, VM_BUDGET.1)
        .envs(req.env.iter().map(|(k, v)| (k, v)))
        // Last: podbag is /init here and keeps its settings on the data disk, whatever
        // the Mac sent.
        .env(VM_CONFIG_DIR.0, VM_CONFIG_DIR.1)
        .env("PODBAG_BIN", "/init");
    // A command is the user's work, not the agent's: it may be ended when memory runs out.
    // SAFETY: only open, write and close between fork and exec; no allocation.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
            let fd = libc::open(c"/proc/self/oom_score_adj".as_ptr(), libc::O_WRONLY);
            if fd >= 0 {
                libc::write(fd, b"0".as_ptr().cast(), 1);
                libc::close(fd);
            }
            Ok(())
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_from_proc() {
        let stat = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\n\
                    cpu1 50 0 25 400 25 0 0 0 0 0\nintr 1 2 3\n";
        let meminfo =
            "MemTotal:        2000000 kB\nMemFree:  100 kB\nMemAvailable:    1500000 kB\n";
        let v = stats_of(stat, meminfo);
        assert_eq!(v["cpuTotal"], 1000);
        assert_eq!(v["cpuIdle"], 850);
        assert_eq!(v["cpus"], 2);
        assert_eq!(v["memTotalKiB"], 2_000_000);
        assert_eq!(v["memAvailableKiB"], 1_500_000);
    }

    fn request(env: &[(&str, &str)]) -> CliRequest {
        CliRequest {
            argv: vec!["ps".into()],
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cwd: "/".into(),
            paths: Vec::new(),
            tty: false,
            rows: 0,
            cols: 0,
            time: (0, 0),
        }
    }

    fn budget(cmd: &std::process::Command) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| *k == "PODBAG_MEMORY_BUDGET")
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn the_thrashing_guard_waits_out_the_kernels_own_kill_and_a_containers_limit() {
        let gone = "MemTotal: 1000000 kB\nMemAvailable: 40000 kB\n";
        let room = "MemTotal: 1000000 kB\nMemAvailable: 400000 kB\n";
        assert!(guard_should_act(gone, (7, 7), (3, 3)));
        assert!(!guard_should_act(room, (7, 7), (3, 3)));
        // The kernel ended a process since the last look: its memory is on its way back.
        assert!(!guard_should_act(gone, (7, 8), (3, 3)));
        // A container hit its own memory limit: the kernel deals with that cgroup alone.
        assert!(!guard_should_act(gone, (7, 7), (3, 5)));
    }

    #[test]
    fn the_thrashing_guard_reads_the_counters() {
        assert_eq!(oom_kills("pgfault 12\noom_kill 4\npgmajfault 1\n"), 4);
        assert_eq!(oom_kills("nothing"), 0);
        let root = tempfile::tempdir().unwrap();
        let events = |dir: &str, high: u64, max: u64| {
            let d = root.path().join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("memory.events"),
                format!("low 9\nhigh {high}\nmax {max}\noom 9\noom_kill 9\n"),
            )
            .unwrap();
        };
        events("podbag", 0, 0);
        events("podbag/a", 2, 1);
        events("podbag/b", 0, 3);
        assert_eq!(limit_events_in(root.path()), 6);
    }

    #[test]
    fn the_thrashing_guard_acts_only_when_memory_is_gone() {
        let meminfo = |available: u64| {
            format!("MemTotal: 1000000 kB\nMemFree: 1000 kB\nMemAvailable: {available} kB\n")
        };
        assert!(memory_is_gone(&meminfo(40_000)));
        // A page cache in use leaves much that the kernel could free: only busy.
        assert!(!memory_is_gone(&meminfo(400_000)));
        assert!(!memory_is_gone("nothing to read"));
    }

    #[test]
    fn the_thrashing_guard_chooses_as_the_oom_killer_does() {
        let proc = tempfile::tempdir().unwrap();
        let process = |pid: &str, score: i64, adj: i64| {
            let dir = proc.path().join(pid);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("oom_score"), format!("{score}\n")).unwrap();
            std::fs::write(dir.join("oom_score_adj"), format!("{adj}\n")).unwrap();
        };
        process("1", 900, 0); // init: never
        process("50", 950, -1000); // the agent: never
        process("60", 500, 0);
        process("70", 700, 0); // the biggest it may end
        process("80", 0, 0); // a kernel thread
        std::fs::create_dir(proc.path().join("self")).unwrap();
        assert_eq!(oom_choice_in(proc.path()), Some(70));
        std::fs::remove_dir_all(proc.path().join("70")).unwrap();
        assert_eq!(oom_choice_in(proc.path()), Some(60));
    }

    #[test]
    fn a_command_in_the_vm_has_no_budget_but_the_one_the_mac_sends() {
        assert_eq!(budget(&command(&request(&[]))).as_deref(), Some("0"));
        let sent = request(&[("PODBAG_MEMORY_BUDGET", "50%")]);
        assert_eq!(budget(&command(&sent)).as_deref(), Some("50%"));
    }

    #[test]
    fn a_command_in_the_vm_keeps_its_settings_on_the_data_disk() {
        let sent = request(&[("PODBAG_CONFIG_DIR", "/Users/me/.config/podbag")]);
        let config = command(&sent)
            .get_envs()
            .find(|(k, _)| *k == "PODBAG_CONFIG_DIR")
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()));
        assert_eq!(config.as_deref(), Some("/var/lib/podbag/config"));
    }
}
