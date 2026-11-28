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
        forward_response(reply, response).await
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

pub(super) async fn forward_response(
    mut reply: SendResponse<Bytes>,
    response: Response<RecvStream>,
) -> io::Result<()> {
    let (headers, body) = response.into_parts();
    let end = body.is_end_stream();
    let send = reply
        .send_response(Response::from_parts(headers, ()), end)
        .map_err(io::Error::other)?;
    if end {
        return Ok(());
    }
    copy(body, send).await
}

async fn copy(mut body: RecvStream, mut send: SendStream<Bytes>) -> io::Result<()> {
    while let Some(chunk) = data(&mut body).await? {
        send_data(&mut send, chunk).await?;
    }
    finish(&mut send, body.trailers().await.map_err(io::Error::other)?)
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
        && trailers
            .and_then(|headers| headers.get("grpc-status"))
            .is_some_and(|status| status == "0")
}
