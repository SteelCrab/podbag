//! The `openpod __shim <id>` helper: it creates one container and serves it until it
//! ends: output to the log (and to the caller when attached), the control socket, signals.

pub mod control;
pub mod logbook;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod proxy;
#[cfg(target_os = "linux")]
mod supervise;

use crate::helper::StatusChannel;
use crate::{paths, state};

/// Run the shim for container `id`. `mode` says whether it serves an attached caller
/// (`None`: what the container was created with). With `keep_on_failure` a failed start
/// keeps the container (`start`); otherwise it is removed (`run`). Returns the process
/// exit code.
pub fn shim_main(id: &str, mode: Option<crate::helper::ShimMode>, keep_on_failure: bool) -> i32 {
    let mut status = StatusChannel::take();
    #[cfg(target_os = "linux")]
    {
        close_inherited_fds_on_exec();
        // Held until the container exists and the handlers are in place.
        crate::signals::block(&supervise::HANDLED);
    }
    if let Err(e) = crate::id::check(id) {
        return status.fail(e);
    }
    let dirs = match paths::dirs() {
        Ok(d) => d,
        Err(e) => return status.fail(e),
    };
    let mut spec = match state::read_config(&dirs, id).and_then(|s| s.validate().map(|()| s)) {
        Ok(s) => s,
        Err(e) => return status.fail(e),
    };
    // Each start chooses: `run -d` and `start` detach, `run` and `start -a` attach.
    if let Some(mode) = mode {
        spec.detach = mode == crate::helper::ShimMode::Detached;
    }
    #[cfg(target_os = "linux")]
    {
        if spec.detach {
            if let Err(e) = supervise::detach() {
                return status.fail(e);
            }
        }
        // Readers take this lock to learn that the shim is gone (state::reconcile).
        let _lock = match state::hold_shim_lock(&dirs, id) {
            Ok(lock) => lock,
            // Another shim serves it: leave the container alone.
            Err(e) => return status.fail(e),
        };
        let outcome = supervise::run(&spec, &dirs, &mut status);
        // A failed `run` leaves no trace, a failed `start` keeps the container; a finished
        // container stays unless --rm. With --rm it goes even when its start failed, like
        // Docker (whose CLI waits for that removal).
        if spec.auto_remove || (!outcome.started && !keep_on_failure) {
            remove_container(&dirs, &spec);
        }
        outcome.code
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (spec, keep_on_failure);
        let code = status.fail(crate::Error::unsupported_os());
        let _ = state::remove_container_dir(&dirs, id);
        code
    }
}

/// Delete the container directory and its anonymous volumes. In rootless mode its
/// writable layer can hold files owned by subordinate IDs, which only root of a mapped user
/// namespace can delete.
#[cfg(target_os = "linux")]
pub fn remove_container(dirs: &paths::Dirs, spec: &crate::spec::ContainerSpec) {
    let anonymous = crate::volume::anonymous_volumes(&spec.mounts);
    if let Err(e) = crate::volume::remove_anonymous(dirs, &spec.id, anonymous) {
        eprintln!("openpod: {e}");
    }
    let dir = dirs.container_dir(&spec.id);
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

#[cfg(target_os = "linux")]
pub(crate) fn close_inherited_fds_on_exec() {
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
