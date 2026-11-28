//! The container process: it sets itself up and executes the command.

use super::rootfs::enter_rootfs;
use crate::spec::ContainerSpec;
use crate::{Error, ErrorKind, Result};
use std::convert::Infallible;
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

/// Entry point of the cloned child. Never returns: it executes the command or exits.
pub(super) fn child_main(
    spec: &ContainerSpec,
    rootless: bool,
    sync_read: OwnedFd,
    status_write: OwnedFd,
    stdio: super::spawn::ContainerStdio,
    joins: &super::spawn::Joins,
    nfs: &super::mounts::NfsMounts,
    gpu: Option<&crate::gpu::nvidia::Setup>,
) -> ! {
    let mut sync = File::from(sync_read);
    let mut byte = [0u8; 1];
    if !matches!(sync.read(&mut byte), Ok(1)) {
        // The parent gave up (for example, ID mapping failed).
        // SAFETY: _exit ends the process without running destructors.
        unsafe { libc::_exit(1) };
    }
    drop(sync);

    let status_fd = std::os::fd::AsRawFd::as_raw_fd(&status_write);
    let early = if stdio.tty_socket.is_none() {
        connect_stdio(stdio)
    } else {
        Ok(())
    };
    let error = match early
        .and_then(|()| setup_and_exec(spec, rootless, status_fd, stdio.tty_socket, joins, nfs, gpu))
    {
        Err(e) => e,
        Ok(never) => match never {},
    };
    let mut status = File::from(status_write);
    let _ = status.write_all(&serde_json::to_vec(&error).unwrap_or_default());
    // SAFETY: as above.
    unsafe { libc::_exit(error.exit_code()) }
}

/// Put the chosen descriptors on 0, 1 and 2, and start a new session. With a terminal, it
/// becomes the session's controlling terminal. Without one the session has none, so the
/// caller's terminal is out of reach (`/dev/tty`, TIOCSTI); the shim or the exec helper
/// passes the terminal's signals on instead.
pub(super) fn connect_stdio(stdio: super::spawn::ContainerStdio) -> Result<()> {
    let fail = |what: &str| os_error(what, std::io::Error::last_os_error(), "report this bug");
    for (target, &fd) in stdio.fds.iter().enumerate() {
        // SAFETY: dup2 and fcntl on descriptors owned by this process.
        unsafe {
            if fd == target as i32 {
                // dup2 onto itself keeps FD_CLOEXEC; clear it so the command keeps it.
                libc::fcntl(fd, libc::F_SETFD, 0);
            } else if libc::dup2(fd, target as i32) == -1 {
                return Err(fail("cannot connect the container's standard streams"));
            }
        }
    }
    // SAFETY: plain system calls on our own process and stdin.
    unsafe {
        if libc::setsid() == -1 {
            return Err(fail("cannot start the container's session"));
        }
        if stdio.tty && libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
            return Err(fail("cannot give the container its terminal"));
        }
    }
    Ok(())
}

