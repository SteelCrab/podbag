//! HTTP/1.1 on one connection: a request with `Connection: close`, then the response
//! head and its body, decoded from its framing (chunked, by length, or until close).

use std::io::{self, BufRead, BufReader, Read, Write};

/// A connection: a Unix socket, or a program's stdin and stdout.
pub trait Conn: Read + Write + Send {}

impl<T: Read + Write + Send> Conn for T {}

/// The most a response head may take.
const MAX_HEAD: usize = 64 * 1024;

/// A response: its status and headers; the body is read from it.
pub struct Response {
    pub status: u16,
    headers: Vec<(String, String)>,
    body: Reader,
}

enum Framing {
    Length(u64),
    Chunked { left: u64, started: bool },
    Close,
    Done,
}

struct Reader {
    inner: BufReader<Box<dyn Conn>>,
    framing: Framing,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The whole body (at most `limit` bytes).
    pub fn bytes(mut self, limit: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        (&mut self).take(limit as u64 + 1).read_to_end(&mut out)?;
        if out.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the response is too big",
            ));
        }
        Ok(out)
    }
}

impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.body.read(buf)
    }
}

/// A request body.
pub enum Body<'a> {
    None,
    /// JSON, sent with its length.
    Json(&'a [u8]),
    /// A stream of this content type, sent chunked as it is read.
    Stream(&'static str, &'a mut dyn Read),
}

/// Send a request and read the response head. `headers` go out as given.
pub fn send(
    mut conn: Box<dyn Conn>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> io::Result<Response> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: podbag/{}\r\nConnection: close\r\n",
        env!("CARGO_PKG_VERSION")
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    match &body {
        Body::Json(b) => head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            b.len()
        )),
        Body::Stream(kind, _) => head.push_str(&format!(
            "Content-Type: {kind}\r\nTransfer-Encoding: chunked\r\n\r\n"
        )),
        Body::None if method == "GET" || method == "HEAD" => head.push_str("\r\n"),
        Body::None => head.push_str("Content-Length: 0\r\n\r\n"),
    }
    conn.write_all(head.as_bytes())?;
    let sent = match body {
        Body::None => Ok(()),
        Body::Json(b) => conn.write_all(b),
        Body::Stream(_, r) => write_chunked(&mut conn, r),
    };
    let sent = sent.and_then(|()| conn.flush());
    let head_only = method == "HEAD";
    match sent {
        Ok(()) => read_response(BufReader::new(conn), head_only),
        // The engine may answer (an error) and close before it has the whole body:
        // its answer says more than the failed write.
        Err(e) => read_response(BufReader::new(conn), head_only).map_err(|_| e),
    }
}

/// Copy `r` to `w` as HTTP chunks, then the last (empty) chunk.
fn write_chunked(w: &mut dyn Write, r: &mut dyn Read) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        write!(w, "{n:x}\r\n")?;
        w.write_all(&buf[..n])?;
        w.write_all(b"\r\n")?;
    }
    w.write_all(b"0\r\n\r\n")
}

fn read_response(mut inner: BufReader<Box<dyn Conn>>, head_only: bool) -> io::Result<Response> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
    let mut raw = Vec::new();
    loop {
        let before = raw.len();
        (&mut inner)
            .take((MAX_HEAD - raw.len()) as u64)
            .read_until(b'\n', &mut raw)?;
        if raw.len() == before {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the connection closed before a response",
            ));
        }
        if raw.ends_with(b"\r\n\r\n") || raw.ends_with(b"\n\n") {
            break;
        }
        if raw.len() >= MAX_HEAD {
            return Err(bad("the response head is too big"));
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    match parsed.parse(&raw) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(bad("the response head is not valid HTTP")),
    }
    let status = parsed
        .code
        .ok_or_else(|| bad("the response has no status"))?;
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).trim().to_string(),
            )
        })
        .collect();
    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let framing = if head_only || status == 204 || status == 304 || status == 101 {
        Framing::Done
    } else if find("Transfer-Encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        Framing::Chunked {
            left: 0,
            started: false,
        }
    } else if let Some(n) = find("Content-Length") {
        let n = n.parse().map_err(|_| bad("invalid Content-Length"))?;
        if n == 0 {
            Framing::Done
        } else {
            Framing::Length(n)
        }
    } else {
        Framing::Close
    };
    Ok(Response {
        status,
        headers,
        body: Reader { inner, framing },
    })
}

