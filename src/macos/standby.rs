//! `podbag __vm-standby`: what stays of the VM host while its VM is off. The host starts
//! it with the lock and the two listening sockets (inherited) and ends; the standby waits:
//! when one of the sockets has a connection, it becomes the host (exec `__vm-host`), which
//! starts a VM and serves the waiting connection. So `docker.sock` answers while the VM is
//! off, and the standby holds no VM. SIGTERM, SIGINT or SIGHUP (`podbag vm stop`, a
//! logout) end it.

use super::host::{Inherited, paths, remove_files, write_whole};

pub fn standby_main() -> i32 {
    let Some(fds) = Inherited::from_env() else {
        eprintln!("podbag: __vm-standby is started by the VM host");
        return 2;
    };
    let p = match paths() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("podbag: {e}");
            return 1;
        }
    };
    // `podbag vm stop` signals the process in the pid file, and a standby whatever state it
    // finds: the state first, so that this pid never shows with the host's old state.
    write_whole(&p.state, "standby");
    write_whole(&p.pid, &std::process::id().to_string());
    // Signals as bytes on a pipe, watched with the sockets.
    let mut pipe = [0; 2];
    // SAFETY: pipe fills two descriptors; both are made close-on-exec.
    unsafe {
        if libc::pipe(pipe.as_mut_ptr()) != 0 {
            eprintln!("podbag: {}", std::io::Error::last_os_error());
            return 1;
        }
        for fd in pipe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    // SAFETY: getppid has no arguments.
    let parent = unsafe { libc::getppid() };
    let signals = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];
    crate::signals::notify_pipe(pipe[1], &signals);
    loop {
        let mut watch = [fds.docker, fds.vm, pipe[0]].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: three pollfds on the stack.
        if unsafe { libc::poll(watch.as_mut_ptr(), 3, -1) } < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("podbag: {}", std::io::Error::last_os_error());
            remove_files(&p);
            return 1;
        }
        if watch[2].revents != 0 {
            remove_files(&p);
            return 0;
        }
        if watch[0].revents != 0 || watch[1].revents != 0 {
            // Not while the host that started it still ends: an exec next to an ending
            // parent has hung in macOS's kernel (2 s at most). A host that had ended
            // before this process looked left it to launchd (pid 1): nothing to wait for.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            // SAFETY: getppid has no arguments.
            while parent != 1
                && unsafe { libc::getppid() } == parent
                && std::time::Instant::now() < deadline
            {
                // A stop asked meanwhile ends the standby: the exec would lose it.
                let mut stop = libc::pollfd {
                    fd: pipe[0],
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one pollfd on the stack.
                if unsafe { libc::poll(&mut stop, 1, 10) } > 0 {
                    remove_files(&p);
                    return 0;
                }
            }
            // Held across exec (a child keeps the mask) for the host's own signal thread.
            crate::signals::block(&signals);
            // One that came just before the block is in the pipe, which the exec closes.
            let mut stop = libc::pollfd {
                fd: pipe[0],
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd on the stack.
            if unsafe { libc::poll(&mut stop, 1, 0) } > 0 {
                remove_files(&p);
                return 0;
            }
            let e = fds.exec("__vm-host");
            eprintln!("podbag: cannot start the VM host again: {e}");
            remove_files(&p);
            return 1;
        }
    }
}
