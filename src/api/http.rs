//! The HTTP/1.1 side of the Docker Engine API service: requests with bodies (by length or
//! chunked), keep-alive, close-delimited streams and hijacked connections. Parsing is
//! httparse's; everything else is here. No async runtime: one thread per connection.

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};

/// The most a request line and its headers may take.
pub const MAX_HEAD: usize = 64 * 1024;
/// The most a request body may take.
pub const MAX_BODY: usize = 16 * 1024 * 1024;
const MAX_HEADERS: usize = 100;

#[derive(Debug)]
pub enum HttpError {
    /// The request is malformed or too big: answer 400 and close.
    Bad(String),
    /// The connection failed or timed out: close it.
    Io(std::io::Error),
}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        HttpError::Io(e)
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Query,
    /// Names as sent; look them up with [`Request::header`].
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// How the body comes, when it was left unread for the handler
    /// ([`read_request_with`]); `None` when `body` holds it.
    pub framing: Framing,
    /// HTTP/1.0 closes after each request unless asked otherwise.
    pub http10: bool,
}

/// How a request body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    None,
    Length(u64),
    Chunked,
}

impl Request {
    /// A header's value; names compare without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn header_has(&self, name: &str, token: &str) -> bool {
        self.header(name)
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    }

    /// The client asked to switch the connection to a raw stream (`Upgrade: tcp`).
    pub fn wants_upgrade(&self) -> bool {
        self.header_has("connection", "upgrade") && self.header("upgrade").is_some()
    }

    pub fn keep_alive(&self) -> bool {
        if self.http10 {
            self.header_has("connection", "keep-alive")
        } else {
            !self.header_has("connection", "close")
        }
    }

    /// The body as JSON; an empty body is `null`.
    pub fn json(&self) -> Result<serde_json::Value, HttpError> {
        if self.body.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.body)
            .map_err(|e| HttpError::Bad(format!("the request body is not valid JSON: {e}")))
    }
}

/// Query parameters, percent-decoded, in their order.
#[derive(Debug, Default, Clone)]
pub struct Query(Vec<(String, String)>);

impl Query {
    /// Parse `a=1&b=x%20y`. Pairs that do not decode are dropped.
    pub fn parse(raw: &str) -> Query {
        let pairs = raw
            .split('&')
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                Some((percent_decode(k).ok()?, percent_decode(v).ok()?))
            })
            .collect();
        Query(pairs)
    }

    /// Every value given for `key` (`names=a&names=b`).
    pub fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// `1`, `true` and `True` are true; anything else, or nothing, is false.
    pub fn bool_param(&self, key: &str) -> bool {
        matches!(self.get(key), Some("1" | "true" | "True" | "TRUE"))
    }

    pub fn int_param(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.parse().ok())
    }

    /// Docker's `filters` parameter: a JSON object of lists (`{"label":["a=b"]}`) or of
    /// sets (`{"label":{"a=b":true}}`), as name → values.
    pub fn filters(&self) -> Result<BTreeMap<String, Vec<String>>, HttpError> {
        let Some(raw) = self.get("filters").filter(|r| !r.is_empty()) else {
            return Ok(BTreeMap::new());
        };
        let bad = || HttpError::Bad(format!("invalid filters: {raw}"));
        let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| bad())?;
        let object = value.as_object().ok_or_else(bad)?;
        let mut out = BTreeMap::new();
        for (name, values) in object {
            let list: Vec<String> = match values {
                serde_json::Value::Array(a) => a
                    .iter()
                    .map(|v| v.as_str().map(String::from).ok_or_else(bad))
                    .collect::<Result<_, _>>()?,
                serde_json::Value::Object(o) => o
                    .iter()
                    .filter(|(_, on)| on.as_bool() == Some(true))
                    .map(|(k, _)| k.clone())
                    .collect(),
                _ => return Err(bad()),
            };
            out.insert(name.clone(), list);
        }
        Ok(out)
    }
}

/// Decode `%XX` escapes and `+` (a space in query strings).
pub fn percent_decode(s: &str) -> Result<String, HttpError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| HttpError::Bad(format!("bad escape in {s}")))?;
                out.push(hex);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| HttpError::Bad(format!("{s} is not UTF-8 when decoded")))
}