fn read_line(r: &mut impl BufRead) -> io::Result<String> {
    let mut line = Vec::new();
    r.take(1024).read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "a chunked body was cut off",
        ));
    }
    Ok(String::from_utf8_lossy(&line).trim().to_string())
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match &mut self.framing {
                Framing::Done => return Ok(0),
                Framing::Close => return self.inner.read(buf),
                Framing::Length(left) => {
                    let want = buf.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                    let n = self.inner.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the response body ended early",
                        ));
                    }
                    *left -= n as u64;
                    if *left == 0 {
                        self.framing = Framing::Done;
                    }
                    return Ok(n);
                }
                Framing::Chunked { left, started } if *left == 0 => {
                    if std::mem::replace(started, true) {
                        // The CRLF after the chunk before.
                        read_line(&mut self.inner)?;
                    }
                    let line = read_line(&mut self.inner)?;
                    let size = line.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size, 16).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size")
                    })?;
                    if size == 0 {
                        // Trailers, up to the empty line.
                        while !read_line(&mut self.inner)?.is_empty() {}
                        self.framing = Framing::Done;
                        return Ok(0);
                    }
                    *left = size;
                }
                Framing::Chunked { left, .. } => {
                    let want = buf.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                    let n = self.inner.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the response body ended early",
                        ));
                    }
                    *left -= n as u64;
                    return Ok(n);
                }
            }
        }
    }
}

/// Docker's multiplexed stream (logs of a container without a terminal): 8-byte headers
/// (stream 1 stdout, 2 stderr; then the size, big-endian) before each frame.
pub struct Demux<R> {
    inner: R,
    left: usize,
}

impl<R: Read> Demux<R> {
    pub fn new(inner: R) -> Demux<R> {
        Demux { inner, left: 0 }
    }
}

impl<R: Read> Read for Demux<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.left == 0 {
            let mut header = [0u8; 8];
            match self.inner.read_exact(&mut header) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(e),
            }
            self.left = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        }
        let want = buf.len().min(self.left);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Ok(0);
        }
        self.left -= n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A connection that answers with fixed bytes and drops what is written.
    struct Canned(Cursor<Vec<u8>>);

    impl Read for Canned {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for Canned {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn get(answer: &str) -> io::Result<(u16, String)> {
        let conn = Box::new(Canned(Cursor::new(answer.as_bytes().to_vec())));
        let r = send(conn, "GET", "/x", &[], Body::None)?;
        let status = r.status;
        let body = r.bytes(1024)?;
        Ok((status, String::from_utf8(body).unwrap()))
    }

    #[test]
    fn bodies_by_length_chunks_and_close() {
        assert_eq!(
            get("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap(),
            (200, "hello".into())
        );
        assert_eq!(
            get("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nhel\r\n2;x=y\r\nlo\r\n0\r\n\r\n")
                .unwrap(),
            (200, "hello".into())
        );
        assert_eq!(
            get("HTTP/1.1 404 Not Found\r\n\r\nall of it").unwrap(),
            (404, "all of it".into())
        );
        assert_eq!(
            get("HTTP/1.1 204 No Content\r\n\r\n").unwrap(),
            (204, String::new())
        );
    }

    #[test]
    fn cut_off_bodies_and_bad_heads_are_errors() {
        assert!(get("HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nhello").is_err());
        assert!(get("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nhel").is_err());
        assert!(get("nonsense\r\n\r\n").is_err());
        assert!(get("").is_err());
    }

    #[test]
    fn frames_are_joined() {
        let mut raw = Vec::new();
        for (stream, text) in [(1u8, "out\n"), (2, "err\n"), (1, "")] {
            raw.extend([stream, 0, 0, 0]);
            raw.extend((text.len() as u32).to_be_bytes());
            raw.extend(text.as_bytes());
        }
        let mut out = String::new();
        Demux::new(Cursor::new(raw))
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "out\nerr\n");
    }
}
