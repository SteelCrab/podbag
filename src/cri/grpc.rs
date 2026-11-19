//! gRPC on the HTTP/2 server: length-prefixed messages, and the status in trailers.

use std::sync::Arc;

use super::h2::{Handler, Request, Response};

/// gRPC status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Ok = 0,
    Cancelled = 1,
    Unknown = 2,
    InvalidArgument = 3,
    DeadlineExceeded = 4,
    NotFound = 5,
    AlreadyExists = 6,
    PermissionDenied = 7,
    ResourceExhausted = 8,
    FailedPrecondition = 9,
    Aborted = 10,
    OutOfRange = 11,
    Unimplemented = 12,
    Internal = 13,
    Unavailable = 14,
    DataLoss = 15,
    Unauthenticated = 16,
}

/// A call's failure: a code and a message for the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub code: Code,
    pub message: String,
}

impl Status {
    pub fn new(code: Code, message: impl Into<String>) -> Status {
        Status {
            code,
            message: message.into(),
        }
    }
}

/// Unary and server-streaming methods, by path (`/package.Service/Method`).
pub trait Service: Send + Sync + 'static {
    /// The encoded answer to a unary call; `None` when the path is not a unary method here.
    fn unary(&self, path: &str, request: &[u8]) -> Option<Result<Vec<u8>, Status>>;

    /// A server-streaming call: send messages with `out` until done or cancelled; `None`
    /// when the path is not a streaming method here.
    fn stream(
        &self,
        path: &str,
        request: &[u8],
        out: &mut Stream<'_>,
    ) -> Option<Result<(), Status>> {
        let _ = (path, request, out);
        None
    }
}

/// The sending side of a server-streaming call.
pub struct Stream<'a> {
    response: &'a mut Response,
    started: bool,
}

impl Stream<'_> {
    /// Sends one message; fails when the caller has gone.
    pub fn send(&mut self, message: &[u8]) -> Result<(), Status> {
        let gone = |_| Status::new(Code::Cancelled, "the caller went away");
        if !self.started {
            self.response.headers(&OK_HEADERS, false).map_err(gone)?;
            self.started = true;
        }
        self.response.data(&frame(message)).map_err(gone)
    }

    pub fn cancelled(&self) -> bool {
        self.response.cancelled()
    }
}

const OK_HEADERS: [(&str, &str); 2] = [(":status", "200"), ("content-type", "application/grpc")];

/// Serves a [`Service`] over HTTP/2.
pub struct Server<S: Service>(pub Arc<S>);

impl<S: Service> Handler for Server<S> {
    fn handle(&self, request: Request, response: &mut Response) {
        let path = request.path().to_string();
        let grpc = request
            .header("content-type")
            .is_some_and(|t| t.starts_with("application/grpc"));
        if !grpc {
            let _ = response.headers(&[(":status", "415")], true);
            return;
        }
        let message = match message(&request.body) {
            Ok(m) => m,
            Err(status) => return fail(response, &status),
        };
        if let Some(result) = self.0.unary(&path, message) {
            match result {
                Ok(answer) => {
                    let sent = response
                        .headers(&OK_HEADERS, false)
                        .and_then(|_| response.data(&frame(&answer)));
                    if sent.is_ok() {
                        let _ = response.trailers(&[("grpc-status", "0")]);
                    }
                }
                Err(status) => fail(response, &status),
            }
            return;
        }
        let mut out = Stream {
            response,
            started: false,
        };
        let result = self.0.stream(&path, message, &mut out);
        let started = out.started;
        match result {
            Some(Ok(())) if started => {
                let _ = response.trailers(&[("grpc-status", "0")]);
            }
            Some(Ok(())) => {
                let _ =
                    response.headers(&[OK_HEADERS[0], OK_HEADERS[1], ("grpc-status", "0")], true);
            }
            Some(Err(status)) if started => {
                let code = (status.code as u8).to_string();
                let text = percent_encode(&status.message);
                let _ = response.trailers(&[("grpc-status", &code), ("grpc-message", &text)]);
            }
            Some(Err(status)) => fail(response, &status),
            None => fail(
                response,
                &Status::new(Code::Unimplemented, format!("unknown method {path}")),
            ),
        }
    }
}

/// A trailers-only response: one HEADERS frame with the status.
fn fail(response: &mut Response, status: &Status) {
    let code = (status.code as u8).to_string();
    let text = percent_encode(&status.message);
    let _ = response.headers(
        &[
            OK_HEADERS[0],
            OK_HEADERS[1],
            ("grpc-status", &code),
            ("grpc-message", &text),
        ],
        true,
    );
}

/// The one message in a unary request body.
pub fn message(body: &[u8]) -> Result<&[u8], Status> {
    if body.len() < 5 {
        return Err(Status::new(Code::Internal, "the request has no message"));
    }
    if body[0] != 0 {
        return Err(Status::new(
            Code::Unimplemented,
            "compressed messages are not supported",
        ));
    }
    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    body.get(5..5 + len)
        .filter(|_| body.len() == 5 + len)
        .ok_or_else(|| Status::new(Code::Internal, "the request message has a wrong length"))
}

