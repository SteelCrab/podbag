//! Forward BuildKit HTTP/2 streams while allowing the Engine's own exporter.
use super::{MAX_MESSAGE, grpc_ok};
use ::http::{HeaderMap, HeaderValue, Request, Response, Version};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use h2::{RecvStream, SendStream, client::SendRequest, server::SendResponse};
use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

pub(super) async fn upstream(
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
) -> io::Result<SendRequest<Bytes>> {
    let (sender, connection) = h2::client::handshake(stream)
        .await
        .map_err(io::Error::other)?;
    tokio::spawn(connection);
    Ok(sender)
}

pub(super) async fn forward(
    upstream: SendRequest<Bytes>,
    request: Request<RecvStream>,
    reply: SendResponse<Bytes>,
) -> io::Result<()> {
    let (headers, body) = request.into_parts();
    let end = body.is_end_stream();
    let (response, upload) = upstream
        .ready()
        .await
        .map_err(io::Error::other)?
        .send_request(Request::from_parts(headers, ()), end)
        .map_err(io::Error::other)?;
    let download = async {
        let response = response.await.map_err(io::Error::other)?;
        forward_response(reply, response).await.map(|_| ())
    };
    if end {
        return download.await;
    }
    tokio::pin!(download);
    tokio::select! {
        result = &mut download => result,
        result = copy(body, upload) => {
            result?;
            download.await
        }
    }
}

/// Forward the response and report whether it reached an OK gRPC status.
pub(super) async fn forward_response(
    mut reply: SendResponse<Bytes>,
    response: Response<RecvStream>,
) -> io::Result<bool> {
    let (headers, body) = response.into_parts();
    let end = body.is_end_stream();
    let accepted = headers.status.is_success() && grpc_ok(&headers.headers);
    let completed = ended_successfully(&headers.headers);
    let send = reply
        .send_response(Response::from_parts(headers, ()), end)
        .map_err(io::Error::other)?;
    if end {
        return Ok(accepted && completed);
    }
    let completed = copy(body, send).await?;
    Ok(accepted && completed)
}

async fn copy(mut body: RecvStream, mut send: SendStream<Bytes>) -> io::Result<bool> {
    loop {
        let chunk = tokio::select! {
            reset = std::future::poll_fn(|context| send.poll_reset(context)) => {
                reset.map_err(io::Error::other)?;
                return Ok(false);
            }
            chunk = data(&mut body) => chunk?,
        };
        let Some(chunk) = chunk else {
            let trailers = body.trailers().await.map_err(io::Error::other)?;
            let completed = trailers.as_ref().is_some_and(ended_successfully);
            finish(&mut send, trailers)?;
            return Ok(completed);
        };
        send_data(&mut send, chunk).await?;
    }
}

pub(super) async fn send_data(send: &mut SendStream<Bytes>, mut data: Bytes) -> io::Result<()> {
    send.reserve_capacity(data.len());
    while !data.is_empty() {
        let capacity = std::future::poll_fn(|context| send.poll_capacity(context))
            .await
            .ok_or_else(|| io::Error::other("BuildKit closed the HTTP/2 stream"))?
            .map_err(io::Error::other)?;
        let chunk = data.split_to(capacity.min(data.len()));
        send.send_data(chunk, false).map_err(io::Error::other)?;
    }
    Ok(())
}

pub(super) fn finish(send: &mut SendStream<Bytes>, trailers: Option<HeaderMap>) -> io::Result<()> {
    match trailers {
        Some(trailers) => send.send_trailers(trailers),
        None => send.send_data(Bytes::new(), true),
    }
    .map_err(io::Error::other)
}

pub(super) async fn data(body: &mut RecvStream) -> io::Result<Option<Bytes>> {
    let Some(chunk) = body.data().await else {
        return Ok(None);
    };
    let chunk = chunk.map_err(io::Error::other)?;
    body.flow_control()
        .release_capacity(chunk.len())
        .map_err(io::Error::other)?;
    Ok(Some(chunk))
}