/// Read one request. `Ok(None)` when the client closed before sending anything.
pub fn read_request(reader: &mut impl BufRead) -> Result<Option<Request>, HttpError> {
    read_request_with(reader, &|_| false)
}

/// [`read_request`], leaving the body in `reader` when `streamed` says so: the handler
/// reads it with a [`BodyReader`] as it arrives, without a size limit.
pub fn read_request_with(
    reader: &mut impl BufRead,
    streamed: &dyn Fn(&Request) -> bool,
) -> Result<Option<Request>, HttpError> {
    // The request line and headers, up to the blank line.
    let mut head = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(HttpError::Io(e)),
        };
        if available.is_empty() {
            if head.is_empty() {
                return Ok(None);
            }
            return Err(HttpError::Bad("the request ended in its headers".into()));
        }
        // Copy up to the end of the head; leave the body in the reader.
        let start = head.len().saturating_sub(3);
        head.extend_from_slice(available);
        let consumed = available.len();
        if let Some(end) = find_head_end(&head[start..]).map(|e| e + start) {
            let used = consumed - (head.len() - end);
            reader.consume(used);
            head.truncate(end);
            break;
        }
        reader.consume(consumed);
        if head.len() > MAX_HEAD {
            return Err(HttpError::Bad("the request headers are too large".into()));
        }
    }
    if head.len() > MAX_HEAD {
        return Err(HttpError::Bad("the request headers are too large".into()));
    }
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => {
            return Err(HttpError::Bad("incomplete request headers".into()));
        }
        Err(e) => return Err(HttpError::Bad(format!("malformed request: {e}"))),
    }
    let method = parsed.method.unwrap_or("").to_string();
    let target = parsed.path.unwrap_or("/");
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
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
    let mut req = Request {
        method,
        path: percent_decode(path)?,
        query: Query::parse(raw_query),
        headers,
        body: Vec::new(),
        framing: Framing::None,
        http10: parsed.version == Some(0),
    };
    let chunked = req
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    if streamed(&req) {
        req.framing = match req.header("content-length") {
            _ if chunked => Framing::Chunked,
            Some(len) => Framing::Length(
                len.parse()
                    .map_err(|_| HttpError::Bad(format!("invalid Content-Length {len}")))?,
            ),
            None => Framing::None,
        };
        return Ok(Some(req));
    }
    if chunked {
        req.body = read_chunked(reader)?;
    } else if let Some(len) = req.header("content-length") {
        let len: usize = len
            .parse()
            .map_err(|_| HttpError::Bad(format!("invalid Content-Length {len}")))?;
        if len > MAX_BODY {
            return Err(HttpError::Bad("the request body is too large".into()));
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body)?;
        req.body = body;
    }
    Ok(Some(req))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn read_line(reader: &mut impl BufRead, limit: usize) -> Result<String, HttpError> {
    let mut line = Vec::new();
    reader.take(limit as u64 + 2).read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        return Err(HttpError::Bad(
            "a chunked body line is too long or cut off".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&line).trim_end().to_string())
}

/// A request body read as it arrives, decoded from its framing. It ends at the body's end;
/// a body cut short is an error.
pub struct BodyReader<'a> {
    inner: &'a mut dyn BufRead,
    chunked: bool,
    /// Bytes left in the body (by length) or in the current chunk.
    left: u64,
    /// A chunk was read: a CRLF comes before the next size line.
    started: bool,
    done: bool,
}

