//! A small HTTP/2 server (RFC 9113) for gRPC over a Unix socket, without an async runtime.
//!
//! One thread reads a connection's frames. Each request runs on a thread of its own, and
//! answers through a [`Response`], which writes whole frames under a lock and waits for
//! flow-control credit. The server sends no pushes and keeps no priorities.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::hpack::{self, Decoder, Field};

pub const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

// Frame types.
pub const DATA: u8 = 0x0;
pub const HEADERS: u8 = 0x1;
pub const PRIORITY: u8 = 0x2;
pub const RST_STREAM: u8 = 0x3;
pub const SETTINGS: u8 = 0x4;
pub const PING: u8 = 0x6;
pub const GOAWAY: u8 = 0x7;
pub const WINDOW_UPDATE: u8 = 0x8;
pub const CONTINUATION: u8 = 0x9;

// Flags.
pub const END_STREAM: u8 = 0x1;
pub const ACK: u8 = 0x1;
pub const END_HEADERS: u8 = 0x4;
pub const PADDED: u8 = 0x8;
pub const PRIORITY_FLAG: u8 = 0x20;

// Error codes.
const PROTOCOL_ERROR: u32 = 0x1;
const FLOW_CONTROL_ERROR: u32 = 0x3;
const FRAME_SIZE_ERROR: u32 = 0x6;
const REFUSED_STREAM: u32 = 0x7;
const CANCEL: u32 = 0x8;
const COMPRESSION_ERROR: u32 = 0x9;

/// The size of a frame we accept, and of our header table.
const MAX_FRAME: usize = 16_384;
const TABLE_SIZE: usize = 4096;
/// A request body larger than this is refused (the kubelet sends at most a few MiB).
pub const MAX_BODY: usize = 16 << 20;
/// Our receive window per stream, announced in SETTINGS, and for the whole connection; we
/// hand credit back as data comes.
const RECV_WINDOW: u32 = 1 << 20;
/// Every window's size before SETTINGS or WINDOW_UPDATE change it.
const DEFAULT_WINDOW: u32 = 65_535;
/// Header blocks larger than this end the connection.
const MAX_HEADER_BLOCK: usize = 256 << 10;

/// One request: its headers (pseudo-headers included) and its whole body.
pub struct Request {
    pub stream: u32,
    pub headers: Vec<Field>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn path(&self) -> &str {
        self.header(":path").unwrap_or("")
    }
}

/// What answers requests; called on a thread per request.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, request: Request, response: &mut Response);
}

/// Sends one stream's response: headers, then data, then trailers.
pub struct Response {
    stream: u32,
    conn: Arc<Shared>,
    headers_sent: bool,
    done: bool,
}

/// What the reader and the responders share.
struct Shared {
    writer: Mutex<UnixStream>,
    flow: Mutex<Flow>,
    credit: Condvar,
}

struct Flow {
    /// Credit we may still send, on the connection and per open stream.
    connection: i64,
    streams: HashMap<u32, i64>,
    /// The peer's SETTINGS_INITIAL_WINDOW_SIZE and SETTINGS_MAX_FRAME_SIZE.
    initial: i64,
    max_frame: usize,
    /// Streams the peer reset, and whether the connection is gone.
    reset: HashSet<u32>,
    closed: bool,
}

impl Shared {
    fn send(&self, kind: u8, flags: u8, stream: u32, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(9 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        frame.push(kind);
        frame.push(flags);
        frame.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
        frame.extend_from_slice(payload);
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(&frame)
    }

    fn goaway(&self, last: u32, code: u32) {
        let mut payload = (last & 0x7fff_ffff).to_be_bytes().to_vec();
        payload.extend_from_slice(&code.to_be_bytes());
        let _ = self.send(GOAWAY, 0, 0, &payload);
    }
}

impl Response {
    /// Response headers (`:status` first). Sent at most once.
    pub fn headers(&mut self, fields: &[(&str, &str)], end: bool) -> io::Result<()> {
        if self.headers_sent && !end {
            return Ok(());
        }
        self.headers_sent = true;
        self.header_block(fields, end)
    }

