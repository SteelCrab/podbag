//! The rootless network session (design 6.8.4): one process per user (per runtime
//! directory) that holds a user namespace with the full subordinate ID map and a network
//! namespace owned by it. Bridges and NAT rules live in that network namespace; `pasta`
//! connects it to the host. Rootless bridge containers are created inside the session's
//! user namespace, so their veth ends can be moved between the two network namespaces.
//!
//! The first container that needs the session starts it; each container adds its ID to
//! `refs`; the container that removes the last ID stops it.

use crate::helper::{StatusChannel, StatusMessage};
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result, idmap};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

/// The DNS address pasta answers on inside the session (see `netfiles::PASTA_DNS`).
const DNS_FORWARD: &str = crate::netfiles::PASTA_DNS;

/// Where pasta maps the host's loopback (dropped for containers by the forward chain).
const HOST_LOOPBACK: &str = "169.254.1.3";

fn dir(dirs: &Dirs) -> PathBuf {
    dirs.runtime.join("session")
}

fn io_error(what: &str, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot {what} the rootless network session"),
        &e,
        "check that the openpod runtime directory is writable",
    )
}

/// The session's lock, held while it is started, stopped, or its references change.
fn lock(dirs: &Dirs) -> Result<File> {
    let d = dir(dirs);
    let mut builder = crate::store::perm::private_dir_builder();
    builder
        .recursive(true)
        .create(&d)
        .map_err(|e| io_error("use", e))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(d.join("lock"))
        .map_err(|e| io_error("lock", e))?;
    file.lock().map_err(|e| io_error("lock", e))?;
    Ok(file)
}

