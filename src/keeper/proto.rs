//! What clients and starters say to the keeper: one JSON line, with file descriptors
//! passed beside it (`SCM_RIGHTS`).

use serde::{Deserialize, Serialize};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// The longest message line: a start request carries the caller's environment.
pub const MAX_LINE: usize = 256 * 1024;
/// The most descriptors one message carries.
pub const MAX_FDS: usize = 8;

/// Linux marks received descriptors close-on-exec in the same step and never raises
/// SIGPIPE for a send; other systems (where only the tests run) get the flag afterwards.
#[cfg(target_os = "linux")]
const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const RECV_FLAGS: libc::c_int = 0;
#[cfg(target_os = "linux")]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const SEND_FLAGS: libc::c_int = 0;

/// A client's request, sent with stdin, stdout, stderr and the status pipe (in that order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Start {
        id: String,
        /// `attached` or `detached`.
        mode: String,
        keep_on_failure: bool,
        /// The caller's settings for podbag (`keeper::settings`): the starter runs with
        /// them, like a shim the caller would have started itself.
        env: Vec<(String, String)>,
        /// The build of the caller's helper program (`keeper::build_of`). A keeper of
        /// another build refuses: it would start the container with other code.
        #[serde(default)]
        build: String,
        /// The container wants a systemd scope of its own (rootless limits): a keeper
        /// that has no cgroup to make groups in refuses.
        #[serde(default)]
        scope: bool,
    },
}

/// The keeper's answers on the client's connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// The starter runs: `pid` takes the caller's signals.
    Started { pid: u32 },
    /// The starter ended with this exit code (or 128 + signal).
    Exited { code: i32 },
    /// The keeper cannot take the request; the caller starts a shim itself.
    Refused { why: String },
}

/// A starter gives a running detached container to the keeper (its parent). The
/// descriptors beside it, in order: the control listener, the shim lock, then the output
/// (stdout and stderr pipes, or the terminal's master and slave with `tty`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handover {
    pub id: String,
    pub pid: u32,
    pub tty: bool,
    /// The starter's directories: the keeper may serve clients with other settings.
    pub data: String,
    pub runtime: String,
    pub config: String,
}

/// Send one message: a 4-byte big-endian length, then `payload`, with `fds` attached to
/// the first byte.
pub fn send_with_fds(stream: &UnixStream, payload: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
    if payload.len() > MAX_LINE || fds.len() > MAX_FDS {
        return Err(std::io::Error::other("the message is too large"));
    }
    let mut message = Vec::with_capacity(4 + payload.len());
    message.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    message.extend_from_slice(payload);
    let mut iov = libc::iovec {
        iov_base: message.as_mut_ptr().cast(),
        iov_len: message.len(),
    };
    // Room for MAX_FDS descriptors, aligned like a cmsghdr.
    let mut control = [0u64; 8];
    // SAFETY: an all-zero msghdr is valid; the fields set below point to live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let bytes = std::mem::size_of_val(fds);
        // SAFETY: CMSG_* compute sizes and positions inside `control`, which holds
        // CMSG_SPACE(MAX_FDS descriptors); the copy writes `bytes` there.
        unsafe {
            let space = libc::CMSG_SPACE(bytes as u32) as usize;
            debug_assert!(space <= std::mem::size_of_val(&control));
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(cmsg), bytes);
        }
    }
    let sent = loop {
        // SAFETY: msg points to a valid iovec and control buffer for the call.
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, SEND_FLAGS) };
        if n >= 0 {
            break n as usize;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    // The descriptors went with the first bytes; the rest is plain data.
    let mut rest = message.get(sent..).unwrap_or_default();
    while !rest.is_empty() {
        // SAFETY: sending from a valid slice.
        let n = unsafe {
            libc::send(
                stream.as_raw_fd(),
                rest.as_ptr().cast(),
                rest.len(),
                SEND_FLAGS,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        rest = rest.get(n as usize..).unwrap_or_default();
    }
    Ok(())
}

/// One `recvmsg` into `buf`: how many bytes came (0: the peer closed). Descriptors that
/// came with them join `fds`, close-on-exec.
fn recv_once(
    stream: &UnixStream,
    buf: &mut [u8],
    flags: libc::c_int,
    fds: &mut Vec<OwnedFd>,
) -> std::io::Result<usize> {
    use std::os::fd::FromRawFd;
    loop {
        let mut control = [0u64; 8];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // SAFETY: an all-zero msghdr is valid; the fields point to live buffers.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        // SAFETY: msg points to valid buffers for the call.
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, flags) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        // SAFETY: the kernel filled `control` up to msg_controllen; CMSG_* walk only that.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let bytes =
                        ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                    let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                    for i in 0..bytes / std::mem::size_of::<RawFd>() {
                        // Fresh descriptors the kernel made for this process.
                        let fd = std::ptr::read_unaligned(data.add(i));
                        if RECV_FLAGS == 0 {
                            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                        }
                        fds.push(OwnedFd::from_raw_fd(fd));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(bad("more descriptors than a message may carry"));
        }
        return Ok(n as usize);
    }
}

fn bad(why: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_string())
}

