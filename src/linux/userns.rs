//! Run a function as root of a user namespace with the caller's subordinate IDs mapped.
//!
//! Rootless store work needs this: image files must be owned by the IDs the image names
//! (100, 101, …), which on the host are subordinate IDs only a mapped namespace can set.

use crate::{Error, ErrorKind, Result, idmap};
use rustix::pipe::{PipeFlags, pipe_with};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs::File;
use std::io::{Read, Write};

/// Run `f` in a new user namespace as its root, and return its result.
/// As real root there is nothing to map, so `f` runs directly.
/// The calling process must be single-threaded (a helper process).
pub fn run_in_userns<T: Serialize + DeserializeOwned>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    // SAFETY: these calls have no preconditions.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if euid == 0 {
        return f();
    }
    let pipe = || {
        pipe_with(PipeFlags::CLOEXEC).map_err(|e| {
            Error::io(
                "cannot enter a user namespace",
                &e.into(),
                "check the open file limit (`ulimit -n`)",
            )
        })
    };
    let (sync_read, sync_write) = pipe()?;
    let (result_read, result_write) = pipe()?;

    // SAFETY: fork-like clone in a single-threaded process.
    let pid = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &super::spawn::clone_args(libc::CLONE_NEWUSER as u64),
            std::mem::size_of::<super::spawn::CloneArgs>(),
        )
    };
    if pid == -1 {
        let e = std::io::Error::last_os_error();
        return Err(Error::new(
            ErrorKind::Runtime,
            "cannot create a user namespace",
            e.to_string(),
            "check that user namespaces are enabled: `sysctl user.max_user_namespaces` must be above 0",
        ));
    }
    if pid == 0 {
        drop(sync_write);
        drop(result_read);
        let mut byte = [0u8; 1];
        if !matches!(File::from(sync_read).read(&mut byte), Ok(1)) {
            // SAFETY: leave without running the parent's destructors.
            unsafe { libc::_exit(1) };
        }
        let result: Result<T> = super::init::become_root()
            .map_err(|e| {
                super::userns_denial(true, Some(&e), None).unwrap_or_else(|| {
                    Error::io(
                        "cannot switch to root in the user namespace",
                        &e,
                        "check the ranges for your user in /etc/subuid and /etc/subgid",
                    )
                })
            })
            .and_then(|()| {
                // SAFETY: prctl with these arguments has no memory preconditions.
                unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
                f()
            });
        let bytes = serde_json::to_vec(&result).unwrap_or_default();
        let _ = File::from(result_write).write_all(&bytes);
        // SAFETY: as above.
        unsafe { libc::_exit(0) };
    }

    let pid = pid as u32;
    drop(sync_read);
    drop(result_write);
    if let Err(e) = idmap::apply_id_maps(pid, euid, egid) {
        let denied =
            (e.kind == ErrorKind::Runtime).then(|| super::userns_denial(true, None, Some(pid)));
        reap(pid, true);
        return Err(denied.flatten().unwrap_or(e));
    }
    let _ = File::from(sync_write).write_all(&[1]);
    let mut bytes = Vec::new();
    let _ = File::from(result_read).read_to_end(&mut bytes);
    let status = reap(pid, false);
    if bytes.is_empty() {
        return Err(Error::new(
            ErrorKind::Internal,
            "the user namespace helper ended without a result",
            format!("wait status {status}"),
            "run the command again; if it keeps failing, report this bug",
        ));
    }
    serde_json::from_slice::<Result<T>>(&bytes).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "the user namespace helper sent an unreadable result",
            e.to_string(),
            "report this bug",
        )
    })?
}

fn reap(pid: u32, kill: bool) -> i32 {
    let mut status = 0;
    // SAFETY: pid is our own child; status points to a valid int.
    unsafe {
        if kill {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        while libc::waitpid(pid as libc::pid_t, &mut status, 0) == -1
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {}
    }
    status
}