fn read_pid(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The holder process, if it still runs (and is ours: a fork of `openpod __session`).
fn holder(dirs: &Dirs) -> Option<u32> {
    let pid = read_pid(&dir(dirs).join("pid"))?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let ours = cmdline.split(|&b| b == 0).any(|arg| arg == b"__session");
    ours.then_some(pid)
}

fn read_refs(dirs: &Dirs) -> Vec<String> {
    std::fs::read_to_string(dir(dirs).join("refs"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .filter(|l| !l.is_empty())
        .collect()
}

fn write_refs(dirs: &Dirs, refs: &[String]) -> Result<()> {
    let mut text = refs.join("\n");
    text.push('\n');
    std::fs::write(dir(dirs).join("refs"), text).map_err(|e| io_error("update", e))
}

/// Make sure the session runs and count container `id` as one of its users. Returns the
/// holder's PID.
pub fn acquire(dirs: &Dirs, id: &str) -> Result<u32> {
    let _lock = lock(dirs)?;
    let pid = match holder(dirs) {
        Some(pid) => pid,
        None => {
            stop(dirs);
            start()?
        }
    };
    let mut refs = read_refs(dirs);
    if !refs.iter().any(|r| r == id) {
        refs.push(id.to_string());
    }
    write_refs(dirs, &refs)?;
    Ok(pid)
}

/// Container `id` no longer needs the session. Entries of containers whose shims are gone
/// are dropped too; with no user left, the session stops.
pub fn release(dirs: &Dirs, id: &str) {
    let Ok(_lock) = lock(dirs) else { return };
    let refs: Vec<String> = read_refs(dirs)
        .into_iter()
        .filter(|r| r != id && crate::state::shim_running(dirs, r))
        .collect();
    if refs.is_empty() {
        stop(dirs);
        let _ = std::fs::remove_file(dir(dirs).join("refs"));
    } else {
        let _ = write_refs(dirs, &refs);
    }
}

/// Whether `pid` runs the program called `name` (its `comm`): a PID file can be stale and
/// its number reused by an unrelated process.
fn runs(pid: u32, name: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim_end() == name)
}

/// Stop pasta and the holder, if they run. Only processes that still are pasta and our
/// session holder get a signal.
fn stop(dirs: &Dirs) {
    let d = dir(dirs);
    if let Some(pid) = read_pid(&d.join("pasta.pid")).filter(|&p| runs(p, "pasta")) {
        // SAFETY: kill has no memory arguments; the process is pasta.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    if let Some(pid) = holder(dirs) {
        // SAFETY: as above; `holder` checked that the process is our session.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    let _ = std::fs::remove_file(d.join("pasta.pid"));
    let _ = std::fs::remove_file(d.join("pid"));
}

/// The pasta program: `OPENPOD_PASTA`, or `pasta` in PATH.
pub(crate) fn pasta() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &std::path::Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if let Some(p) = std::env::var_os("OPENPOD_PASTA") {
        let p = PathBuf::from(p);
        return executable(&p).then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("pasta"))
        .find(|p| executable(p))
}

fn missing_pasta() -> Error {
    Error::new(
        ErrorKind::MissingRequirement,
        "cannot start the rootless network",
        "pasta was not found (rootless bridge networking needs it)",
        "install the passt package, or run with --network host or --network none",
    )
}

/// Start `openpod __session` (in its own systemd scope when there is one, so it does not
/// live in a container's cgroup) and wait until it reports the holder's PID.
fn start() -> Result<u32> {
    if pasta().is_none() {
        return Err(missing_pasta());
    }
    // In a scope of its own only where containers get one too (systemd-run and the
    // user's service manager); otherwise as a plain child, like the containers.
    let scope = crate::cgroup::systemd_run()
        .filter(|_| crate::cgroup::user_manager())
        .map(|systemd_run| crate::helper::Scope {
            systemd_run,
            slice: format!(
                "{}.slice",
                crate::cgroup::parent_name().unwrap_or_else(|_| "openpod".into())
            ),
            unit: None,
            delegate: false,
        });
    use std::process::Stdio;
    let mut helper = crate::helper::spawn_with_args(
        &["__session"],
        Stdio::null(),
        Stdio::null(),
        Stdio::inherit(),
        scope.as_ref(),
    )?;
    let pid = helper.wait_started()?;
    let _ = helper.child.wait();
    Ok(pid)
}

/// Join the session's user namespace. The calling process must be single-threaded.
pub fn join_userns(pid: u32) -> Result<()> {
    let ns = File::open(format!("/proc/{pid}/ns/user")).map_err(|e| io_error("join", e))?;
    // SAFETY: setns on a namespace descriptor we opened; the caller is single-threaded.
    if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWUSER) } != 0 {
        return Err(io_error("join", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// `openpod __session`: create the holder (new user and network namespaces, mapped like a
/// container's), start pasta for its network, report the holder's PID, and exit. The
/// holder stays, asleep, until the last container stops it.
pub fn session_main() -> i32 {
    let mut status = StatusChannel::take();
    crate::shim::close_inherited_fds_on_exec();
    match create() {
        Ok(pid) => {
            status.report(&StatusMessage::Started { pid });
            0
        }
        Err(e) => status.fail(e),
    }
}

fn create() -> Result<u32> {
    let pasta = pasta().ok_or_else(missing_pasta)?;
    // SAFETY: these calls have no preconditions.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let fail = |what: &str, e: std::io::Error| io_error(what, e);
    let mut fds = [0i32; 2];
    // SAFETY: a valid two-element array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(fail("create", std::io::Error::last_os_error()));
    }
    // SAFETY: two fresh descriptors.
    let (sync_r, sync_w) = unsafe {
        use std::os::fd::FromRawFd;
        (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1]))
    };
    let record = dir(&crate::paths::dirs()?).join("pid");
    let flags = (libc::CLONE_NEWUSER | libc::CLONE_NEWNET) as u64;
    let args = super::spawn::clone_args(flags);
    // SAFETY: clone3 without CLONE_VM behaves like fork; this process is single-threaded.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const super::spawn::CloneArgs,
            std::mem::size_of::<super::spawn::CloneArgs>(),
        )
    };
    if ret == -1 {
        return Err(fail("create", std::io::Error::last_os_error()));
    }
    if ret == 0 {
        drop(sync_w);
        holder_main(sync_r, record);
    }
    let pid = ret as u32;
    drop(sync_r);
    if let Err(e) = idmap::apply_id_maps(pid, euid, egid) {
        // SAFETY: our own child.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        return Err(e);
    }
    // The record first: the holder checks it from now on.
    if let Err(e) = std::fs::write(&record, format!("{pid}\n")) {
        // SAFETY: our own child.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        return Err(io_error("record", e));
    }
    let mut sync_w = sync_w;
    let _ = sync_w.write_all(&[1]);
    drop(sync_w);
    // pasta sets up its interface in the holder's network namespace, then goes to the
    // background. Its error output goes to a file: a pipe would stay open in the daemon.
    let dirs = crate::paths::dirs()?;
    let d = dir(&dirs);
    let log = File::create(d.join("pasta.log")).map_err(|e| fail("start pasta for", e))?;
    let started = std::process::Command::new(&pasta)
        // The gateway address must not lead to the host's loopback, where services meant
        // only for the host listen. This pasta maps the gateway even with --no-map-gw, so
        // the mapping moves to an address of its own, which the network's forward chain
        // drops (netlink::nft::HOST_LOOPBACK).
        .args([
            "--config-net",
            "--quiet",
            "--map-host-loopback",
            HOST_LOOPBACK,
            "--dns-forward",
            DNS_FORWARD,
            "--pid",
        ])
        .arg(d.join("pasta.pid"))
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .status()
        .map_err(|e| fail("start pasta for", e))?;
    if !started.success() {
        // SAFETY: our own child.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        return Err(Error::new(
            ErrorKind::Runtime,
            "cannot start the rootless network",
            format!(
                "pasta failed: {}",
                std::fs::read_to_string(d.join("pasta.log"))
                    .unwrap_or_default()
                    .trim()
            ),
            "check that pasta works: `pasta --version`; see README \"Requirements\"",
        ));
    }
    Ok(pid)
}

/// The holder: become root of the new user namespace, bring loopback up, turn on IPv4
/// forwarding, then wait in a session of its own. It ends by itself once its record
/// (`record`, the session's `pid` file) is gone or names another process, so a lost
/// session does not linger; pasta follows it out.
fn holder_main(mut sync: File, record: PathBuf) -> ! {
    let mut byte = [0u8; 1];
    if !matches!(sync.read(&mut byte), Ok(1)) {
        // SAFETY: leave the forked child without running destructors.
        unsafe { libc::_exit(1) };
    }
    drop(sync);
    let ok = super::init::become_root().is_ok()
        && (|| -> std::io::Result<()> {
            let mut s = crate::netlink::Socket::route()?;
            if let Some(lo) = s.link_index("lo")? {
                s.request(crate::netlink::route::set_up(lo))?;
            }
            std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
        })()
        .is_ok();
    if !ok {
        // SAFETY: as above.
        unsafe { libc::_exit(1) };
    }
    // SAFETY: plain system calls; the holder only waits from here on.
    unsafe {
        libc::setsid();
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        for fd in 0..3 {
            libc::dup2(null, fd);
        }
    }
    // SAFETY: getpid has no preconditions.
    let me = unsafe { libc::getpid() } as u32;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if read_pid(&record) != Some(me) {
            // SAFETY: leave without running destructors.
            unsafe { libc::_exit(0) };
        }
    }
}