    /// Body data; blocks while the peer gives no credit. Fails once the stream is reset.
    pub fn data(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = self.take_credit(bytes.len())?;
            self.conn.send(DATA, 0, self.stream, &bytes[..n])?;
            bytes = &bytes[n..];
        }
        Ok(())
    }

    /// Trailers, which end the stream.
    pub fn trailers(&mut self, fields: &[(&str, &str)]) -> io::Result<()> {
        self.header_block(fields, true)
    }

    /// Whether the peer has reset the stream (for long streaming responses).
    pub fn cancelled(&self) -> bool {
        let flow = self.conn.flow.lock().unwrap_or_else(|e| e.into_inner());
        flow.closed || flow.reset.contains(&self.stream)
    }

    fn header_block(&mut self, fields: &[(&str, &str)], end: bool) -> io::Result<()> {
        if self.cancelled() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream reset"));
        }
        let block = hpack::encode(fields);
        let max = self
            .conn
            .flow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .max_frame;
        let mut chunks = block.chunks(max).peekable();
        let first = chunks.next().unwrap_or(&[]);
        let last = chunks.peek().is_none();
        let flags = if end { END_STREAM } else { 0 } | if last { END_HEADERS } else { 0 };
        self.conn.send(HEADERS, flags, self.stream, first)?;
        while let Some(chunk) = chunks.next() {
            let flags = if chunks.peek().is_none() {
                END_HEADERS
            } else {
                0
            };
            self.conn.send(CONTINUATION, flags, self.stream, chunk)?;
        }
        if end {
            self.done = true;
            let mut flow = self.conn.flow.lock().unwrap_or_else(|e| e.into_inner());
            flow.streams.remove(&self.stream);
            flow.reset.remove(&self.stream);
        }
        Ok(())
    }

    /// Waits for send credit; returns how many octets may go now.
    fn take_credit(&self, want: usize) -> io::Result<usize> {
        let mut flow = self.conn.flow.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if flow.closed || flow.reset.contains(&self.stream) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream reset"));
            }
            let stream = *flow.streams.get(&self.stream).unwrap_or(&0);
            let n = (want as i64)
                .min(stream)
                .min(flow.connection)
                .min(flow.max_frame as i64);
            if n > 0 {
                flow.connection -= n;
                *flow.streams.entry(self.stream).or_insert(0) -= n;
                return Ok(n as usize);
            }
            flow = self
                .conn
                .credit
                .wait_timeout(flow, Duration::from_secs(60))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        // A handler that returned without ending its stream: reset it so the peer moves on.
        if !self.done {
            let _ = self
                .conn
                .send(RST_STREAM, 0, self.stream, &CANCEL.to_be_bytes());
            let mut flow = self.conn.flow.lock().unwrap_or_else(|e| e.into_inner());
            flow.streams.remove(&self.stream);
        }
    }
}

/// A request stream being received.
struct Incoming {
    headers: Vec<Field>,
    body: Vec<u8>,
    /// Octets of DATA since we last gave the stream credit back.
    unacked: u32,
}

/// Serves one connection until the peer closes it or breaks the protocol.
pub fn serve(stream: UnixStream, handler: Arc<dyn Handler>) -> io::Result<()> {
    let mut reader = stream.try_clone()?;
    let conn = Arc::new(Shared {
        writer: Mutex::new(stream),
        flow: Mutex::new(Flow {
            connection: DEFAULT_WINDOW as i64,
            streams: HashMap::new(),
            initial: DEFAULT_WINDOW as i64,
            max_frame: MAX_FRAME,
            reset: HashSet::new(),
            closed: false,
        }),
        credit: Condvar::new(),
    });
    let result = read_loop(&mut reader, &conn, &handler);
    let mut flow = conn.flow.lock().unwrap_or_else(|e| e.into_inner());
    flow.closed = true;
    conn.credit.notify_all();
    result
}