/// A message with its 5-octet prefix (not compressed).
pub fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + message.len());
    out.push(0);
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// grpc-message is percent-encoded: everything outside printable ASCII, and '%'.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &b in text.as_bytes() {
        if (0x20..=0x7e).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cri::h2::tests::Client;
    use crate::cri::h2::{DATA, END_STREAM, HEADERS};

    struct Upper;
    impl Service for Upper {
        fn unary(&self, path: &str, request: &[u8]) -> Option<Result<Vec<u8>, Status>> {
            match path {
                "/t.T/Upper" => Some(Ok(request.to_ascii_uppercase())),
                "/t.T/Fail" => Some(Err(Status::new(Code::NotFound, "no such thing: 100% gone"))),
                _ => None,
            }
        }
        fn stream(
            &self,
            path: &str,
            request: &[u8],
            out: &mut Stream<'_>,
        ) -> Option<Result<(), Status>> {
            (path == "/t.T/Count").then(|| {
                for i in 0..request[0] {
                    out.send(&[i])?;
                }
                Ok(())
            })
        }
    }

    fn headers(c: &mut Client, block: &[u8]) -> Vec<(String, String)> {
        c.decoder.decode(block).unwrap()
    }

    #[test]
    fn answers_unary_calls_with_a_message_and_status_zero() {
        let mut c = Client::connect(Arc::new(Server(Arc::new(Upper))));
        c.request(1, "/t.T/Upper", &frame(b"abc"));
        let (kind, _, _, block) = c.next();
        assert_eq!(kind, HEADERS);
        assert!(
            headers(&mut c, &block).contains(&("content-type".into(), "application/grpc".into()))
        );
        let (kind, _, _, data) = c.next();
        assert_eq!((kind, data), (DATA, frame(b"ABC")));
        let (_, flags, _, block) = c.next();
        assert_ne!(flags & END_STREAM, 0);
        assert_eq!(
            headers(&mut c, &block),
            vec![("grpc-status".to_string(), "0".to_string())]
        );
    }

    #[test]
    fn errors_are_trailers_only_with_an_encoded_message() {
        let mut c = Client::connect(Arc::new(Server(Arc::new(Upper))));
        c.request(1, "/t.T/Fail", &frame(b""));
        let (kind, flags, _, block) = c.next();
        assert_eq!(kind, HEADERS);
        assert_ne!(flags & END_STREAM, 0, "one HEADERS frame ends the stream");
        let h = headers(&mut c, &block);
        assert!(h.contains(&("grpc-status".into(), "5".into())));
        assert!(h.contains(&("grpc-message".into(), "no such thing: 100%25 gone".into())));
        c.request(3, "/t.T/Nope", &frame(b""));
        let (_, _, _, block) = c.next();
        assert!(headers(&mut c, &block).contains(&("grpc-status".into(), "12".into())));
    }

    #[test]
    fn streams_messages_then_ends() {
        let mut c = Client::connect(Arc::new(Server(Arc::new(Upper))));
        c.request(1, "/t.T/Count", &frame(&[3]));
        assert_eq!(c.next().0, HEADERS);
        for i in 0..3u8 {
            let (kind, _, _, data) = c.next();
            assert_eq!((kind, data), (DATA, frame(&[i])));
        }
        let (_, flags, _, block) = c.next();
        assert_ne!(flags & END_STREAM, 0);
        assert_eq!(
            headers(&mut c, &block),
            vec![("grpc-status".to_string(), "0".to_string())]
        );
    }

    /// grpc-go (through grpcurl) against this server: HPACK with Huffman strings and the
    /// dynamic table, SETTINGS, WINDOW_UPDATE and PING as a real client sends them. Skipped
    /// when grpcurl is not installed.
    #[test]
    fn a_real_grpc_client_gets_answers() {
        use crate::cri::pb;
        use prost::Message;
        use std::os::unix::net::UnixListener;
        use std::process::Command;

        if Command::new("grpcurl").arg("-version").output().is_err() {
            eprintln!("skipped: grpcurl is not installed");
            return;
        }
        struct Fake;
        impl Service for Fake {
            fn unary(&self, path: &str, _: &[u8]) -> Option<Result<Vec<u8>, Status>> {
                (path == "/runtime.v1.RuntimeService/Version").then(|| {
                    Ok(pb::VersionResponse {
                        version: "0.1.0".into(),
                        runtime_name: "podbag".into(),
                        runtime_version: "test".into(),
                        runtime_api_version: "v1".into(),
                    }
                    .encode_to_vec())
                })
            }
        }
        let dir = std::env::temp_dir().join(format!("pbg-grpc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("cri.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let server: Arc<dyn crate::cri::h2::Handler> = Arc::new(Server(Arc::new(Fake)));
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let server = Arc::clone(&server);
                std::thread::spawn(move || crate::cri::h2::serve(conn, server));
            }
        });
        let call = |method: &str| {
            Command::new("grpcurl")
                .args([
                    "-plaintext",
                    "-unix",
                    "-import-path",
                    concat!(env!("CARGO_MANIFEST_DIR"), "/dev/cri"),
                ])
                .args(["-proto", "api.proto", "-d", "{}"])
                .arg(&sock)
                .arg(method)
                .output()
                .unwrap()
        };
        let version = call("runtime.v1.RuntimeService/Version");
        let unknown = call("runtime.v1.RuntimeService/Status");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            version.status.success(),
            "{}",
            String::from_utf8_lossy(&version.stderr)
        );
        let text = String::from_utf8_lossy(&version.stdout);
        assert!(text.contains("\"runtimeName\": \"podbag\""), "{text}");
        assert!(text.contains("\"runtimeApiVersion\": \"v1\""), "{text}");
        let out = unknown;
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Unimplemented"),
            "{out:?}"
        );
    }

    #[test]
    fn rejects_bad_message_frames() {
        assert!(message(&[0, 0, 0, 0]).is_err());
        assert!(message(&[1, 0, 0, 0, 0]).is_err(), "compressed");
        assert!(message(&[0, 0, 0, 0, 2, b'x']).is_err(), "short");
        assert_eq!(message(&[0, 0, 0, 0, 1, b'x']).unwrap(), b"x");
    }
}