impl<'a> BodyReader<'a> {
    pub fn new(inner: &'a mut dyn BufRead, framing: Framing) -> BodyReader<'a> {
        let (chunked, left) = match framing {
            Framing::None => (false, 0),
            Framing::Length(n) => (false, n),
            Framing::Chunked => (true, 0),
        };
        BodyReader {
            inner,
            chunked,
            left,
            started: false,
            done: !chunked && left == 0,
        }
    }

    /// Move to the next chunk: its size line, after the CRLF that ends the one before.
    fn next_chunk(&mut self, first: bool) -> std::io::Result<()> {
        let bad = |why: String| std::io::Error::new(std::io::ErrorKind::InvalidData, why);
        if !first {
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
            if &crlf != b"\r\n" {
                return Err(bad("a chunk is not followed by CRLF".into()));
            }
        }
        let line = read_line(&mut self.inner, 1024).map_err(|e| bad(e.to_string()))?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_text, 16)
            .map_err(|_| bad(format!("invalid chunk size {size_text:?}")))?;
        if size == 0 {
            while !read_line(&mut self.inner, 8 * 1024)
                .map_err(|e| bad(e.to_string()))?
                .is_empty()
            {}
            self.done = true;
        }
        self.left = size;
        Ok(())
    }
}

impl std::io::Read for BodyReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.done {
            return Ok(0);
        }
        if self.chunked && self.left == 0 {
            let first = !std::mem::replace(&mut self.started, true);
            self.next_chunk(first)?;
            if self.done {
                return Ok(0);
            }
        }
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the request body ended early",
            ));
        }
        self.left -= n as u64;
        if !self.chunked && self.left == 0 {
            self.done = true;
        }
        Ok(n)
    }
}

/// Writes HTTP/1.1 chunks; [`ChunkedWriter::finish`] sends the last one.
pub struct ChunkedWriter<W: Write> {
    inner: W,
}

impl<W: Write> ChunkedWriter<W> {
    pub fn new(inner: W) -> ChunkedWriter<W> {
        ChunkedWriter { inner }
    }

    /// The last chunk: the body is whole.
    pub fn finish(&mut self) -> std::io::Result<()> {
        self.inner.write_all(b"0\r\n\r\n")?;
        self.inner.flush()
    }
}

impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.inner, "{:x}\r\n", buf.len())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Bad(why) => f.write_str(why),
            HttpError::Io(e) => e.fmt(f),
        }
    }
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader, 1024)?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::Bad(format!("invalid chunk size {size_text:?}")))?;
        if size == 0 {
            // Trailers, up to the blank line.
            while !read_line(reader, 8 * 1024)?.is_empty() {}
            return Ok(body);
        }
        // body.len() never passes MAX_BODY, so this cannot wrap around like a sum could.
        if size > MAX_BODY - body.len() {
            return Err(HttpError::Bad("the request body is too large".into()));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
        if &crlf != b"\r\n" {
            return Err(HttpError::Bad("a chunk is not followed by CRLF".into()));
        }
    }
}

/// Whether the client closed its end completely. Unlike [`client_gone`], a client that
/// only ended its input (half-close, as `docker run -i` does at the end of stdin) still
/// counts as there: it reads the container's output.
pub fn peer_closed(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut fds = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    // SAFETY: one valid pollfd, no waiting.
    let n = unsafe { libc::poll(&mut fds, 1, 0) };
    n > 0 && fds.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// True when the client closed its side of `stream` (a peek finds the end), for streams
/// that may stay quiet for a long time.
pub fn client_gone(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut byte = [0u8; 1];
    // SAFETY: a one-byte buffer; MSG_PEEK leaves the data in the socket.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            byte.as_mut_ptr().cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    match n {
        0 => true,
        n if n > 0 => false,
        _ => {
            let e = std::io::Error::last_os_error();
            !matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            )
        }
    }
}

/// The reason phrase for a status code.
pub fn reason(status: u16) -> &'static str {
    match status {
        101 => "UPGRADED",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Unknown",
    }
}

/// The status line, the common headers, `extra`, and the blank line.
pub fn write_head(
    w: &mut impl Write,
    status: u16,
    extra: &[(&str, String)],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (k, v) in common_headers().iter().chain(extra) {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())
}