fn setup_and_exec(
    spec: &ContainerSpec,
    rootless: bool,
    status_fd: i32,
    tty_socket: Option<i32>,
    joins: &super::spawn::Joins,
    nfs: &super::mounts::NfsMounts,
    gpu: Option<&crate::gpu::nvidia::Setup>,
) -> Result<Infallible> {
    // First, while the host's files are in view: the pod's namespaces.
    joins.enter()?;
    if rootless {
        become_root().map_err(|e| {
            setup_error(
                true,
                "cannot switch to the container user",
                e,
                "check the ranges for your user in /etc/subuid and /etc/subgid",
            )
        })?;
    }
    // Set after the credentials change: changing credentials clears it.
    // SAFETY: prctl with these arguments has no memory preconditions.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };

    // A shared UTS namespace keeps its name: it is the pod's (or the host's).
    if spec.namespaces.uts == crate::spec::Namespace::Own {
        rustix::system::sethostname(spec.hostname.as_bytes()).map_err(|e| {
            setup_error(
                rootless,
                "cannot set the container hostname",
                e.into(),
                "use a hostname of at most 64 characters",
            )
        })?;
    }
    if spec.own_netns() {
        loopback_up()?;
    }
    enter_rootfs(
        &spec.rootfs,
        &spec.mounts,
        spec.network != crate::network::NetworkMode::Host,
        rootless,
        &spec.security,
        nfs,
        gpu,
    )?;
    if let Some(sock) = tty_socket {
        // Open the terminal in the container's own /dev/pts, so the command can find it.
        let (master, slave) = super::pty::open(None)?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&slave);
        mount_console(fd)?;
        // Before the command runs: it may ask for the size at once.
        if let Some((rows, cols)) = spec.console_size {
            super::pty::set_size(std::os::fd::AsRawFd::as_raw_fd(&master), rows, cols);
        }
        super::pty::send_fd(sock, std::os::fd::AsRawFd::as_raw_fd(&master))
            .and_then(|()| super::pty::send_fd(sock, fd))
            .map_err(|e| {
                os_error(
                    "cannot hand over the container terminal",
                    e,
                    "report this bug",
                )
            })?;
        drop(master);
        connect_stdio(super::spawn::ContainerStdio {
            fds: [fd; 3],
            tty: true,
            tty_socket: None,
        })?;
    }

    let workdir = spec.workdir.as_deref().unwrap_or("/");
    let workdir_error = |e| {
        os_error(
            &format!("cannot enter the working directory {workdir}"),
            e,
            "choose a working directory the container can create, such as /app",
        )
    };
    // Created as root, then entered as the container user, like Docker.
    std::fs::create_dir_all(workdir).map_err(workdir_error)?;
    if spec.security.read_only_rootfs {
        super::rootfs::make_root_read_only(rootless)?;
    }
    let creds = crate::credentials::resolve_user(
        spec.user.as_deref(),
        &std::fs::read_to_string("/etc/passwd").unwrap_or_default(),
        &std::fs::read_to_string("/etc/group").unwrap_or_default(),
    )?;
    super::security::apply(&spec.security)?;
    switch_user(&with_groups(creds.clone(), &spec.security))?;
    die_with_parent(status_fd);
    std::env::set_current_dir(workdir).map_err(workdir_error)?;

    let env = spec.effective_env(&creds.home);
    let program = resolve(&spec.command[0], &env)?;
    reset_signals();
    // Tell the parent the setup worked. The pipe closes on a successful exec.
    // SAFETY: writing one byte from a valid buffer to our own descriptor.
    unsafe { libc::write(status_fd, [0u8].as_ptr().cast(), 1) };
    exec(&program, &spec.command, &env)
}

fn mount_console(terminal: i32) -> Result<()> {
    let path = Path::new("/dev/console");
    let mount = || -> std::io::Result<()> {
        match File::create_new(path) {
            Ok(_) => {}
            // A caller may have supplied its own /dev/console mount.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e) => return Err(e),
        }
        rustix::mount::mount_bind(format!("/proc/self/fd/{terminal}").as_str(), path)
            .map_err(Into::into)
    };
    mount().map_err(|e| {
        os_error(
            "cannot mount the container console",
            e,
            "check PTY mount support",
        )
    })
}

/// Bring up `lo` in this process's own network namespace (it holds CAP_NET_ADMIN there).
fn loopback_up() -> Result<()> {
    let up = || -> std::io::Result<()> {
        let mut s = crate::netlink::Socket::route()?;
        if let Some(lo) = s.link_index("lo")? {
            s.request(crate::netlink::route::set_up(lo))?;
        }
        Ok(())
    };
    up().map_err(|e| {
        os_error(
            "cannot bring up the loopback interface",
            e,
            "report this bug",
        )
    })
}

/// End this process when its parent (the shim) ends. Changing credentials clears
/// PR_SET_PDEATHSIG, so call this after the last change. If the parent already ended,
/// the status pipe has lost its reader (POLLERR): end now.
pub(super) fn die_with_parent(status_fd: i32) {
    let mut pipe = libc::pollfd {
        fd: status_fd,
        events: 0,
        revents: 0,
    };
    // SAFETY: prctl with integer arguments; poll on one valid pollfd.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        if libc::poll(&mut pipe, 1, 0) > 0 && pipe.revents & libc::POLLERR != 0 {
            libc::_exit(1);
        }
    }
}

/// Start the command like Docker does: no blocked signals, every signal at its default action.
/// Ignored signals survive execve, and this process inherited podbag's (Rust ignores SIGPIPE).
pub(super) fn reset_signals() {
    // SAFETY: sa and set are initialized before use; SIG_DFL needs no handler code.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in 1..libc::SIGRTMAX() {
            if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
    }
}

