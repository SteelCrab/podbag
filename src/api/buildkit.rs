//! Docker's h2c upgrades for a private BuildKit worker.

use super::http::Request;
use super::router::{Reply, message, reply_error};
use ::http::{HeaderMap, HeaderName, HeaderValue};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use std::io;
use std::os::unix::net::UnixStream;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

mod export;
mod grpc;
mod worker;
pub(super) use worker::{Client, Owner};

const SESSION_RPC: &str = "http://localhost/moby.buildkit.v1.Control/Session";
const SESSION_ID: &str = "x-docker-expose-session-uuid";
const SESSION_METHOD: &str = "x-docker-expose-session-grpc-method";
const MAX_MESSAGE: usize = 16 << 20;

#[derive(Message)]
struct SessionBytes {
    #[prost(bytes = "bytes", tag = "1")]
    data: Bytes,
}

pub(super) fn upgrade(req: &Request, worker: &Client, is_session: bool) -> Reply {
    if !req.wants_upgrade()
        || !req
            .header("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("h2c"))
    {
        return message(400, "BuildKit requires an h2c connection upgrade");
    }
    let metadata = if is_session {
        let Ok(headers) = session_headers(req) else {
            return message(400, "invalid BuildKit session metadata");
        };
        headers
    } else {
        HeaderMap::new()
    };
    let backend = match worker.connect() {
        Ok(connection) => connection,
        Err(error) => return reply_error(&error),
    };

    Reply::Hijack {
        content_type: "application/grpc",
        run: Box::new(move |mut stream, prefix, head| {
            if head.send_protocol(&mut stream, "h2c").is_ok() {
                let _ = relay(stream, backend, prefix, is_session, metadata);
            }
        }),
    }
}

fn session_headers(req: &Request) -> io::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in &req.headers {
        let name = name.to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            SESSION_ID
                | "x-docker-expose-session-name"
                | "x-docker-expose-session-sharedkey"
                | "x-docker-expose-session-sharedkey-encoded"
                | SESSION_METHOD
        ) {
            continue;
        }
        headers.append(
            HeaderName::from_bytes(name.as_bytes()).map_err(io::Error::other)?,
            HeaderValue::from_str(value).map_err(io::Error::other)?,
        );
    }
    let mut ids = headers.get_all(SESSION_ID).iter();
    if ids.next().is_none_or(HeaderValue::is_empty) || ids.next().is_some() {
        return Err(io::Error::other("missing or repeated BuildKit session ID"));
    }
    Ok(headers)
}

fn relay(
    stream: UnixStream,
    backend: worker::Connection,
    prefix: Vec<u8>,
    is_session: bool,
    metadata: HeaderMap,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    backend.stream.set_nonblocking(true)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?
        .block_on(async {
            let stream = tokio::net::UnixStream::from_std(stream)?;
            let worker = tokio::net::UnixStream::from_std(backend.stream)?;
            let (read, write) = stream.into_split();
            let stream = tokio::io::join(
                AsyncReadExt::chain(std::io::Cursor::new(prefix), read),
                write,
            );
            if is_session {
                return relay_session(stream, worker, metadata, backend.exports).await;
            }
            relay_control(stream, worker, backend.exports).await
        })
}

async fn relay_control(
    client: impl AsyncRead + AsyncWrite + Unpin,
    worker: tokio::net::UnixStream,
    exports: export::Exports,
) -> io::Result<()> {
    let (upstream, mut downstream) = tokio::try_join!(grpc::upstream(worker), async {
        h2::server::handshake(client)
            .await
            .map_err(io::Error::other)
    })?;
    while let Some(incoming) = downstream.accept().await {
        let (request, reply) = incoming.map_err(io::Error::other)?;
        if request.uri().path() == export::SOLVE {
            tokio::spawn(export::solve(
                upstream.clone(),
                request,
                reply,
                exports.clone(),
            ));
        } else {
            tokio::spawn(grpc::forward(upstream.clone(), request, reply));
        }
    }
    Ok(())
}

async fn relay_session(
    client: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    worker: tokio::net::UnixStream,
    mut metadata: HeaderMap,
    exports: export::Exports,
) -> io::Result<()> {
    let session = metadata[SESSION_ID]
        .to_str()
        .map_err(io::Error::other)?
        .to_owned();
    if !metadata.get_all(SESSION_METHOD).iter().any(|value| {
        value
            .as_bytes()
            .eq_ignore_ascii_case(export::FILE_SEND.as_bytes())
    }) {
        metadata.append(SESSION_METHOD, HeaderValue::from_static(export::FILE_SEND));
    }
    let (bridge, reverse) = tokio::io::duplex(64 << 10);
    tokio::select! {
        result = bridge_session(bridge, worker, metadata) => result,
        result = relay_reverse_session(client, reverse, exports, session) => result,
    }
}

async fn relay_reverse_session(
    client: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    worker: tokio::io::DuplexStream,
    exports: export::Exports,
    session: String,
) -> io::Result<()> {
    let (upstream, mut downstream) = tokio::try_join!(grpc::upstream(client), async {
        h2::server::handshake(worker)
            .await
            .map_err(io::Error::other)
    })?;
    while let Some(incoming) = downstream.accept().await {
        let (request, reply) = incoming.map_err(io::Error::other)?;
        tokio::spawn(export::session(
            upstream.clone(),
            request,
            reply,
            exports.clone(),
            session.clone(),
        ));
    }
    Ok(())
}