fn common_headers() -> [(&'static str, String); 4] {
    [
        ("Api-Version", super::router::API_VERSION.to_string()),
        ("Docker-Experimental", "false".to_string()),
        ("Ostype", "linux".to_string()),
        ("Server", format!("podbag/{}", env!("CARGO_PKG_VERSION"))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn parse(raw: &[u8]) -> Result<Option<Request>, HttpError> {
        read_request(&mut BufReader::new(Cursor::new(raw.to_vec())))
    }

    #[test]
    fn a_streamed_body_is_left_for_the_handler() {
        use std::io::Read;
        let raw = b"PUT /a HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n6;x=y\r\n world\r\n0\r\nTrailer: t\r\n\r\n\
                    PUT /b HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET /c HTTP/1.1\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let streamed = |r: &Request| r.method == "PUT";
        let first = read_request_with(&mut reader, &streamed).unwrap().unwrap();
        assert_eq!(first.framing, Framing::Chunked);
        assert!(first.body.is_empty());
        let mut body = String::new();
        BodyReader::new(&mut reader, first.framing)
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "hello world");
        let second = read_request_with(&mut reader, &streamed).unwrap().unwrap();
        assert_eq!(second.framing, Framing::Length(3));
        let mut body = Vec::new();
        BodyReader::new(&mut reader, second.framing)
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, b"abc");
        let third = read_request_with(&mut reader, &streamed).unwrap().unwrap();
        assert_eq!((third.path.as_str(), third.framing), ("/c", Framing::None));
        // A body cut short is an error, not an early end.
        let cut = b"PUT /d HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc";
        let mut reader = BufReader::new(Cursor::new(cut.to_vec()));
        let req = read_request_with(&mut reader, &streamed).unwrap().unwrap();
        let mut body = Vec::new();
        let err = BodyReader::new(&mut reader, req.framing)
            .read_to_end(&mut body)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn parses_a_request_with_a_body() {
        let raw = b"POST /v1.47/containers/create?name=web&all=1&filters=%7B%22label%22%3A%5B%22a%3Db%22%5D%7D HTTP/1.1\r\nHost: docker\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"Image\":\"x\"}";
        let req = parse(raw).unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1.47/containers/create");
        assert_eq!(req.query.get("name"), Some("web"));
        assert!(req.query.bool_param("all"));
        assert_eq!(
            req.query.filters().unwrap()["label"],
            vec!["a=b".to_string()]
        );
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.json().unwrap()["Image"], "x");
        assert!(req.keep_alive());
    }

    #[test]
    fn reads_chunked_bodies() {
        let raw = b"POST /images/load HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nTrailer: x\r\n\r\n";
        let req = parse(raw).unwrap().unwrap();
        assert_eq!(req.body, b"Wikipedia");
    }

    #[test]
    fn filters_accept_both_docker_forms() {
        let q = Query::parse("filters=%7B%22status%22%3A%7B%22running%22%3Atrue%7D%7D");
        assert_eq!(q.filters().unwrap()["status"], vec!["running".to_string()]);
        assert!(Query::parse("filters=nope").filters().is_err());
        assert!(Query::parse("").filters().unwrap().is_empty());
    }

    #[test]
    fn oversized_and_broken_requests_get_400() {
        let mut big = b"GET /_ping HTTP/1.1\r\nX-Big: ".to_vec();
        big.extend(std::iter::repeat_n(b'a', MAX_HEAD + 10));
        big.extend(b"\r\n\r\n");
        assert!(matches!(parse(&big), Err(HttpError::Bad(_))));
        let bad_chunk = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n";
        assert!(matches!(parse(bad_chunk), Err(HttpError::Bad(_))));
        let too_long = format!(
            "POST /x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(matches!(parse(too_long.as_bytes()), Err(HttpError::Bad(_))));
        assert!(matches!(parse(b"NOT HTTP\r\n\r\n"), Err(HttpError::Bad(_))));
        // A chunk size that wraps the running total around must not pass the limit.
        let wrap =
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\nffffffffffffffff\r\n";
        assert!(matches!(parse(wrap), Err(HttpError::Bad(_))));
        let huge = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nfffffffffffffffff\r\n";
        assert!(matches!(parse(huge), Err(HttpError::Bad(_))));
        assert!(matches!(Query::parse("name=%zz").get("name"), None));
        assert!(parse(b"").unwrap().is_none());
    }

    #[test]
    fn upgrade_and_close_are_seen() {
        let req = parse(b"POST /containers/x/attach?stream=1 HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(req.wants_upgrade());
        let req = parse(b"GET /_ping HTTP/1.1\r\nConnection: close\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(!req.keep_alive());
    }
}