/// Become root of the new user namespace: UID 0 and GID 0, no supplementary groups.
pub(super) fn become_root() -> std::io::Result<()> {
    // SAFETY: plain system calls with integer arguments.
    let ok = unsafe {
        libc::setresgid(0, 0, 0) == 0
            && libc::setgroups(0, std::ptr::null()) == 0
            && libc::setresuid(0, 0, 0) == 0
    };
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The user's credentials with further supplementary groups (sorted, each once), and its
/// own group among them when the settings ask (Kubernetes runtimes do that).
pub(super) fn with_groups(
    mut c: crate::credentials::Credentials,
    security: &crate::spec::Security,
) -> crate::credentials::Credentials {
    c.groups.extend_from_slice(&security.groups);
    if security.own_group_too {
        c.groups.push(c.gid);
    }
    c.groups.sort_unstable();
    c.groups.dedup();
    c
}

/// Become the container user: supplementary groups, then group, then user.
pub(super) fn switch_user(c: &crate::credentials::Credentials) -> Result<()> {
    let groups: Vec<libc::gid_t> = c.groups.iter().map(|&g| g as libc::gid_t).collect();
    // SAFETY: plain system calls; the groups pointer is valid for groups.len() entries.
    let ok = unsafe {
        libc::setgroups(groups.len(), groups.as_ptr()) == 0
            && libc::setresgid(c.gid, c.gid, c.gid) == 0
            && libc::setresuid(c.uid, c.uid, c.uid) == 0
    };
    if ok {
        return Ok(());
    }
    Err(os_error(
        &format!("cannot switch to user {}:{}", c.uid, c.gid),
        std::io::Error::last_os_error(),
        "in rootless mode, IDs above the range in /etc/subuid and /etc/subgid cannot be used",
    ))
}

/// A failed setup step, explained as an AppArmor restriction when that is the cause.
fn setup_error(rootless: bool, what: &str, e: std::io::Error, next: &str) -> Error {
    super::userns_denial(rootless, Some(&e), None).unwrap_or_else(|| os_error(what, e, next))
}

/// Find the program: a path with a slash is used as is; a bare name is searched in PATH.
pub(super) fn resolve(name: &str, env: &[(String, String)]) -> Result<PathBuf> {
    if name.contains('/') {
        return Ok(PathBuf::from(name));
    }
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
        .ok_or_else(|| not_found(name, Some(path)))
}

pub(super) fn exec(
    program: &Path,
    argv: &[String],
    env: &[(String, String)],
) -> Result<Infallible> {
    let invalid = |what: &str| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("cannot execute {}", argv[0]),
            format!("the {what} contains a NUL byte"),
            "remove the NUL byte",
        )
    };
    let c_program =
        CString::new(program.as_os_str().as_encoded_bytes()).map_err(|_| invalid("command"))?;
    let c_argv = argv
        .iter()
        .map(|a| CString::new(a.as_str()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| invalid("argument list"))?;
    let c_env = env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| invalid("environment"))?;

    let mut argv_ptrs: Vec<*const libc::c_char> = c_argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = c_env.iter().map(|e| e.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());

    // SAFETY: all pointers refer to NUL-terminated strings that outlive the call, and both
    // arrays end with a null pointer.
    unsafe { libc::execve(c_program.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr()) };

    let e = std::io::Error::last_os_error();
    let errno = e.raw_os_error().unwrap_or(0);
    if let Some((why, next)) = explain(errno, program) {
        let kind = if errno == libc::ENOENT {
            ErrorKind::CommandNotFound
        } else {
            ErrorKind::CommandNotExecutable
        };
        return Err(Error::new(
            kind,
            format!("cannot execute {}", argv[0]),
            why,
            next,
        ));
    }
    if errno == libc::ENOENT {
        return Err(not_found(&argv[0], None));
    }
    Err(Error::new(
        ErrorKind::CommandNotExecutable,
        format!("cannot execute {}", argv[0]),
        e.to_string(),
        "check that the file is a program for this CPU and has execute permission",
    ))
}

/// Why a program that is there could not start (missing loader or script interpreter,
/// another CPU), from the start of the file. Paths are the container's.
fn explain(errno: i32, program: &Path) -> Option<(String, String)> {
    // Only regular files are read, and opening never waits (a FIFO would).
    let read_head = |p: &Path| {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(p)
            .ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut buf = Vec::new();
        file.take(crate::diagnose::HEAD as u64)
            .read_to_end(&mut buf)
            .ok()?;
        Some(buf)
    };
    crate::diagnose::diagnose_exec(
        errno,
        program,
        read_head,
        |p| p.exists(),
        crate::elf::Machine::host(),
    )
}

fn not_found(name: &str, searched_path: Option<&str>) -> Error {
    let mut why = "no such file in the container".to_string();
    if let Some(path) = searched_path {
        why.push_str(&format!(" (searched PATH: {path})"));
    }
    Error::new(
        ErrorKind::CommandNotFound,
        format!("cannot execute {name}"),
        why,
        "check the command name; the program must exist inside the root filesystem",
    )
}

pub(super) fn os_error(what: &str, e: std::io::Error, next: &str) -> Error {
    Error::new(ErrorKind::Runtime, what, e.to_string(), next)
}
