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
    /// HTTP/1.0 closes after each request unless asked otherwise.
    pub http10: bool,
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
        http10: parsed.version == Some(0),
    };
    if req
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
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
        ("Server", format!("openpod/{}", env!("CARGO_PKG_VERSION"))),
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
