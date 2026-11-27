//! The opt-in DNS listener is private to one container's network namespace. The
//! shim owns its lifetime, and the network store answers from that container's bridges.

use crate::paths::Dirs;
use crate::{Error, Result, linux, network};
use hickory_proto::op::{Edns, Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{DNSClass, RData, Record, RecordType};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) const LISTENER: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 11);
const MAX_PACKET: usize = u16::MAX as usize;
const MAX_UDP: u16 = 4096;
const WORKERS: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Resolver {
    wake: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl Resolver {
    pub(super) fn start(
        dirs: &Dirs,
        id: &str,
        pid: u32,
        upstream: Vec<SocketAddr>,
    ) -> Result<Self> {
        let start = || -> io::Result<Self> {
            let ns = linux::net::namespace(pid)?;
            let (wake, stop) = UnixStream::pair()?;
            let (ready, started) = mpsc::sync_channel(1);
            let state = Server {
                dirs: dirs.clone(),
                caller: id.to_string(),
                upstream,
                stopping: AtomicBool::new(false),
            };
            let thread = thread::Builder::new()
                .name("podbag-dns".into())
                .spawn(move || {
                    let open = || {
                        linux::net::join_netns(&ns)?;
                        let addr = SocketAddr::from((LISTENER, 53));
                        let udp = UdpSocket::bind(addr)?;
                        let tcp = TcpListener::bind(addr)?;
                        loopback_only(&udp)?;
                        loopback_only(&tcp)?;
                        udp.set_nonblocking(true)?;
                        tcp.set_nonblocking(true)?;
                        Ok::<_, io::Error>((Arc::new(udp), tcp))
                    };
                    match open() {
                        Ok((udp, tcp)) => {
                            drop(ns);
                            state.serve(&stop, udp, tcp, ready);
                        }
                        Err(error) => {
                            let _ = ready.send(Err(error));
                        }
                    }
                })?;
            let resolver = Self {
                wake,
                thread: Some(thread),
            };
            started
                .recv()
                .map_err(|_| io::Error::other("the DNS listener exited before it was ready"))??;
            Ok(resolver)
        };
        start().map_err(|error| {
            Error::io(
                "cannot start the embedded DNS listener",
                &error,
                "check that 127.0.0.11 port 53 is available in the container network namespace",
            )
        })
    }
}

impl Drop for Resolver {
    fn drop(&mut self) {
        let _ = self.wake.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn loopback_only(socket: &impl AsRawFd) -> io::Result<()> {
    let interface = c"lo";
    // SAFETY: an open socket, with a NUL-terminated interface name and its length.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            interface.as_ptr().cast(),
            interface.to_bytes_with_nul().len() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

enum Request {
    Datagram(Arc<UdpSocket>, SocketAddr, Vec<u8>),
    Connection(TcpStream),
}

#[derive(Clone, Copy)]
enum Transport {
    Udp,
    Tcp,
}

struct Server {
    dirs: Dirs,
    caller: String,
    upstream: Vec<SocketAddr>,
    stopping: AtomicBool,
}

impl Server {
    fn serve(
        &self,
        stop: &UnixStream,
        udp: Arc<UdpSocket>,
        tcp: TcpListener,
        ready: mpsc::SyncSender<io::Result<()>>,
    ) {
        thread::scope(|scope| {
            let (queue, requests) = mpsc::sync_channel(WORKERS);
            let requests = Arc::new(Mutex::new(requests));
            for _ in 0..WORKERS {
                let requests = Arc::clone(&requests);
                let worker = thread::Builder::new().spawn_scoped(scope, move || {
                    loop {
                        let next = requests.lock().unwrap().recv();
                        let Ok(request) = next else { break };
                        if self.stopping.load(Ordering::Relaxed) {
                            break;
                        }
                        self.handle(request);
                    }
                });
                if let Err(error) = worker {
                    let _ = ready.send(Err(error));
                    return;
                }
            }
            if ready.send(Ok(())).is_err() {
                return;
            }

            let pollable = |fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let mut sockets = [
                pollable(stop.as_raw_fd()),
                pollable(udp.as_raw_fd()),
                pollable(tcp.as_raw_fd()),
            ];
            let mut buf = vec![0; MAX_PACKET];
            loop {
                // SAFETY: the descriptors outlive this call, and sockets is writable.
                let count = unsafe { libc::poll(sockets.as_mut_ptr(), sockets.len() as _, -1) };
                if count < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if sockets[0].revents != 0 {
                    break;
                }
                if sockets[1].revents & libc::POLLIN != 0
                    && let Ok((size, from)) = udp.recv_from(&mut buf)
                {
                    let request = Request::Datagram(Arc::clone(&udp), from, buf[..size].to_vec());
                    let _ = queue.try_send(request);
                }
                if sockets[2].revents & libc::POLLIN != 0
                    && let Ok((client, _)) = tcp.accept()
                {
                    let _ = queue.try_send(Request::Connection(client));
                }
            }
            self.stopping.store(true, Ordering::Relaxed);
            drop(queue);
        });
    }

    fn handle(&self, request: Request) {
        match request {
            Request::Datagram(socket, from, wire) => {
                if let Some(answer) = self.answer(&wire, Transport::Udp) {
                    let _ = socket.send_to(&answer, from);
                }
            }
            Request::Connection(mut client) => {
                while !self.stopping.load(Ordering::Relaxed) {
                    let Ok(wire) = read_frame(&mut client) else {
                        break;
                    };
                    let Some(answer) = self.answer(&wire, Transport::Tcp) else {
                        break;
                    };
                    if write_frame(&mut client, &answer).is_err() {
                        break;
                    }
                }
            }
        }
    }

    fn answer(&self, wire: &[u8], transport: Transport) -> Option<Vec<u8>> {
        let request = Message::from_vec(wire).ok()?;
        if request.metadata.message_type != MessageType::Query {
            return None;
        }

        let mut reply = response_to(&request, !self.upstream.is_empty());
        if request.metadata.op_code != OpCode::Query {
            reply.metadata.response_code = ResponseCode::NotImp;
        } else if let [query] = request.queries.as_slice() {
            if query.query_class != DNSClass::IN
                || matches!(query.query_type, RecordType::AXFR | RecordType::IXFR)
            {
                reply.metadata.response_code = ResponseCode::Refused;
            } else {
                let name = query.name.to_ascii();
                match network::lookup_peer(&self.dirs, &self.caller, name.trim_end_matches('.')) {
                    Ok(peers) if !peers.is_empty() => {
                        reply.metadata.authoritative = true;
                        for address in peers {
                            let data = match (address, query.query_type) {
                                (IpAddr::V4(ip), RecordType::A | RecordType::ANY) => {
                                    RData::A(A(ip))
                                }
                                (IpAddr::V6(ip), RecordType::AAAA | RecordType::ANY) => {
                                    RData::AAAA(AAAA(ip))
                                }
                                _ => continue,
                            };
                            // Membership can change immediately after a live attach or detach.
                            reply.add_answer(Record::from_rdata(query.name.clone(), 0, data));
                        }
                    }
                    Ok(_) => {
                        if let Some(wire) =
                            forward(&request, wire, transport, &self.upstream, &self.stopping)
                        {
                            return Some(wire);
                        }
                        reply.metadata.response_code = ResponseCode::ServFail;
                    }
                    Err(_) => reply.metadata.response_code = ResponseCode::ServFail,
                }
            }
        } else {
            reply.metadata.response_code = ResponseCode::FormErr;
        }
        encode(reply, &request, transport)
    }
}

fn response_to(request: &Message, recursive: bool) -> Message {
    let mut response = Message::response(request.metadata.id, request.metadata.op_code);
    if let [query] = request.queries.as_slice() {
        response.add_query(query.clone());
    }
    response.metadata.recursion_desired = request.metadata.recursion_desired;
    response.metadata.recursion_available = recursive;
    if request.edns.is_some() {
        let mut edns = Edns::new();
        edns.set_max_payload(MAX_UDP);
        response.set_edns(edns);
    }
    response
}

fn encode(reply: Message, request: &Message, transport: Transport) -> Option<Vec<u8>> {
    let bytes = reply.to_vec().ok()?;
    if matches!(transport, Transport::Udp)
        && bytes.len() > usize::from(request.max_payload().min(MAX_UDP))
    {
        return reply.truncate().to_vec().ok();
    }
    Some(bytes)
}

fn forward(
    request: &Message,
    wire: &[u8],
    transport: Transport,
    upstream: &[SocketAddr],
    stopping: &AtomicBool,
) -> Option<Vec<u8>> {
    for endpoint in upstream {
        if stopping.load(Ordering::Relaxed) {
            break;
        }
        let response = match transport {
            Transport::Udp => forward_udp(*endpoint, wire),
            Transport::Tcp => forward_tcp(*endpoint, wire),
        };
        let Ok(response) = response else { continue };
        let Ok(decoded) = Message::from_vec(&response) else {
            continue;
        };
        if decoded.metadata.message_type != MessageType::Response
            || decoded.metadata.id != request.metadata.id
            || decoded.metadata.op_code != request.metadata.op_code
            || decoded.queries != request.queries
            || matches!(
                decoded.metadata.response_code,
                ResponseCode::ServFail | ResponseCode::Refused
            )
        {
            continue;
        }
        match transport {
            Transport::Udp if response.len() > usize::from(request.max_payload().min(MAX_UDP)) => {
                return decoded.truncate().to_vec().ok();
            }
            _ => return Some(response),
        }
    }
    None
}

fn forward_udp(endpoint: SocketAddr, wire: &[u8]) -> io::Result<Vec<u8>> {
    let bind = match endpoint {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    };
    let socket = UdpSocket::bind(bind)?;
    socket.connect(endpoint)?;
    socket.set_read_timeout(Some(TIMEOUT))?;
    socket.set_write_timeout(Some(TIMEOUT))?;
    socket.send(wire)?;
    let mut reply = vec![0; MAX_PACKET];
    let len = socket.recv(&mut reply)?;
    reply.truncate(len);
    Ok(reply)
}

fn forward_tcp(endpoint: SocketAddr, wire: &[u8]) -> io::Result<Vec<u8>> {
    let mut socket = TcpStream::connect_timeout(&endpoint, TIMEOUT)?;
    write_frame(&mut socket, wire)?;
    read_frame(&mut socket)
}

fn read_frame(conn: &mut TcpStream) -> io::Result<Vec<u8>> {
    // One deadline for the whole record keeps a slow peer from resetting it byte by byte.
    let deadline = Instant::now() + TIMEOUT;
    let mut read = |mut buffer: &mut [u8]| -> io::Result<()> {
        while !buffer.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            conn.set_read_timeout(Some(left))?;
            match conn.read(buffer) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(len) => buffer = &mut buffer[len..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    };
    let mut size = [0u8; 2];
    read(&mut size)?;
    let mut frame = vec![0; u16::from_be_bytes(size) as usize];
    read(&mut frame)?;
    Ok(frame)
}

fn write_frame(conn: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    let len = u16::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidData)?;
    conn.set_write_timeout(Some(TIMEOUT))?;
    conn.write_all(&len.to_be_bytes())?;
    conn.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Query;
    use hickory_proto::rr::Name;

    #[test]
    fn forwards_real_queries_to_an_upstream_over_udp_and_tcp() {
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = tcp.local_addr().unwrap();
        tcp.set_nonblocking(true).unwrap();
        let udp = UdpSocket::bind(addr).unwrap();
        udp.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut query = Message::query();
        query.metadata.recursion_desired = true;
        let name = Name::from_ascii("upstream.example.").unwrap();
        query.add_query(Query::query(name.clone(), RecordType::A));
        let wire = query.to_vec().unwrap();
        let ip = Ipv4Addr::new(192, 0, 2, 17);
        let answer = |wire: &[u8]| {
            let query = Message::from_vec(wire).unwrap();
            assert!(query.metadata.recursion_desired);
            let mut reply = response_to(&query, true);
            reply.add_answer(Record::from_rdata(name.clone(), 60, RData::A(A(ip))));
            reply.to_vec().unwrap()
        };
        let stopping = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                let mut frame = [0u8; 512];
                let (n, from) = udp.recv_from(&mut frame).unwrap();
                udp.send_to(&answer(&frame[..n]), from).unwrap();
            });
            scope.spawn(|| {
                let deadline = Instant::now() + TIMEOUT;
                let mut accepted = loop {
                    match tcp.accept() {
                        Ok((accepted, _)) => break accepted,
                        Err(e)
                            if e.kind() == io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept the resolver's TCP query: {error}"),
                    }
                };
                let frame = read_frame(&mut accepted).unwrap();
                write_frame(&mut accepted, &answer(&frame)).unwrap();
            });
            for transport in [Transport::Udp, Transport::Tcp] {
                let bytes = forward(&query, &wire, transport, &[addr], &stopping).unwrap();
                let received = Message::from_vec(&bytes).unwrap();
                assert_eq!(received.queries, query.queries);
                assert_eq!(received.answers[0].data, RData::A(A(ip)));
            }
        });
    }
}
