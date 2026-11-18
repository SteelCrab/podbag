//! Create the container process and wait for it.

use super::init::child_main;
use crate::spec::ContainerSpec;
use crate::{Error, ErrorKind, Result, idmap};
use rustix::pipe::{PipeFlags, pipe_with};
use std::fs::File;
use std::io::{Read, Write};

/// A container process whose command has started.
pub struct Spawned {
    /// The PID in the caller's PID namespace.
    pub pid: u32,
}

/// `struct clone_args` from linux/sched.h (version 2, 88 bytes).
#[repr(C)]
#[derive(Default)]
pub(super) struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// clone3 arguments for a fork-like clone with `flags`.
pub(super) fn clone_args(flags: u64) -> CloneArgs {
    CloneArgs {
        flags,
        exit_signal: libc::SIGCHLD as u64,
        ..Default::default()
    }
}

/// File descriptors (in the calling process) that become the command's stdin, stdout and
/// stderr. With `tty`, the descriptors are a terminal and the command gets it as its
/// controlling terminal.
#[derive(Clone, Copy, Debug)]
pub struct ContainerStdio {
    pub fds: [i32; 3],
    pub tty: bool,
    /// Create the terminal inside the container (its own /dev/pts) and send its
    /// controlling side back on this socket. `fds` is then unused.
    pub tty_socket: Option<i32>,
}

/// `CLONE_INTO_CGROUP` from linux/sched.h: start the child in the cgroup `args.cgroup`.
pub(super) const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// How the container gets its user namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserNs {
    /// Root mode: none.
    Host,
    /// Rootless: a new one, mapped with newuidmap and newgidmap.
    New,
    /// Rootless: this process already joined one (the network session's).
    Joined,
}

/// Whose child the container becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parent {
    /// The caller: a shim that serves the container itself.
    Caller,
    /// The caller's parent (`CLONE_PARENT`): a starter makes the container for the keeper,
    /// which reaps it. The caller cannot wait for it.
    Grandparent,
}

/// Create the container process in its own namespaces (cgroup namespace included) and,
/// when `cgroup` is given, directly in that cgroup. Returns after its command was executed.
pub fn spawn(
    spec: &ContainerSpec,
    stdio: ContainerStdio,
    cgroup: Option<std::os::fd::BorrowedFd<'_>>,
    userns: UserNs,
    parent: Parent,
    before_start: &mut dyn FnMut(u32) -> Result<()>,
) -> Result<Spawned> {
    // SAFETY: these calls have no preconditions.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let rootless = userns != UserNs::Host;

    let pipe = || {
        pipe_with(PipeFlags::CLOEXEC).map_err(|e| {
            Error::io(
                "cannot create the container",
                &e.into(),
                "check the open file limit (`ulimit -n`)",
            )
        })
    };
    let (sync_read, sync_write) = pipe()?;
    let (status_read, status_write) = pipe()?;

    let mut flags = libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWCGROUP;
    if userns == UserNs::New {
        flags |= libc::CLONE_NEWUSER;
    }
    if spec.network != crate::network::NetworkMode::Host {
        flags |= libc::CLONE_NEWNET;
    }
    let mut args = clone_args(flags as u64);
    if let Some(fd) = cgroup {
        args.flags |= CLONE_INTO_CGROUP;
        args.cgroup = std::os::fd::AsRawFd::as_raw_fd(&fd) as u64;
    }
    if parent == Parent::Grandparent {
        // The kernel takes the exit signal from this process (SIGCHLD); asking for one
        // here is an error.
        args.flags |= libc::CLONE_PARENT as u64;
        args.exit_signal = 0;
    }
    // A child of ours is killed and reaped; a child of our parent is killed, and reaped
    // there.
    let end = |pid: u32| match parent {
        Parent::Caller => kill_and_reap(pid),
        // SAFETY: kill has no memory arguments.
        Parent::Grandparent => unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        },
    };

    // SAFETY: clone3 without CLONE_VM behaves like fork. This process is single-threaded,
    // so the child may run any code before it calls execve.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    if ret == -1 {
        let e = std::io::Error::last_os_error();
        return Err(Error::new(
            ErrorKind::Runtime,
            "cannot create the container namespaces",
            e.to_string(),
            "check that user namespaces are enabled: `sysctl user.max_user_namespaces` must be above 0",
        ));
    }
    if ret == 0 {
        drop(sync_write);
        drop(status_read);
        child_main(spec, rootless, sync_read, status_write, stdio);
    }

    let pid = ret as u32;
    drop(sync_read);
    drop(status_write);

    if userns == UserNs::New {
        if let Err(e) = idmap::apply_id_maps(pid, euid, egid) {
            // A missing tool or subuid entry is its own problem; a failing tool may be AppArmor.
            // Read the child's AppArmor label before the child is gone.
            let denied = (e.kind == ErrorKind::Runtime)
                .then(|| super::userns_denial(rootless, None, Some(pid)));
            end(pid);
            return Err(denied.flatten().unwrap_or(e));
        }
    }

    // Network and similar setup from outside, while the child waits.
    if let Err(e) = before_start(pid) {
        end(pid);
        return Err(e);
    }

    // Let the child continue. If it already died, the status pipe below is empty.
    let _ = File::from(sync_write).write_all(&[1]);

    // The child writes one 0 byte right before execve. The pipe closes on a successful
    // exec, so "0 byte, then end" means started; anything else is an error.
    let mut report = Vec::new();
    let _ = File::from(status_read).read_to_end(&mut report);
    if report == [0] {
        return Ok(Spawned { pid });
    }
    let status = match parent {
        Parent::Caller => wait(pid).unwrap_or(-1),
        Parent::Grandparent => -1,
    };
    if report.is_empty() {
        return Err(Error::new(
            ErrorKind::Runtime,
            "the container process ended before it started",
            format!(
                "it exited with status {status} without reporting an error (out of memory, or killed)"
            ),
            "run the command again; check `dmesg` for an out-of-memory kill",
        ));
    }
    let report = report.strip_prefix(&[0]).unwrap_or(&report);
    Err(serde_json::from_slice::<Error>(report).unwrap_or_else(|e| {
        Error::new(
            ErrorKind::Internal,
            "the container process sent an unreadable error",
            e.to_string(),
            "report this bug",
        )
    }))
}

/// Fork with clone3, directly into `cgroup` when given. Returns 0 in the child and the
/// child's PID in the parent. The caller must be single-threaded, as with fork.
pub(super) fn fork_into(cgroup: Option<std::os::fd::BorrowedFd<'_>>) -> std::io::Result<u32> {
    let mut args = clone_args(0);
    if let Some(fd) = cgroup {
        args.flags |= CLONE_INTO_CGROUP;
        args.cgroup = std::os::fd::AsRawFd::as_raw_fd(&fd) as u64;
    }
    // SAFETY: clone3 without CLONE_VM behaves like fork; the caller is single-threaded.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    if ret == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as u32)
    }
}

/// Wait for the process to end. Returns its exit code, or 128 + signal number.
pub fn wait(pid: u32) -> Result<i32> {
    let mut status = 0;
    loop {
        // SAFETY: status points to a valid int.
        let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
        if rc != -1 {
            break;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(Error::io(
                "cannot wait for the container",
                &e,
                "report this bug",
            ));
        }
    }
    if libc::WIFEXITED(status) {
        Ok(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Ok(128 + libc::WTERMSIG(status))
    } else {
        Ok(128)
    }
}

fn kill_and_reap(pid: u32) {
    // SAFETY: pid is our own child; kill and waitpid have no memory preconditions.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
        libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0);
    }
}
