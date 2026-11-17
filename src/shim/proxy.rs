//! Published ports (`-p`): the shim listens on the host and relays each connection (TCP)
//! or each client's datagrams (UDP) to the container. Rootless, the container is only
//! reachable from the network session, so the connection to it is made from a thread
//! that joined the session's network namespace.

use crate::network::{PortMapping, Proto};
use crate::{Error, ErrorKind, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a UDP client may stay quiet before its relay ends.
const UDP_IDLE: Duration = Duration::from_secs(60);

/// A bound host socket, and the mapping with the port the kernel gave it.
pub(super) enum Listener {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

/// Bind every published port on the host. A port that cannot be bound fails the start.
pub(super) fn open(mappings: &[PortMapping]) -> Result<Vec<(PortMapping, Listener)>> {
    let mut out = Vec::with_capacity(mappings.len());
    for m in mappings {
        let addr = SocketAddrV4::new(m.host_ip, m.host_port);
        let bound = match m.proto {
            Proto::Tcp => TcpListener::bind(addr).and_then(|l| {
                let port = l.local_addr()?.port();
                Ok((port, Listener::Tcp(l)))
            }),
            Proto::Udp => UdpSocket::bind(addr).and_then(|s| {
                let port = s.local_addr()?.port();
                Ok((port, Listener::Udp(s)))
            }),
        };
        match bound {
            Ok((port, listener)) => out.push((
                PortMapping {
                    host_port: port,
                    ..*m
                },
                listener,
            )),
            Err(e) => {
                let next = if e.kind() == std::io::ErrorKind::PermissionDenied {
                    "use a host port of 1024 or more, or lower the sysctl net.ipv4.ip_unprivileged_port_start"
                } else {
                    "choose another host port"
                };
                return Err(Error::new(
                    ErrorKind::Runtime,
                    format!("cannot publish port {}", m.host_port),
                    format!("{addr}: {e}"),
                    next,
                ));
            }
        }
    }
    Ok(out)
}

/// Make a connection to the container: directly (root), or from the session's network
/// namespace (rootless).
fn dial_tcp(target: SocketAddrV4, session: Option<u32>) -> std::io::Result<TcpStream> {
    let connect = || TcpStream::connect_timeout(&SocketAddr::V4(target), Duration::from_secs(5));
    match session {
        None => connect(),
        Some(pid) => crate::linux::net::in_netns(pid, connect),
    }
}

fn dial_udp(target: SocketAddrV4, session: Option<u32>) -> std::io::Result<UdpSocket> {
    let connect = || {
        let s = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
        s.connect(target)?;
        Ok(s)
    };
    match session {
        None => connect(),
        Some(pid) => crate::linux::net::in_netns(pid, connect),
    }
}

/// Copy one direction until it ends, then end the other side's writing.
fn copy(mut from: TcpStream, mut to: TcpStream) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

fn relay_tcp(
    listener: TcpListener,
    target: SocketAddrV4,
    session: Option<u32>,
    stop: Arc<AtomicBool>,
) {
    loop {
        let client = match listener.accept() {
            Ok((client, _)) => client,
            Err(_) if stop.load(Ordering::SeqCst) => return,
            // Out of descriptors and the like: wait instead of spinning.
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        std::thread::spawn(move || {
            let Ok(upstream) = dial_tcp(target, session) else {
                return;
            };
            let (Ok(client2), Ok(upstream2)) = (client.try_clone(), upstream.try_clone()) else {
                return;
            };
            let back = std::thread::spawn(move || copy(upstream2, client2));
            copy(client, upstream);
            let _ = back.join();
        });
    }
}

fn relay_udp(host: UdpSocket, target: SocketAddrV4, session: Option<u32>, stop: Arc<AtomicBool>) {
    let host = Arc::new(host);
    let peers: Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>> = Arc::default();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let (n, client) = match host.recv_from(&mut buf) {
            Ok(got) => got,
            Err(_) if stop.load(Ordering::SeqCst) => return,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let known = peers.lock().ok().and_then(|p| p.get(&client).cloned());
        let upstream = match known {
            Some(u) => u,
            None => {
                let Ok(u) = dial_udp(target, session) else {
                    continue;
                };
                let u = Arc::new(u);
                if let Ok(mut p) = peers.lock() {
                    p.insert(client, u.clone());
                }
                let (host, peers, reply) = (host.clone(), peers.clone(), u.clone());
                std::thread::spawn(move || {
                    let _ = reply.set_read_timeout(Some(UDP_IDLE));
                    let mut buf = vec![0u8; 64 * 1024];
                    while let Ok(n) = reply.recv(&mut buf) {
                        let _ = host.send_to(&buf[..n], client);
                    }
                    if let Ok(mut p) = peers.lock() {
                        p.remove(&client);
                    }
                });
                u
            }
        };
        let _ = upstream.send(&buf[..n]);
    }
}

/// The running relays of a container's published ports.
pub(super) struct Published {
    stop: Arc<AtomicBool>,
    sockets: Vec<RawFd>,
    udp: Vec<std::thread::JoinHandle<()>>,
}

impl Published {
    /// Stop accepting: the container has ended, and its address may soon belong to
    /// another one. Returns once no listener answers any more.
    pub(super) fn close(self) {
        self.stop.store(true, Ordering::SeqCst);
        for fd in &self.sockets {
            // SAFETY: shutting down a socket the relay threads still own; it only ends
            // their waiting (accept/recv fail and they see `stop`).
            unsafe { libc::shutdown(*fd, libc::SHUT_RDWR) };
        }
        for t in self.udp {
            let _ = t.join();
        }
    }
}

/// Relay every published port to `container` until `Published::close`.
pub(super) fn serve(
    listeners: Vec<(PortMapping, Listener)>,
    container: Ipv4Addr,
    session: Option<u32>,
) -> Published {
    let stop = Arc::new(AtomicBool::new(false));
    let mut published = Published {
        stop: stop.clone(),
        sockets: Vec::new(),
        udp: Vec::new(),
    };
    for (mapping, listener) in listeners {
        let target = SocketAddrV4::new(container, mapping.container_port);
        let stop = stop.clone();
        match listener {
            Listener::Tcp(l) => {
                published.sockets.push(l.as_raw_fd());
                std::thread::spawn(move || relay_tcp(l, target, session, stop));
            }
            Listener::Udp(s) => {
                published.sockets.push(s.as_raw_fd());
                // A shut-down UDP socket may not wake its reader: a timeout does.
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                published.udp.push(std::thread::spawn(move || {
                    relay_udp(s, target, session, stop)
                }));
            }
        }
    }
    published
}