fn cut_short() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "the connection ended before a message",
    )
}

/// Receive one message and the descriptors that came with it (at most `max_fds`; they
/// are close-on-exec). A message cut short, longer than [`MAX_LINE`], or with more
/// descriptors than allowed is an error (and its descriptors are closed).
pub fn recv_with_fds(
    stream: &UnixStream,
    max_fds: usize,
) -> std::io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut header = [0u8; 4];
    let mut fds: Vec<OwnedFd> = Vec::new();
    let mut got = 0usize;
    while got < header.len() {
        let rest = header.get_mut(got..).unwrap_or_default();
        match recv_once(stream, rest, RECV_FLAGS, &mut fds)? {
            0 => return Err(cut_short()),
            n => got += n,
        }
    }
    if fds.len() > max_fds {
        return Err(bad("more descriptors than expected"));
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_LINE {
        return Err(bad("the message is too long"));
    }
    let mut body = vec![0u8; len];
    let mut reader = stream;
    std::io::Read::read_exact(&mut reader, &mut body)?;
    Ok((body, fds))
}

/// A message that arrives piece by piece on a socket that is read without waiting: the
/// keeper's loop must never wait for a peer.
#[derive(Default)]
pub struct Incoming {
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
}

/// How far a message has come.
pub enum Progress {
    /// Not complete: read again when the socket is readable.
    More,
    /// The message and its descriptors.
    Done(Vec<u8>, Vec<OwnedFd>),
}