pub(super) async fn unary(body: &mut RecvStream) -> io::Result<BytesMut> {
    let mut buffer = BytesMut::new();
    while let Some(chunk) = data(body).await? {
        if buffer.len() + chunk.len() > MAX_MESSAGE + 5 {
            return Err(io::Error::other("BuildKit Solve message is too large"));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer)
}

pub(super) fn frame(data: &[u8], compressed: bool) -> Bytes {
    let mut frame = BytesMut::with_capacity(5 + data.len());
    frame.put_u8(u8::from(compressed));
    frame.put_u32(data.len() as u32);
    frame.extend_from_slice(data);
    frame.freeze()
}

pub(super) fn read_frame(buffer: &mut BytesMut) -> io::Result<Option<(bool, Bytes)>> {
    if buffer.len() < 5 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(buffer[1..5].try_into().unwrap()) as usize;
    let compressed = buffer[0];
    if compressed > 1 || len > MAX_MESSAGE {
        return Err(io::Error::other("invalid BuildKit gRPC frame"));
    }
    if buffer.len() < 5 + len {
        return Ok(None);
    }
    buffer.advance(5);
    Ok(Some((compressed == 1, buffer.split_to(len).freeze())))
}

pub(super) fn status(
    mut reply: SendResponse<Bytes>,
    status: &'static str,
    message: &str,
) -> io::Result<()> {
    let mut response = Response::builder()
        .version(Version::HTTP_2)
        .header("content-type", "application/grpc")
        .header("grpc-status", status)
        .body(())
        .map_err(io::Error::other)?;
    if !message.is_empty() {
        use std::fmt::Write;
        let mut encoded = String::new();
        for byte in message.as_bytes() {
            if (0x20..=0x7e).contains(byte) && *byte != b'%' {
                encoded.push(*byte as char);
            } else {
                write!(&mut encoded, "%{byte:02X}").unwrap();
            }
        }
        response.headers_mut().insert(
            "grpc-message",
            HeaderValue::from_str(&encoded).map_err(io::Error::other)?,
        );
    }
    reply
        .send_response(response, true)
        .map_err(io::Error::other)?;
    Ok(())
}

pub(super) fn success(response: &Response<RecvStream>, trailers: Option<&HeaderMap>) -> bool {
    response.status().is_success()
        && grpc_ok(response.headers())
        && trailers.is_some_and(ended_successfully)
}

fn ended_successfully(headers: &HeaderMap) -> bool {
    headers
        .get("grpc-status")
        .is_some_and(|status| status == "0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use h2::Reason;
    use std::future::poll_fn;
    use std::time::Duration;
    use tokio::io::duplex;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn cancelling_an_idle_response_resets_the_upstream_subscription() {
        let (worker_socket, proxy_upstream) = duplex(4096);
        let (observed, reset) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut connection = h2::server::handshake(worker_socket).await.unwrap();
            let (_, mut reply) = connection.accept().await.unwrap().unwrap();
            let mut stream = reply.send_response(Response::new(()), false).unwrap();
            let (_, mut reply) = connection.accept().await.unwrap().unwrap();
            let mut other = reply.send_response(Response::new(()), false).unwrap();
            let reason = tokio::select! {
                reset = poll_fn(|context| stream.poll_reset(context)) => reset.unwrap(),
                _ = poll_fn(|context| connection.poll_closed(context)) => {
                    panic!("the worker connection closed before the subscription reset")
                }
            };
            observed.send(reason).unwrap();

            other
                .send_data(Bytes::from_static(b"history event"), false)
                .unwrap();
            let mut trailers = HeaderMap::new();
            trailers.insert("grpc-status", HeaderValue::from_static("0"));
            other.send_trailers(trailers).unwrap();
            let _ = poll_fn(|context| connection.poll_closed(context)).await;
        });
        let sender = upstream(proxy_upstream).await.unwrap();
        let (proxy_socket, client_socket) = duplex(4096);
        let proxy = tokio::spawn(async move {
            let mut connection = h2::server::handshake(proxy_socket).await.unwrap();
            // As in the service, keep the upstream sender for the connection's lifetime.
            while let Some(request) = connection.accept().await {
                let (request, reply) = request.unwrap();
                let sender = sender.clone();
                tokio::spawn(async move { forward(sender, request, reply).await.unwrap() });
            }
        });
        let (sender, connection) = h2::client::handshake(client_socket).await.unwrap();
        let client = tokio::spawn(connection);
        let request = || {
            Request::builder()
                .uri("http://buildkit/moby.buildkit.v1.Control/ListenBuildHistory")
                .body(())
                .unwrap()
        };
        let (fail, deadline) = oneshot::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            let _ = fail.send(());
        });
        tokio::select! {
            _ = deadline => panic!("the idle subscription did not reset or the second subscription stalled"),
            _ = async {
                let mut subscription = sender.clone().ready().await.unwrap();
                let (response, mut send) = subscription.send_request(request(), true).unwrap();
                let response = response.await.unwrap();
                assert!(!response.body().is_end_stream());

                // Wait until both subscriptions have response headers before cancelling one.
                let mut subscription = sender.clone().ready().await.unwrap();
                let (other, _) = subscription.send_request(request(), true).unwrap();
                let mut other = other.await.unwrap().into_body();
                send.send_reset(Reason::CANCEL);

                assert_eq!(reset.await.unwrap(), Reason::CANCEL);
                assert_eq!(other.data().await.unwrap().unwrap(), "history event");
                assert!(other.data().await.is_none());
                assert_eq!(other.trailers().await.unwrap().unwrap()["grpc-status"], "0");
            } => {}
        }
        assert!(!client.is_finished());
        assert!(!proxy.is_finished());
        drop(sender);
        proxy.abort();
        client.abort();
        worker.abort();
    }
}
