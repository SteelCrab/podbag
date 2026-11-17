//! The `openpod __shim <id>` helper: it creates one container and watches it.

use crate::helper::{STATUS_FD, StatusMessage};
use crate::{Error, paths, state};
use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;

/// Run the shim for container `id`. Returns the process exit code.
pub fn shim_main(id: &str) -> i32 {
    let mut status = take_status_fd();
    #[cfg(target_os = "linux")]
    close_inherited_fds_on_exec();
    if let Err(e) = crate::id::check(id) {
        return fail(&mut status, e);
    }
    let dirs = match paths::dirs() {
        Ok(d) => d,
        Err(e) => return fail(&mut status, e),
    };
    let code = run(id, &dirs, &mut status);
    let _ = state::remove_container_dir(&dirs, id);
    code
}

#[cfg(target_os = "linux")]
fn run(id: &str, dirs: &paths::Dirs, status: &mut Option<File>) -> i32 {
    use crate::linux;
    let spec = match state::read_config(dirs, id).and_then(|s| s.validate().map(|()| s)) {
        Ok(s) => s,
        Err(e) => return fail(status, e),
    };
    let child = match linux::spawn(&spec) {
        Ok(c) => c,
        Err(e) => return fail(status, e),
    };
    // Install forwarding before reporting "started": callers forward signals only after that.
    crate::signals::forward_to(
        child.pid,
        &[
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGHUP,
            libc::SIGQUIT,
            libc::SIGUSR1,
            libc::SIGUSR2,
        ],
    );
    report(status, &StatusMessage::Started { pid: child.pid });
    match linux::wait(child.pid) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("openpod: {e}");
            e.exit_code()
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(id: &str, dirs: &paths::Dirs, status: &mut Option<File>) -> i32 {
    if let Err(e) = state::read_config(dirs, id) {
        return fail(status, e);
    }
    fail(status, Error::unsupported_os())
}

fn fail(status: &mut Option<File>, error: Error) -> i32 {
    let code = error.exit_code();
    report(status, &StatusMessage::Failed { error });
    code
}

/// Write the status line and close the status descriptor.
fn report(status: &mut Option<File>, message: &StatusMessage) {
    if let Some(mut f) = status.take() {
        if let Ok(mut line) = serde_json::to_string(message) {
            line.push('\n');
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Mark every descriptor above stderr close-on-exec, so nothing the caller leaked
/// (descriptors without FD_CLOEXEC) reaches the container.
#[cfg(target_os = "linux")]
fn close_inherited_fds_on_exec() {
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    // SAFETY: close_range with CLOSE_RANGE_CLOEXEC only changes descriptor flags.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3 as libc::c_uint,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        )
    };
    if rc == 0 {
        return;
    }
    // Kernels before 5.11: walk /proc/self/fd instead.
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return;
    };
    let fds: Vec<i32> = entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    for fd in fds.into_iter().filter(|&fd| fd > 2) {
        // SAFETY: setting FD_CLOEXEC on a descriptor number has no memory effects.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

/// Take ownership of the status descriptor, if the caller passed one.
/// It is marked close-on-exec so the container never inherits it.
fn take_status_fd() -> Option<File> {
    // SAFETY: fcntl on any integer is safe; it fails with EBADF when fd 3 is not open.
    let open = unsafe { libc::fcntl(STATUS_FD, libc::F_GETFD) } != -1;
    if !open {
        return None;
    }
    // SAFETY: fd 3 is open and was given to this process for this purpose only.
    unsafe { libc::fcntl(STATUS_FD, libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: as above; nothing else in this process uses fd 3.
    Some(unsafe { File::from_raw_fd(STATUS_FD) })
}