fn read_loop(
    reader: &mut UnixStream,
    conn: &Arc<Shared>,
    handler: &Arc<dyn Handler>,
) -> io::Result<()> {
    let mut preface = [0u8; 24];
    reader.read_exact(&mut preface)?;
    if &preface != PREFACE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an HTTP/2 client",
        ));
    }
    // Our settings: header table, concurrent streams, receive window.
    let mut settings = Vec::new();
    for (id, value) in [
        (0x1u16, TABLE_SIZE as u32),
        (0x3, 1000),
        (0x4, RECV_WINDOW),
        (0x6, MAX_HEADER_BLOCK as u32),
    ] {
        settings.extend_from_slice(&id.to_be_bytes());
        settings.extend_from_slice(&value.to_be_bytes());
    }
    conn.send(SETTINGS, 0, 0, &settings)?;
    // SETTINGS sets only each stream's window; the connection's starts at 65,535 octets.
    // Raise it to the same size, or the client stops for good once that much has come.
    conn.send(
        WINDOW_UPDATE,
        0,
        0,
        &(RECV_WINDOW - DEFAULT_WINDOW).to_be_bytes(),
    )?;

    let mut decoder = Decoder::new(TABLE_SIZE);
    let mut incoming: HashMap<u32, Incoming> = HashMap::new();
    // A header block in progress: (stream, fragments, end stream).
    let mut block: Option<(u32, Vec<u8>, bool)> = None;
    let mut last_stream = 0u32;
    let mut connection_unacked = 0u32;
    loop {
        let mut head = [0u8; 9];
        match reader.read_exact(&mut head) {
            // Clients often close without GOAWAY (crictl does): that is a normal end.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            other => other?,
        }
        let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
        let (kind, flags) = (head[3], head[4]);
        let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
        if len > MAX_FRAME {
            conn.goaway(last_stream, FRAME_SIZE_ERROR);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;

        // Only CONTINUATION may follow a header block that has not ended.
        if let Some((stream, _, _)) = &block
            && (kind != CONTINUATION || id != *stream)
        {
            conn.goaway(last_stream, PROTOCOL_ERROR);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header block interrupted",
            ));
        }

        match kind {
            SETTINGS => {
                if flags & ACK != 0 {
                    continue;
                }
                apply_settings(conn, &payload)?;
                conn.send(SETTINGS, ACK, 0, &[])?;
            }
            PING => {
                if flags & ACK == 0 && payload.len() == 8 {
                    conn.send(PING, ACK, 0, &payload)?;
                }
            }
            WINDOW_UPDATE => {
                if payload.len() != 4 {
                    conn.goaway(last_stream, FRAME_SIZE_ERROR);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bad WINDOW_UPDATE",
                    ));
                }
                let inc = (u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                    & 0x7fff_ffff) as i64;
                let mut flow = conn.flow.lock().unwrap_or_else(|e| e.into_inner());
                if id == 0 {
                    flow.connection += inc;
                    if flow.connection > i32::MAX as i64 {
                        drop(flow);
                        conn.goaway(last_stream, FLOW_CONTROL_ERROR);
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "window too large",
                        ));
                    }
                } else if let Some(w) = flow.streams.get_mut(&id) {
                    *w += inc;
                }
                conn.credit.notify_all();
            }
            HEADERS => {
                if id == 0 || id % 2 == 0 {
                    conn.goaway(last_stream, PROTOCOL_ERROR);
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "bad stream id"));
                }
                let fragment = strip(&payload, flags, true)?;
                if id > last_stream {
                    last_stream = id;
                }
                block = Some((id, fragment.to_vec(), flags & END_STREAM != 0));
                if flags & END_HEADERS != 0 {
                    finish_headers(
                        conn,
                        handler,
                        &mut decoder,
                        &mut incoming,
                        block.take().unwrap(),
                        last_stream,
                    )?;
                }
            }
            CONTINUATION => {
                let Some((_, fragments, _)) = block.as_mut() else {
                    conn.goaway(last_stream, PROTOCOL_ERROR);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "CONTINUATION without HEADERS",
                    ));
                };
                fragments.extend_from_slice(&payload);
                if fragments.len() > MAX_HEADER_BLOCK {
                    conn.goaway(last_stream, PROTOCOL_ERROR);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "header block too large",
                    ));
                }
                if flags & END_HEADERS != 0 {
                    finish_headers(
                        conn,
                        handler,
                        &mut decoder,
                        &mut incoming,
                        block.take().unwrap(),
                        last_stream,
                    )?;
                }
            }
            DATA => {
                let data = strip(&payload, flags, false)?;
                // Hand flow-control credit back at once for what arrived (padding included).
                connection_unacked += len as u32;
                if connection_unacked >= RECV_WINDOW / 2 {
                    conn.send(WINDOW_UPDATE, 0, 0, &connection_unacked.to_be_bytes())?;
                    connection_unacked = 0;
                }
                let Some(request) = incoming.get_mut(&id) else {
                    // Data for a stream we reset or never saw: ignore it.
                    continue;
                };
                request.body.extend_from_slice(data);
                request.unacked += len as u32;
                if request.body.len() > MAX_BODY {
                    incoming.remove(&id);
                    conn.send(RST_STREAM, 0, id, &REFUSED_STREAM.to_be_bytes())?;
                    continue;
                }
                if flags & END_STREAM != 0 {
                    let request = incoming.remove(&id).unwrap();
                    dispatch(conn, handler, id, request.headers, request.body);
                } else if request.unacked >= RECV_WINDOW / 2 {
                    conn.send(WINDOW_UPDATE, 0, id, &request.unacked.to_be_bytes())?;
                    request.unacked = 0;
                }
            }
            RST_STREAM => {
                incoming.remove(&id);
                let mut flow = conn.flow.lock().unwrap_or_else(|e| e.into_inner());
                flow.reset.insert(id);
                conn.credit.notify_all();
            }
            GOAWAY => return Ok(()),
            PRIORITY => {}
            _ => {} // Unknown frame types are ignored (RFC 9113 5.5).
        }
    }
}

