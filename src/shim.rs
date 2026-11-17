//! The `openpod __shim <id>` helper: it creates one container and watches it.

use crate::helper::StatusChannel;
use crate::{paths, state};

/// Run the shim for container `id`. Returns the process exit code.
pub fn shim_main(id: &str) -> i32 {
    let mut status = StatusChannel::take();
    #[cfg(target_os = "linux")]
    close_inherited_fds_on_exec();
    if let Err(e) = crate::id::check(id) {
        return status.fail(e);
    }
    let dirs = match paths::dirs() {
        Ok(d) => d,
        Err(e) => return status.fail(e),
    };
    let code = run(id, &dirs, &mut status);
    remove_container(&dirs, id);
    code
}

/// Delete the container directory. In rootless mode its writable layer can hold files
/// owned by subordinate IDs, which only root of a mapped user namespace can delete.
#[cfg(target_os = "linux")]
fn remove_container(dirs: &paths::Dirs, id: &str) {
    let dir = dirs.container_dir(id);
    let removed = crate::linux::userns::run_in_userns(|| {
        crate::store::remove_tree(&dir).map_err(|e| {
            crate::Error::io(
                format!("cannot remove {}", dir.display()),
                &e,
                "remove it by hand",
            )
        })
    });
    if let Err(e) = removed {
        eprintln!("openpod: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn remove_container(dirs: &paths::Dirs, id: &str) {
    let _ = state::remove_container_dir(dirs, id);
}

#[cfg(target_os = "linux")]
fn run(id: &str, dirs: &paths::Dirs, status: &mut StatusChannel) -> i32 {
    use crate::helper::StatusMessage;
    use crate::linux;
    let spec = match state::read_config(dirs, id).and_then(|s| s.validate().map(|()| s)) {
        Ok(s) => s,
        Err(e) => return status.fail(e),
    };
    let child = match linux::spawn(&spec) {
        Ok(c) => c,
        Err(e) => return status.fail(e),
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
    status.report(&StatusMessage::Started { pid: child.pid });
    match linux::wait(child.pid) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("openpod: {e}");
            e.exit_code()
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(id: &str, dirs: &paths::Dirs, status: &mut StatusChannel) -> i32 {
    if let Err(e) = state::read_config(dirs, id) {
        return status.fail(e);
    }
    status.fail(crate::Error::unsupported_os())
}

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
