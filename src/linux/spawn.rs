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

/// Create the container process. Returns after its command was executed.
pub fn spawn(spec: &ContainerSpec, stdio: ContainerStdio) -> Result<Spawned> {
    // SAFETY: these calls have no preconditions.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let rootless = euid != 0;

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

    let mut flags =
        libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC;
    if rootless {
        flags |= libc::CLONE_NEWUSER;
    }
    let args = clone_args(flags as u64);

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

    if rootless {
        if let Err(e) = idmap::apply_id_maps(pid, euid, egid) {
            // A missing tool or subuid entry is its own problem; a failing tool may be AppArmor.
            // Read the child's AppArmor label before the child is gone.
            let denied = (e.kind == ErrorKind::Runtime)
                .then(|| super::userns_denial(rootless, None, Some(pid)));
            kill_and_reap(pid);
            return Err(denied.flatten().unwrap_or(e));
        }
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
    let status = wait(pid).unwrap_or(-1);
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