fn apply_settings(conn: &Shared, payload: &[u8]) -> io::Result<()> {
    if !payload.len().is_multiple_of(6) {
        conn.goaway(0, FRAME_SIZE_ERROR);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad SETTINGS"));
    }
    let mut flow = conn.flow.lock().unwrap_or_else(|e| e.into_inner());
    for item in payload.chunks(6) {
        let id = u16::from_be_bytes([item[0], item[1]]);
        let value = u32::from_be_bytes([item[2], item[3], item[4], item[5]]);
        match id {
            0x4 => {
                // SETTINGS_INITIAL_WINDOW_SIZE changes every open stream's window by the
                // difference (RFC 9113 6.9.2).
                let delta = value as i64 - flow.initial;
                flow.initial = value as i64;
                for w in flow.streams.values_mut() {
                    *w += delta;
                }
            }
            0x5 if (MAX_FRAME as u32..=(1 << 24) - 1).contains(&value) => {
                flow.max_frame = value as usize;
            }
            _ => {}
        }
    }
    conn.credit.notify_all();
    Ok(())
}

/// The frame's content without padding (and without HEADERS' priority fields).
fn strip(payload: &[u8], flags: u8, headers: bool) -> io::Result<&[u8]> {
    let mut start = 0;
    let mut end = payload.len();
    if flags & PADDED != 0 {
        let pad = *payload.first().ok_or_else(|| bad("padding"))? as usize;
        start = 1;
        end = end
            .checked_sub(pad)
            .filter(|&e| e >= start)
            .ok_or_else(|| bad("padding"))?;
    }
    if headers && flags & PRIORITY_FLAG != 0 {
        start += 5;
        if start > end {
            return Err(bad("priority"));
        }
    }
    Ok(&payload[start..end])
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("bad frame: {what}"))
}

fn finish_headers(
    conn: &Arc<Shared>,
    handler: &Arc<dyn Handler>,
    decoder: &mut Decoder,
    incoming: &mut HashMap<u32, Incoming>,
    (id, fragments, end): (u32, Vec<u8>, bool),
    last_stream: u32,
) -> io::Result<()> {
    let fields = match decoder.decode(&fragments) {
        Ok(fields) => fields,
        Err(e) => {
            conn.goaway(last_stream, COMPRESSION_ERROR);
            return Err(io::Error::new(io::ErrorKind::InvalidData, e));
        }
    };
    {
        let mut flow = conn.flow.lock().unwrap_or_else(|e| e.into_inner());
        let initial = flow.initial;
        flow.streams.insert(id, initial);
    }
    if end {
        dispatch(conn, handler, id, fields, Vec::new());
    } else {
        incoming.insert(
            id,
            Incoming {
                headers: fields,
                body: Vec::new(),
                unacked: 0,
            },
        );
    }
    Ok(())
}

