//! A netlink socket that sends one request at a time and reads the kernel's answer.

use super::{Message, NLMSG_DONE, NLMSG_ERROR};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub struct Socket {
    fd: OwnedFd,
    seq: u32,
}

impl Socket {
    /// `protocol`: `libc::NETLINK_ROUTE` or `libc::NETLINK_NETFILTER`. The socket belongs
    /// to the calling thread's network namespace.
    pub fn open(protocol: i32) -> io::Result<Socket> {
        // SAFETY: plain socket creation; the result is checked.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                protocol,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sockaddr_nl is plain data; zero means "kernel picks the port ID".
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        // SAFETY: a valid address of the stated size.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Socket { fd, seq: 1 })
    }

    pub fn route() -> io::Result<Socket> {
        Socket::open(libc::NETLINK_ROUTE)
    }

    /// Send `bytes` (one or more finished messages) and collect the answers until the
    /// kernel acknowledges message `last_seq` or ends a dump. An error answer is returned
    /// as that errno. Returns the payloads of the other answers.
    fn exchange(&mut self, bytes: &[u8], last_seq: u32) -> io::Result<Vec<Vec<u8>>> {
        self.exchange_until(bytes, last_seq, false)
    }

    /// As `exchange`; with `whole_dump`, only the end of a dump ends the reading (a
    /// listing must be read to its end, or its last message meets the next request).
    fn exchange_until(
        &mut self,
        bytes: &[u8],
        last_seq: u32,
        whole_dump: bool,
    ) -> io::Result<Vec<Vec<u8>>> {
        // SAFETY: sending from a valid buffer.
        let sent =
            unsafe { libc::send(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut answers = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            // SAFETY: receiving into a valid buffer.
            let got =
                unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if got < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            let mut at = 0usize;
            let got = got as usize;
            while at + 16 <= got {
                let len = u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
                let ty = u16::from_ne_bytes(buf[at + 4..at + 6].try_into().unwrap());
                let seq = u32::from_ne_bytes(buf[at + 8..at + 12].try_into().unwrap());
                if len < 16 || at + len > got {
                    return Err(io::Error::other("malformed netlink answer"));
                }
                let body = &buf[at + 16..at + len];
                match ty {
                    NLMSG_ERROR => {
                        let errno = i32::from_ne_bytes(body[0..4].try_into().unwrap());
                        if errno != 0 {
                            return Err(io::Error::from_raw_os_error(-errno));
                        }
                        if seq == last_seq {
                            return Ok(answers);
                        }
                    }
                    NLMSG_DONE => return Ok(answers),
                    _ => answers.push(body.to_vec()),
                }
                at += (len + 3) & !3;
            }
            if !answers.is_empty() && last_seq == 0 && !whole_dump {
                return Ok(answers);
            }
        }
    }

    /// Send one request that asks for an acknowledgement.
    pub fn request(&mut self, msg: Message) -> io::Result<()> {
        let seq = self.next_seq();
        self.exchange(&msg.finish(seq), seq).map(drop)
    }

    /// Send pre-built bytes (a batch) whose last message has sequence `last_seq`.
    pub fn request_raw(&mut self, bytes: &[u8], last_seq: u32) -> io::Result<Vec<Vec<u8>>> {
        self.exchange(bytes, last_seq)
    }

    /// Send one listing request (`NLM_F_DUMP`) and return every entry of the answer.
    pub fn dump(&mut self, msg: Message) -> io::Result<Vec<Vec<u8>>> {
        let seq = self.next_seq();
        self.exchange_until(&msg.finish(seq), 0, true)
    }

    pub fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    /// The index of link `name`, or `None` if there is no such link.
    pub fn link_index(&mut self, name: &str) -> io::Result<Option<u32>> {
        let seq = self.next_seq();
        match self.exchange(&super::route::get_link(name).finish(seq), 0) {
            Ok(answers) => Ok(answers
                .first()
                .filter(|a| a.len() >= 8)
                .map(|a| i32::from_ne_bytes(a[4..8].try_into().unwrap()) as u32)),
            Err(e) if e.raw_os_error() == Some(libc::ENODEV) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