async fn bridge_session(
    bridge: tokio::io::DuplexStream,
    worker: tokio::net::UnixStream,
    metadata: HeaderMap,
) -> io::Result<()> {
    // The Docker client exposes a gRPC server on its upgraded session. BuildKit
    // connects to it through its own Control/Session bidirectional stream.
    let mut upstream = grpc::upstream(worker)
        .await?
        .ready()
        .await
        .map_err(io::Error::other)?;
    let mut request = ::http::Request::post(SESSION_RPC)
        .version(::http::Version::HTTP_2)
        .body(())
        .map_err(io::Error::other)?;
    *request.headers_mut() = metadata;
    request
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/grpc"));
    request
        .headers_mut()
        .insert("te", HeaderValue::from_static("trailers"));
    let (response, sender) = upstream
        .send_request(request, false)
        .map_err(io::Error::other)?;
    let (client_read, client_write) = tokio::io::split(bridge);
    let send = send_session(client_read, sender);
    let receive = receive_session(client_write, response);
    tokio::pin!(receive);
    tokio::select! {
        result = &mut receive => result,
        result = send => {
            result?;
            receive.await
        }
    }
}

async fn send_session(
    mut client: impl AsyncRead + Unpin,
    mut worker: h2::SendStream<Bytes>,
) -> io::Result<()> {
    let mut buffer = [0; 32 << 10];
    loop {
        let read = client.read(&mut buffer).await?;
        if read == 0 {
            return worker
                .send_data(Bytes::new(), true)
                .map_err(io::Error::other);
        }
        send_message(&mut worker, Bytes::copy_from_slice(&buffer[..read])).await?;
    }
}

async fn send_message(worker: &mut h2::SendStream<Bytes>, data: Bytes) -> io::Result<()> {
    grpc::send_data(worker, encode_message(data)?).await
}

fn encode_message(data: Bytes) -> io::Result<Bytes> {
    let message = SessionBytes { data };
    let len = message.encoded_len();
    let mut frame = BytesMut::with_capacity(5 + len);
    frame.put_u8(0);
    frame.put_u32(len as u32);
    message.encode(&mut frame).map_err(io::Error::other)?;
    Ok(frame.freeze())
}

async fn receive_session(
    mut client: impl AsyncWrite + Unpin,
    response: h2::client::ResponseFuture,
) -> io::Result<()> {
    let response = response.await.map_err(io::Error::other)?;
    if !response.status().is_success() || !grpc_ok(response.headers()) {
        return Err(io::Error::other("BuildKit rejected the session"));
    }
    let mut worker = response.into_body();
    let mut buffer = BytesMut::new();
    while let Some(data) = worker.data().await {
        let data = data.map_err(io::Error::other)?;
        buffer.extend_from_slice(&data);
        while let Some(message) = read_message(&mut buffer)? {
            client.write_all(&message).await?;
        }
        worker
            .flow_control()
            .release_capacity(data.len())
            .map_err(io::Error::other)?;
    }
    if !buffer.is_empty()
        || worker
            .trailers()
            .await
            .map_err(io::Error::other)?
            .is_some_and(|headers| !grpc_ok(&headers))
    {
        return Err(io::Error::other(
            "BuildKit session ended with an invalid response",
        ));
    }
    client.shutdown().await
}

fn grpc_ok(headers: &HeaderMap) -> bool {
    headers
        .get("grpc-status")
        .is_none_or(|status| status == "0")
}

fn read_message(buffer: &mut BytesMut) -> io::Result<Option<Bytes>> {
    let Some((compressed, message)) = grpc::read_frame(buffer)? else {
        return Ok(None);
    };
    if compressed {
        return Err(io::Error::other(
            "compressed BuildKit session frames are unsupported",
        ));
    }
    Ok(Some(
        SessionBytes::decode(message)
            .map_err(io::Error::other)?
            .data,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn parses_split_and_combined_session_frames() {
        let first = encode_message(Bytes::from_static(b"first")).unwrap();
        let second = encode_message(Bytes::from_static(b"second")).unwrap();
        let mut received = BytesMut::from(&first[..3]);
        assert!(read_message(&mut received).unwrap().is_none());
        received.extend_from_slice(&first[3..first.len() - 1]);
        assert!(read_message(&mut received).unwrap().is_none());
        received.extend_from_slice(&first[first.len() - 1..]);
        received.extend_from_slice(&second);
        assert_eq!(read_message(&mut received).unwrap().unwrap(), "first");
        assert_eq!(read_message(&mut received).unwrap().unwrap(), "second");
        assert!(received.is_empty());
        assert!(read_message(&mut BytesMut::from(&[1, 0, 0, 0, 0][..])).is_err());
        assert!(read_message(&mut BytesMut::from(&[0, 127, 0, 0, 0][..])).is_err());
    }

    #[test]
    fn copies_only_session_metadata_and_keeps_repeated_methods() {
        let raw = b"POST /session HTTP/1.1\r\nX-Docker-Expose-Session-Uuid: fixture\r\nX-Docker-Expose-Session-Grpc-Method: /first\r\nX-Docker-Expose-Session-Grpc-Method: /second\r\nAuthorization: unused\r\n\r\n";
        let req = super::super::http::read_request_with(&mut BufReader::new(&raw[..]), &|_| false)
            .unwrap()
            .unwrap();
        let headers = session_headers(&req).unwrap();
        assert_eq!(headers[SESSION_ID], "fixture");
        assert_eq!(
            headers
                .get_all("x-docker-expose-session-grpc-method")
                .iter()
                .collect::<Vec<_>>(),
            ["/first", "/second"]
        );
        assert!(!headers.contains_key("authorization"));

        let ctx = super::super::router::Ctx::for_test(std::path::Path::new("/unused"));
        assert!(matches!(
            super::super::router::route(&req, &ctx),
            Reply::Json(404, _)
        ));
    }
}
