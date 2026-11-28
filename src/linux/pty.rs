//! Pseudo-terminals for `-t`, and raw mode for the caller's terminal.

use crate::{Error, Result};
use std::os::fd::{FromRawFd, OwnedFd};

/// Open a terminal pair (controlling side, container side), both close-on-exec.
/// When `size_from` is a terminal, the new terminal gets its size.
pub fn open(size_from: Option<i32>) -> Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty writes two descriptors; the name and settings arguments may be null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(Error::io(
            "cannot create a terminal for the container",
            &std::io::Error::last_os_error(),
            "check that /dev/pts is mounted",
        ));
    }
    // SAFETY: two fresh descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // SAFETY: setting a flag on our own descriptor.
        unsafe {
            libc::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(fd),
                libc::F_SETFD,
                libc::FD_CLOEXEC,
            )
        };
    }
    if let Some(from) = size_from {
        copy_size(from, std::os::fd::AsRawFd::as_raw_fd(&master));
    }
    Ok((master, slave))
}

/// Send descriptor `fd` over the unix socket `sock` (SCM_RIGHTS).
pub fn send_fd(sock: i32, fd: i32) -> std::io::Result<()> {
    // SAFETY: a msghdr with one data byte and a control buffer sized for one descriptor.
    unsafe {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let space = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as usize;
        let mut control = vec![0u8; space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Receive one descriptor sent with [`send_fd`]. It is made close-on-exec.
pub fn recv_fd(sock: i32) -> std::io::Result<OwnedFd> {
    // SAFETY: as in send_fd; the received descriptor is new and owned by us.
    unsafe {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let space = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as usize;
        let mut control = vec![0u8; space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        if libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) <= 0 {
            return Err(std::io::Error::other(
                "the container did not send its terminal",
            ));
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(std::io::Error::other(
                "the container did not send its terminal",
            ));
        }
        Ok(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
            libc::CMSG_DATA(cmsg).cast::<i32>(),
        )))
    }
}

/// Copy the window size of terminal `from` to terminal `to`. Does nothing if `from` is not a terminal.
pub fn copy_size(from: i32, to: i32) {
    // SAFETY: winsize is plain data; ioctl fails harmlessly on a non-terminal.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(from, libc::TIOCGWINSZ, &mut size) == 0 {
            libc::ioctl(to, libc::TIOCSWINSZ, &size);
        }
    }
}

/// Set the window size of terminal `fd`.
pub fn set_size(fd: i32, rows: u16, cols: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: a winsize value; ioctl fails harmlessly on a non-terminal.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
}

/// Read resize records (rows, cols: two little-endian u16) from `from` and apply the
/// last whole one to terminal `to`. `pending` keeps a partial record. False at the end.
pub fn apply_resizes(from: i32, to: i32, pending: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 64];
    // SAFETY: reading into a valid buffer.
    let n = unsafe { libc::read(from, buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return false;
    }
    pending.extend_from_slice(&buf[..n as usize]);
    let whole = pending.len() / 4 * 4;
    if whole >= 4 {
        let r = &pending[whole - 4..whole];
        set_size(
            to,
            u16::from_le_bytes([r[0], r[1]]),
            u16::from_le_bytes([r[2], r[3]]),
        );
        pending.drain(..whole);
    }
    true
}

/// Copy terminal output to stdout (and stdin to the terminal when `stdin`) until the
/// terminal closes. SIGWINCH resizes it from stdin's terminal; `resize` (a pipe of resize
/// records, see [`apply_resizes`]) resizes it when the caller is not a terminal. Used by
/// `exec -t`.
pub fn relay(master: &OwnedFd, stdin: bool, _child: u32, resize: Option<i32>) {
    use std::os::fd::AsRawFd;
    let m = master.as_raw_fd();
    let Ok((sig_r, sig_w)) = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    ) else {
        return;
    };
    crate::signals::notify_pipe(sig_w.as_raw_fd(), &[libc::SIGWINCH]);
    let mut relay_stdin = stdin;
    let mut resize = resize;
    let mut pending = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut fds = vec![
            libc::pollfd {
                fd: m,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sig_r.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let resize_at = resize.map(|fd| {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let stdin_at = relay_stdin.then(|| {
            fds.push(libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        // SAFETY: a valid pollfd array.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
            continue;
        }
        if fds[1].revents != 0 {
            let mut b = [0u8; 16];
            // SAFETY: reading into a valid buffer from our non-blocking pipe.
            unsafe { libc::read(sig_r.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
            copy_size(0, m);
        }
        if fds[0].revents != 0 {
            // SAFETY: reading into a valid buffer from the terminal.
            let n = unsafe { libc::read(m, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return; // the other side closed (EIO)
            }
            // SAFETY: writing bytes we just read to our stdout.
            unsafe { libc::write(1, buf.as_ptr().cast(), n as usize) };
        }
        if let (Some(i), Some(fd)) = (resize_at, resize) {
            if fds[i].revents != 0 && !apply_resizes(fd, m, &mut pending) {
                resize = None;
            }
        }
        if stdin_at.is_some_and(|i| fds[i].revents != 0) {
            // SAFETY: reading into a valid buffer from our stdin.
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                relay_stdin = false;
            } else {
                // SAFETY: writing bytes we just read to the terminal.
                unsafe { libc::write(m, buf.as_ptr().cast(), n as usize) };
            }
        }
    }
}

/// The caller's terminal in raw mode; the old settings come back on drop.
pub struct RawMode {
    fd: i32,
    saved: libc::termios,
}

impl RawMode {
    /// Raw mode on `fd`, or None when `fd` is not a terminal.
    pub fn enable(fd: i32) -> Option<RawMode> {
        // SAFETY: termios is plain data; the calls fail harmlessly on a non-terminal.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
            Some(RawMode { fd, saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring settings we read earlier.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn tty_master_only_hangs_up_after_the_last_slave_closes() {
        let (master, slave) = open(None).unwrap();
        let keep_open = slave.try_clone().unwrap();
        drop(slave);
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a valid single pollfd and a nonblocking timeout.
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 0);
        drop(keep_open);
        // SAFETY: the same pollfd still points to an open master.
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
        assert_ne!(poll.revents & libc::POLLHUP, 0);
    }
}