impl Incoming {
    /// Take what the socket holds now. Errors are those of [`recv_with_fds`].
    pub fn read(&mut self, stream: &UnixStream, max_fds: usize) -> std::io::Result<Progress> {
        loop {
            let want = match self.bytes.first_chunk::<4>() {
                None => 4usize.saturating_sub(self.bytes.len()),
                Some(header) => {
                    let len = u32::from_be_bytes(*header) as usize;
                    if len > MAX_LINE {
                        return Err(bad("the message is too long"));
                    }
                    (4 + len).saturating_sub(self.bytes.len())
                }
            };
            if want == 0 {
                let body = self.bytes.split_off(4);
                return Ok(Progress::Done(body, std::mem::take(&mut self.fds)));
            }
            let mut chunk = [0u8; 16 * 1024];
            let room = chunk.get_mut(..want.min(16 * 1024)).unwrap_or_default();
            match recv_once(stream, room, RECV_FLAGS | libc::MSG_DONTWAIT, &mut self.fds) {
                Ok(0) => return Err(cut_short()),
                Ok(n) => {
                    self.bytes
                        .extend_from_slice(chunk.get(..n).unwrap_or_default());
                    if self.fds.len() > max_fds {
                        return Err(bad("more descriptors than expected"));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(Progress::More);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    fn start() -> Request {
        Request::Start {
            id: "a".repeat(64),
            mode: "detached".into(),
            keep_on_failure: true,
            env: vec![("PATH".into(), "/bin".into())],
            build: "0.1.0:1:2:3:4.5".into(),
            scope: false,
        }
    }

    #[test]
    fn a_request_of_an_older_client_has_no_build() {
        let line = br#"{"op":"start","id":"a","mode":"detached","keep_on_failure":false,"env":[]}"#;
        let Request::Start { build, scope, .. } = serde_json::from_slice(line).unwrap();
        assert_eq!((build.as_str(), scope), ("", false));
    }

    #[test]
    fn a_message_in_pieces_is_put_together_without_waiting() {
        let (a, b) = UnixStream::pair().unwrap();
        let (_pipe_r, pipe_w) = std::io::pipe().unwrap();
        let line = serde_json::to_vec(&start()).unwrap();
        let mut message = (line.len() as u32).to_be_bytes().to_vec();
        message.extend_from_slice(&line);
        let mut incoming = Incoming::default();
        // Nothing yet: no wait.
        assert!(matches!(incoming.read(&b, 4).unwrap(), Progress::More));
        // The first three bytes carry the descriptor; the rest comes later.
        let (head, tail) = message.split_at(3);
        let mut iov = libc::iovec {
            iov_base: head.as_ptr().cast_mut().cast(),
            iov_len: head.len(),
        };
        let fd = pipe_w.as_raw_fd();
        let mut control = [0u64; 8];
        // SAFETY: a valid msghdr with one iovec and one SCM_RIGHTS descriptor.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&fd).cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                4,
            );
            assert_eq!(libc::sendmsg(a.as_raw_fd(), &msg, 0), 3);
        }
        assert!(matches!(incoming.read(&b, 4).unwrap(), Progress::More));
        (&a).write_all(tail).unwrap();
        let Progress::Done(body, fds) = incoming.read(&b, 4).unwrap() else {
            panic!("the message is complete");
        };
        assert_eq!(body, line);
        assert_eq!(fds.len(), 1);
        // A peer that closes in the middle, and a length beyond the limit, are errors.
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(&[0, 0]).unwrap();
        drop(a);
        let mut incoming = Incoming::default();
        assert!(incoming.read(&b, 4).is_err());
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(&(MAX_LINE as u32 + 1).to_be_bytes())
            .unwrap();
        assert!(Incoming::default().read(&b, 4).is_err());
    }

    #[test]
    fn a_request_travels_with_its_descriptors() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut pipe_r, pipe_w) = std::io::pipe().unwrap();
        let (_other_r, other_w) = std::io::pipe().unwrap();
        let line = serde_json::to_vec(&start()).unwrap();
        send_with_fds(&a, &line, &[pipe_w.as_raw_fd(), other_w.as_raw_fd()]).unwrap();
        drop(pipe_w);
        let (got, fds) = recv_with_fds(&b, MAX_FDS).unwrap();
        assert_eq!(serde_json::from_slice::<Request>(&got).unwrap(), start());
        assert_eq!(fds.len(), 2);
        // The first descriptor is the pipe's write end: what goes in comes out.
        let mut w = std::fs::File::from(fds.into_iter().next().unwrap());
        w.write_all(b"through").unwrap();
        drop(w);
        let mut text = String::new();
        pipe_r.read_to_string(&mut text).unwrap();
        assert_eq!(text, "through");
    }

    #[test]
    fn received_descriptors_are_close_on_exec() {
        let (a, b) = UnixStream::pair().unwrap();
        let (_r, w) = std::io::pipe().unwrap();
        send_with_fds(&a, b"{}", &[w.as_raw_fd()]).unwrap();
        let (_, fds) = recv_with_fds(&b, MAX_FDS).unwrap();
        // SAFETY: F_GETFD on a descriptor we own.
        let flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn replies_follow_on_the_same_connection() {
        let (a, b) = UnixStream::pair().unwrap();
        for reply in [Reply::Started { pid: 7 }, Reply::Exited { code: 3 }] {
            send_with_fds(&a, &serde_json::to_vec(&reply).unwrap(), &[]).unwrap();
        }
        let (first, fds) = recv_with_fds(&b, 0).unwrap();
        assert!(fds.is_empty());
        assert_eq!(
            serde_json::from_slice::<Reply>(&first).unwrap(),
            Reply::Started { pid: 7 }
        );
        let (second, _) = recv_with_fds(&b, 0).unwrap();
        assert_eq!(
            serde_json::from_slice::<Reply>(&second).unwrap(),
            Reply::Exited { code: 3 }
        );
    }

    #[test]
    fn too_many_descriptors_are_an_error() {
        let (a, b) = UnixStream::pair().unwrap();
        let (_r, w) = std::io::pipe().unwrap();
        send_with_fds(&a, b"{}", &[w.as_raw_fd(), w.as_raw_fd(), w.as_raw_fd()]).unwrap();
        assert!(recv_with_fds(&b, 2).is_err());
    }

    #[test]
    fn a_message_cut_short_is_an_error() {
        // The length says 100 bytes; 10 arrive, then the end.
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&100u32.to_be_bytes()).unwrap();
        a.write_all(b"0123456789").unwrap();
        drop(a);
        assert!(recv_with_fds(&b, MAX_FDS).is_err());
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[0, 0]).unwrap();
        drop(a);
        assert!(recv_with_fds(&b, MAX_FDS).is_err(), "half a length");
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        assert!(
            recv_with_fds(&b, MAX_FDS).is_err(),
            "an end without a message"
        );
    }

    #[test]
    fn a_message_longer_than_the_limit_is_refused() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&((MAX_LINE + 1) as u32).to_be_bytes()).unwrap();
        assert!(recv_with_fds(&b, MAX_FDS).is_err());
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(send_with_fds(&a, &vec![b'x'; MAX_LINE + 1], &[]).is_err());
    }

    #[test]
    fn a_long_message_arrives_whole() {
        let (a, b) = UnixStream::pair().unwrap();
        let big: Vec<u8> = (0..MAX_LINE).map(|i| (i % 251) as u8).collect();
        let expected = big.clone();
        let sender = std::thread::spawn(move || send_with_fds(&a, &big, &[]).unwrap());
        let (got, _) = recv_with_fds(&b, 0).unwrap();
        sender.join().unwrap();
        assert_eq!(got, expected);
    }
}
