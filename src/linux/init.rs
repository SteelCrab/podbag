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
) -> ! {
    let mut sync = File::from(sync_read);
    let mut byte = [0u8; 1];
    if !matches!(sync.read(&mut byte), Ok(1)) {
        // The parent gave up (for example, ID mapping failed).
        // SAFETY: _exit ends the process without running destructors.
        unsafe { libc::_exit(1) };
    }
    drop(sync);

    let error = match setup_and_exec(spec, rootless) {
        Err(e) => e,
        Ok(never) => match never {},
    };
    let mut status = File::from(status_write);
    let _ = status.write_all(&serde_json::to_vec(&error).unwrap_or_default());
    // SAFETY: as above.
    unsafe { libc::_exit(error.exit_code()) }
}

fn setup_and_exec(spec: &ContainerSpec, rootless: bool) -> Result<Infallible> {
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

    rustix::system::sethostname(spec.hostname.as_bytes()).map_err(|e| {
        setup_error(
            rootless,
            "cannot set the container hostname",
            e.into(),
            "use a hostname of at most 64 characters",
        )
    })?;
    enter_rootfs(Path::new(&spec.rootfs), rootless)?;

    let workdir = spec.workdir.as_deref().unwrap_or("/");
    std::fs::create_dir_all(workdir)
        .and_then(|_| std::env::set_current_dir(workdir))
        .map_err(|e| {
            os_error(
                &format!("cannot enter the working directory {workdir}"),
                e,
                "choose a working directory the container can create, such as /app",
            )
        })?;

    let env = spec.effective_env();
    let program = resolve(&spec.command[0], &env)?;
    reset_signals();
    exec(&program, &spec.command, &env)
}

/// Start the command like Docker does: no blocked signals, every signal at its default action.
/// Ignored signals survive execve, and this process inherited openpod's (Rust ignores SIGPIPE).
fn reset_signals() {
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
fn become_root() -> std::io::Result<()> {
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

/// A failed setup step, explained as an AppArmor restriction when that is the cause.
fn setup_error(rootless: bool, what: &str, e: std::io::Error, next: &str) -> Error {
    super::userns_denial(rootless, Some(&e), None).unwrap_or_else(|| os_error(what, e, next))
}

/// Find the program: a path with a slash is used as is; a bare name is searched in PATH.
fn resolve(name: &str, env: &[(String, String)]) -> Result<PathBuf> {
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

fn exec(program: &Path, argv: &[String], env: &[(String, String)]) -> Result<Infallible> {
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
    if e.raw_os_error() == Some(libc::ENOENT) {
        return Err(not_found(&argv[0], None));
    }
    Err(Error::new(
        ErrorKind::CommandNotExecutable,
        format!("cannot execute {}", argv[0]),
        e.to_string(),
        "check that the file is a program for this CPU and has execute permission",
    ))
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

fn os_error(what: &str, e: std::io::Error, next: &str) -> Error {
    Error::new(ErrorKind::Runtime, what, e.to_string(), next)
}