fn dispatch(
    conn: &Arc<Shared>,
    handler: &Arc<dyn Handler>,
    stream: u32,
    headers: Vec<Field>,
    body: Vec<u8>,
) {
    let worker = Arc::clone(conn);
    let handler = Arc::clone(handler);
    let spawned = std::thread::Builder::new()
        .name(format!("cri-{stream}"))
        .spawn(move || {
            let mut response = Response {
                stream,
                conn: worker,
                headers_sent: false,
                done: false,
            };
            handler.handle(
                Request {
                    stream,
                    headers,
                    body,
                },
                &mut response,
            );
        });
    if spawned.is_err() {
        // No thread to run it on: refuse the stream; the peer may retry it.
        let _ = conn.send(RST_STREAM, 0, stream, &REFUSED_STREAM.to_be_bytes());
        conn.flow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .remove(&stream);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A test client: writes frames, reads frames.
    pub(crate) struct Client {
        pub sock: UnixStream,
        pub decoder: Decoder,
    }

    impl Client {
        pub fn connect(handler: Arc<dyn Handler>) -> Client {
            let (a, b) = UnixStream::pair().unwrap();
            std::thread::spawn(move || {
                let _ = serve(b, handler);
            });
            let mut sock = a;
            sock.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            sock.write_all(PREFACE).unwrap();
            let mut client = Client {
                sock,
                decoder: Decoder::new(4096),
            };
            client.frame(SETTINGS, 0, 0, &[]);
            client
        }

        pub fn frame(&mut self, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
            let mut frame = (payload.len() as u32).to_be_bytes()[1..].to_vec();
            frame.extend_from_slice(&[kind, flags]);
            frame.extend_from_slice(&stream.to_be_bytes());
            frame.extend_from_slice(payload);
            self.sock.write_all(&frame).unwrap();
        }

        /// The next frame that is not SETTINGS, PING or WINDOW_UPDATE.
        pub fn next(&mut self) -> (u8, u8, u32, Vec<u8>) {
            loop {
                let mut head = [0u8; 9];
                self.sock.read_exact(&mut head).unwrap();
                let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
                let mut payload = vec![0u8; len];
                self.sock.read_exact(&mut payload).unwrap();
                let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]);
                if head[3] == SETTINGS
                    || head[3] == WINDOW_UPDATE
                    || (head[3] == PING && head[4] & ACK != 0)
                {
                    continue;
                }
                return (head[3], head[4], id, payload);
            }
        }

        pub fn request(&mut self, stream: u32, path: &str, body: &[u8]) {
            let block = hpack::encode(&[
                (":method", "POST"),
                (":scheme", "http"),
                (":path", path),
                (":authority", "localhost"),
                ("content-type", "application/grpc"),
                ("te", "trailers"),
            ]);
            self.frame(HEADERS, END_HEADERS, stream, &block);
            self.frame(DATA, END_STREAM, stream, body);
        }
    }

    struct Echo;
    impl Handler for Echo {
        fn handle(&self, request: Request, response: &mut Response) {
            let path = request.path().to_string();
            response
                .headers(&[(":status", "200"), ("x-path", &path)], false)
                .unwrap();
            response.data(&request.body).unwrap();
            response.trailers(&[("x-done", "yes")]).unwrap();
        }
    }

    #[test]
    fn answers_a_request_with_headers_data_and_trailers() {
        let mut c = Client::connect(Arc::new(Echo));
        c.request(1, "/x/Echo", b"hello");
        let (kind, flags, id, block) = c.next();
        assert_eq!((kind, id), (HEADERS, 1));
        assert_eq!(flags & END_STREAM, 0);
        let headers = c.decoder.decode(&block).unwrap();
        assert!(headers.contains(&("x-path".into(), "/x/Echo".into())));
        let (kind, _, _, data) = c.next();
        assert_eq!((kind, data.as_slice()), (DATA, &b"hello"[..]));
        let (kind, flags, _, block) = c.next();
        assert_eq!(kind, HEADERS);
        assert_ne!(flags & END_STREAM, 0);
        assert_eq!(
            c.decoder.decode(&block).unwrap(),
            vec![("x-done".to_string(), "yes".to_string())]
        );
    }

    #[test]
    fn answers_pings_and_joins_continuations() {
        let mut c = Client::connect(Arc::new(Echo));
        c.frame(PING, 0, 0, b"12345678");
        let (kind, flags, _, data) = loop {
            let mut head = [0u8; 9];
            c.sock.read_exact(&mut head).unwrap();
            let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
            let mut payload = vec![0u8; len];
            c.sock.read_exact(&mut payload).unwrap();
            if head[3] == PING {
                break (head[3], head[4], 0, payload);
            }
        };
        assert_eq!(
            (kind, flags & ACK, data.as_slice()),
            (PING, ACK, &b"12345678"[..])
        );
        // A header block split over HEADERS and CONTINUATION.
        let block = hpack::encode(&[
            (":method", "POST"),
            (":path", "/x/Split"),
            ("content-type", "application/grpc"),
        ]);
        let (a, b) = block.split_at(block.len() / 2);
        c.frame(HEADERS, 0, 3, a);
        c.frame(CONTINUATION, END_HEADERS, 3, b);
        c.frame(DATA, END_STREAM, 3, b"ok");
        let (_, _, id, block) = c.next();
        assert_eq!(id, 3);
        assert!(
            c.decoder
                .decode(&block)
                .unwrap()
                .contains(&("x-path".into(), "/x/Split".into()))
        );
    }

    #[test]
    fn waits_for_flow_control_credit() {
        // The client allows 10 octets per stream; a 25-octet answer comes in three DATA frames,
        // each after more credit.
        let mut c = Client::connect(Arc::new(Echo));
        c.frame(SETTINGS, 0, 0, &[0, 4, 0, 0, 0, 10]);
        c.request(5, "/x/Big", &[b'z'; 25]);
        let (kind, _, _, _) = c.next();
        assert_eq!(kind, HEADERS);
        let mut got = Vec::new();
        while got.len() < 25 {
            let (kind, _, id, data) = c.next();
            assert_eq!((kind, id), (DATA, 5));
            assert!(data.len() <= 10, "sent past the window: {}", data.len());
            got.extend(data);
            c.frame(WINDOW_UPDATE, 0, 5, &10u32.to_be_bytes());
        }
        assert_eq!(got, vec![b'z'; 25]);
    }

    struct Quiet;
    impl Handler for Quiet {
        fn handle(&self, _request: Request, response: &mut Response) {
            response.headers(&[(":status", "200")], false).unwrap();
            response.trailers(&[("grpc-status", "0")]).unwrap();
        }
    }

    #[test]
    fn hands_back_the_connection_window() {
        // A client may send only 65,535 octets on the connection until the server gives
        // credit for stream 0. 40 requests of 4 KiB, keeping to that window, all get answers.
        let mut c = Client::connect(Arc::new(Quiet));
        let body = vec![b'q'; 4096];
        let mut window = DEFAULT_WINDOW as i64;
        let mut answered = 0;
        let read = |c: &mut Client, window: &mut i64, answered: &mut i32| {
            let mut head = [0u8; 9];
            c.sock.read_exact(&mut head).unwrap();
            let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
            let mut payload = vec![0u8; len];
            c.sock.read_exact(&mut payload).unwrap();
            let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]);
            if head[3] == WINDOW_UPDATE && id == 0 {
                *window += u32::from_be_bytes(payload[..4].try_into().unwrap()) as i64;
            }
            if head[3] == HEADERS && head[4] & END_STREAM != 0 {
                *answered += 1;
            }
        };
        for i in 0..40u32 {
            while window < body.len() as i64 {
                read(&mut c, &mut window, &mut answered);
            }
            c.request(1 + 2 * i, "/x/Quiet", &body);
            window -= body.len() as i64;
        }
        while answered < 40 {
            read(&mut c, &mut window, &mut answered);
        }
    }
}
