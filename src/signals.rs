//! Signal handling for the CLI and the shim. The library API never calls these.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// A signal by name (`TERM`, `SIGTERM`) or number. Names give Linux numbers on every
/// host: the container always runs on Linux (on macOS, inside a VM).
pub fn parse(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return (n > 0 && n < 65).then_some(n);
    }
    let name = s.to_ascii_uppercase();
    let name = name.strip_prefix("SIG").unwrap_or(&name);
    Some(match name {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        "TERM" => 15,
        "CONT" => 18,
        "STOP" => 19,
        "WINCH" => 28,
        _ => return None,
    })
}

static TARGET: AtomicI32 = AtomicI32::new(0);
static PIPE: AtomicI32 = AtomicI32::new(-1);
/// Forward signals the kernel generated too (see `forward`).
static ALL: AtomicBool = AtomicBool::new(false);

/// Forward a signal to TARGET.
///
/// Containers and exec'd commands run in their own session, outside the caller's terminal.
/// The shim and the exec helper stay in the caller's process group, so they receive the
/// terminal's signals (Ctrl+C: SIGINT with `si_code` SI_KERNEL) and pass every signal on
/// (`forward_all_to`, `notify_pipe`). The CLI passes on only signals another process sent
/// (`si_code <= 0`: kill, sigqueue, tgkill): the terminal's copy already reached its helper.
extern "C" fn forward(sig: libc::c_int, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo_t to an SA_SIGINFO handler.
    let from_process = info.is_null() || unsafe { (*info).si_code } <= 0;
    let pid = TARGET.load(Ordering::Relaxed);
    if (from_process || ALL.load(Ordering::Relaxed)) && pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(pid, sig) };
    }
}

/// Write the signal's number to the notify pipe.
extern "C" fn notify(sig: libc::c_int, _info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    let fd = PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = sig as u8;
        // SAFETY: write is async-signal-safe; one byte from a valid buffer.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

/// Report these signals as bytes on `pipe_write` (an event loop reads the other end),
/// then deliver any that were held.
pub fn notify_pipe(pipe_write: i32, signals: &[i32]) {
    PIPE.store(pipe_write, Ordering::Relaxed);
    let handler = notify as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void);
    for &sig in signals {
        set_action(sig, handler as libc::sighandler_t, libc::SA_SIGINFO);
    }
    set_mask(libc::SIG_UNBLOCK, signals);
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

/// Give these signals their default action again, then deliver any that were held.
pub fn restore_default(signals: &[i32]) {
    for &sig in signals {
        set_action(sig, libc::SIG_DFL, 0);
    }
    set_mask(libc::SIG_UNBLOCK, signals);
}

/// Send each of these signals to `pid` when another process sends it to this process
/// (see `forward`), then deliver any that were held.
pub fn forward_to(pid: u32, signals: &[i32]) {
    ALL.store(false, Ordering::Relaxed);
    install_forward(pid, signals);
}

/// Like `forward_to`, for signals from the terminal too.
pub fn forward_all_to(pid: u32, signals: &[i32]) {
    ALL.store(true, Ordering::Relaxed);
    install_forward(pid, signals);
}

fn install_forward(pid: u32, signals: &[i32]) {
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
