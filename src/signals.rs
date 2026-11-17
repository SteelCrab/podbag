//! Signal handling for the CLI and the shim. The library API never calls these.

use std::sync::atomic::{AtomicI32, Ordering};

static TARGET: AtomicI32 = AtomicI32::new(0);

/// Forward a signal that another process sent (`si_code <= 0`: kill, sigqueue, tgkill).
/// Signals the kernel generated, such as Ctrl+C from a terminal (`SI_KERNEL`), are not
/// forwarded: the terminal sends them to the whole foreground process group, which
/// already includes the container, and a second copy can turn a graceful stop into a
/// forced one.
extern "C" fn forward(sig: libc::c_int, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo_t to an SA_SIGINFO handler.
    let from_process = info.is_null() || unsafe { (*info).si_code } <= 0;
    let pid = TARGET.load(Ordering::Relaxed);
    if from_process && pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(pid, sig) };
    }
}

/// Hold these signals until `ignore` or `forward_to` handles them.
/// Child processes started with `std::process::Command` get an empty mask.
pub fn block(signals: &[i32]) {
    set_mask(libc::SIG_BLOCK, signals);
}

/// Ignore these signals, then deliver any that were held.
pub fn ignore(signals: &[i32]) {
    for &sig in signals {
        set_action(sig, libc::SIG_IGN, 0);
    }
    set_mask(libc::SIG_UNBLOCK, signals);
}

/// Send each of these signals to `pid` when another process sends it to this process
/// (see `forward`), then deliver any that were held.
pub fn forward_to(pid: u32, signals: &[i32]) {
    TARGET.store(pid as i32, Ordering::Relaxed);
    let handler = forward as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void);
    for &sig in signals {
        set_action(sig, handler as libc::sighandler_t, libc::SA_SIGINFO);
    }
    set_mask(libc::SIG_UNBLOCK, signals);
}

fn set_action(sig: i32, handler: libc::sighandler_t, flags: libc::c_int) {
    // SAFETY: sa is fully initialized before use; the handler only calls kill.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        sa.sa_flags = libc::SA_RESTART | flags;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

fn set_mask(how: libc::c_int, signals: &[i32]) {
    // SAFETY: set is initialized by sigemptyset before use.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &sig in signals {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(how, &set, std::ptr::null_mut());
    }
}
