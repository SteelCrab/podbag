//! Docker's "moby" exporter becomes a native BuildKit Docker archive, imported
//! into this Engine only after the worker reports a successful Solve.
use super::{MAX_MESSAGE, SessionBytes, grpc};
use ::http::Request;
use bytes::{Bytes, BytesMut};
use h2::{RecvStream, client::SendRequest, server::SendResponse};
use prost::{
    Message,
    encoding::{DecodeContext, WireType, decode_key, decode_varint, skip_field},
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::sync::{Arc, Mutex};

pub(super) const SOLVE: &str = "/moby.buildkit.v1.Control/Solve";
pub(super) const FILE_SEND: &str = "/moby.filesync.v1.FileSend/DiffCopy";
const EXPORTER_ID: &str = "buildkit-attachable-exporter-id";

#[derive(Clone, Default)]
pub(super) struct Exports(Arc<Mutex<HashMap<String, Session>>>);

enum Session {
    // An explicit file exporter can use the same ID that an unrelated Solve
    // would assign to a local load. Do not allow those Solves to overlap.
    Forward(usize),
    Load { index: usize, archive: Archive },
}

#[derive(Clone, Default)]
struct Archive(Arc<Mutex<ArchiveState>>);

#[derive(Default)]
enum ArchiveState {
    #[default]
    Waiting,
    Receiving,
    Complete(File),
}

struct Lease {
    exports: Exports,
    id: String,
    archive: Option<Archive>,
}

impl Exports {
    fn reserve(&self, id: String, index: Option<usize>) -> io::Result<Lease> {
        use std::collections::hash_map::Entry;
        let mut sessions = self.0.lock().unwrap();
        let mut archive = None;
        match sessions.entry(id.clone()) {
            Entry::Vacant(entry) => {
                if let Some(index) = index {
                    let load = Archive::default();
                    archive = Some(load.clone());
                    entry.insert(Session::Load {
                        index,
                        archive: load,
                    });
                } else {
                    entry.insert(Session::Forward(1));
                }
            }
            Entry::Occupied(mut entry) => match entry.get_mut() {
                Session::Forward(count) if index.is_none() => *count += 1,
                _ => {
                    return Err(io::Error::other(
                        "BuildKit cannot share a session with a concurrent Engine image load",
                    ));
                }
            },
        }
        Ok(Lease {
            exports: self.clone(),
            id,
            archive,
        })
    }

    fn archive(&self, id: &str, index: usize) -> Option<Archive> {
        match self.0.lock().unwrap().get(id)? {
            Session::Load {
                index: expected,
                archive,
            } if *expected == index => Some(archive.clone()),
            _ => None,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut sessions = self.exports.0.lock().unwrap();
        if let Some(Session::Forward(count)) = sessions.get_mut(&self.id)
            && *count > 1
        {
            *count -= 1;
        } else {
            sessions.remove(&self.id);
        }
    }
}

#[derive(Message)]
struct Exporter {
    #[prost(string, tag = "1")]
    kind: String,
    #[prost(map = "string, string", tag = "2")]
    attrs: HashMap<String, String>,
}

#[derive(Message)]
struct SolveFields {
    #[prost(string, tag = "3")]
    deprecated_kind: String,
    #[prost(map = "string, string", tag = "4")]
    deprecated_attrs: HashMap<String, String>,
    #[prost(string, tag = "5")]
    session: String,
    #[prost(message, repeated, tag = "13")]
    exporters: Vec<Exporter>,
}

impl SolveFields {
    fn load_index(&self) -> io::Result<Option<usize>> {
        let mut matches = self
            .exporters
            .iter()
            .enumerate()
            .filter(|(_, exporter)| exporter.kind == "moby");
        let found = matches.next();
        if matches.next().is_some() {
            return Err(io::Error::other(
                "only one Engine image load is supported per BuildKit Solve",
            ));
        }
        let (index, attrs) = match found {
            Some((index, exporter)) => (index, &exporter.attrs),
            None if self.exporters.is_empty() && self.deprecated_kind == "moby" => {
                (0, &self.deprecated_attrs)
            }
            _ => return Ok(None),
        };
        if self.session.is_empty() {
            return Err(io::Error::other(
                "an Engine image load requires a BuildKit session",
            ));
        }
        if attrs.get("push").is_some_and(|value| {
            !matches!(
                value.to_ascii_lowercase().as_str(),
                "false" | "0" | "f" | ""
            )
        }) {
            return Err(io::Error::other(
                "Buildx Engine output cannot push directly; build the image, then use podbag push",
            ));
        }
        if attrs
            .get("tar")
            .is_some_and(|value| !matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "t"))
        {
            return Err(io::Error::other(
                "Engine image loads require a Docker archive",
            ));
        }
        Ok(Some(index))
    }
}

/// Preserve the entire request and exporter messages, including fields added
/// in later BuildKit releases. Change only the selected exporter's type.
fn rewrite(
    mut input: &[u8],
    index: usize,
    deprecated: bool,
) -> Result<Vec<u8>, prost::DecodeError> {
    let mut output = Vec::with_capacity(input.len() + 2);
    let mut exporter = 0;
    while !input.is_empty() {
        let field = input;
        let (tag, wire) = decode_key(&mut input)?;
        skip_field(wire, tag, &mut input, DecodeContext::default())?;
        let span = &field[..field.len() - input.len()];
        if tag == 3 && deprecated && wire == WireType::LengthDelimited {
            prost::encoding::string::encode(3, &"docker".to_owned(), &mut output);
        } else if tag == 13 && !deprecated && wire == WireType::LengthDelimited {
            if exporter == index {
                let mut nested = span;
                decode_key(&mut nested)?;
                decode_varint(&mut nested)?;
                let nested = rewrite_kind(nested)?;
                prost::encoding::bytes::encode(13, &nested, &mut output);
            } else {
                output.extend_from_slice(span);
            }
            exporter += 1;
        } else {
            output.extend_from_slice(span);
        }
    }
    Ok(output)
}

fn rewrite_kind(mut input: &[u8]) -> Result<Vec<u8>, prost::DecodeError> {
    let mut output = Vec::with_capacity(input.len() + 2);
    while !input.is_empty() {
        let field = input;
        let (tag, wire) = decode_key(&mut input)?;
        skip_field(wire, tag, &mut input, DecodeContext::default())?;
        if tag == 1 && wire == WireType::LengthDelimited {
            prost::encoding::string::encode(1, &"docker".to_owned(), &mut output);
        } else {
            output.extend_from_slice(&field[..field.len() - input.len()]);
        }
    }
    Ok(output)
}

pub(super) async fn solve(
    upstream: SendRequest<Bytes>,
    request: Request<RecvStream>,
    mut reply: SendResponse<Bytes>,
    exports: Exports,
) -> io::Result<()> {
    let (mut headers, mut body) = request.into_parts();
    let mut frame = match grpc::unary(&mut body).await {
        Ok(frame) => frame,
        Err(error) => return grpc::status(reply, "3", &error.to_string()),
    };
    let original = frame.clone().freeze();
    let Ok(Some((compressed, raw))) = grpc::read_frame(&mut frame) else {
        return grpc::status(reply, "3", "invalid BuildKit Solve frame");
    };
    if !frame.is_empty() {
        return grpc::status(reply, "3", "unexpected BuildKit Solve messages");
    }
    let payload = if compressed {
        if headers
            .headers
            .get("grpc-encoding")
            .is_none_or(|value| value != "gzip")
        {
            return grpc::status(
                reply,
                "12",
                "Podbag supports gzip-compressed BuildKit Solve messages",
            );
        }
        let mut buffer = Vec::new();
        if flate2::read::GzDecoder::new(raw.as_ref())
            .take((MAX_MESSAGE + 1) as u64)
            .read_to_end(&mut buffer)
            .is_err()
        {
            return grpc::status(reply, "3", "invalid gzip-compressed BuildKit Solve message");
        }
        if buffer.len() > MAX_MESSAGE {
            return grpc::status(reply, "8", "BuildKit Solve message is too large");
        }
        Bytes::from(buffer)
    } else {
        raw
    };
    let fields = match SolveFields::decode(payload.clone()) {
        Ok(fields) => fields,
        Err(_) => return grpc::status(reply, "3", "invalid BuildKit Solve fields"),
    };
    let index = match fields.load_index() {
        Ok(index) => index,
        Err(error) => return grpc::status(reply, "12", &error.to_string()),
    };
    let outgoing = if let Some(index) = index {
        headers.headers.remove("content-length");
        let rewritten =
            rewrite(&payload, index, fields.exporters.is_empty()).map_err(io::Error::other)?;
        // A gRPC message may be sent uncompressed even when the stream advertises gzip.
        grpc::frame(&rewritten, false)
    } else {
        original
    };
    let lease = if !fields.session.is_empty()
        && (!fields.exporters.is_empty() || !fields.deprecated_kind.is_empty())
    {
        match exports.reserve(fields.session, index) {
            Ok(lease) => Some(lease),
            Err(error) => return grpc::status(reply, "9", &error.to_string()),
        }
    } else {
        None
    };

    let (response, mut upload) = upstream
        .ready()
        .await
        .map_err(io::Error::other)?
        .send_request(Request::from_parts(headers, ()), false)
        .map_err(io::Error::other)?;
    grpc::send_data(&mut upload, outgoing).await?;
    grpc::finish(
        &mut upload,
        body.trailers().await.map_err(io::Error::other)?,
    )?;
    let mut response = tokio::select! {
        result = response => result.map_err(io::Error::other)?,
        _ = std::future::poll_fn(|cx| reply.poll_reset(cx)) => {
            upload.send_reset(h2::Reason::CANCEL);
            return Ok(());
        }
    };
    let Some(archive) = lease.as_ref().and_then(|lease| lease.archive.as_ref()) else {
        return grpc::forward_response(reply, response).await;
    };
    if response.body().is_end_stream() {
        return grpc::forward_response(reply, response).await;
    }
    let completed = async {
        let data = grpc::unary(response.body_mut()).await?;
        let trailers = response
            .body_mut()
            .trailers()
            .await
            .map_err(io::Error::other)?;
        Ok::<_, io::Error>((data, trailers))
    };
    let (data, trailers) = tokio::select! {
        biased;
        _ = std::future::poll_fn(|cx| reply.poll_reset(cx)) => {
            upload.send_reset(h2::Reason::CANCEL);
            return Ok(());
        }
        result = completed => result?,
    };
    if grpc::success(&response, trailers.as_ref())
        && let Err(error) = archive.load().await
    {
        return grpc::status(reply, "13", &error.to_string());
    }
    let (headers, _) = response.into_parts();
    let mut send = reply
        .send_response(::http::Response::from_parts(headers, ()), false)
        .map_err(io::Error::other)?;
    grpc::send_data(&mut send, data.freeze()).await?;
    grpc::finish(&mut send, trailers)
}

impl Archive {
    async fn load(&self) -> io::Result<()> {
        let archive = std::mem::take(&mut *self.0.lock().unwrap());
        let ArchiveState::Complete(mut file) = archive else {
            return Err(io::Error::other(
                "BuildKit did not send the Engine image archive",
            ));
        };
        tokio::task::spawn_blocking(move || {
            file.rewind()?;
            crate::Image::load(&mut file).map(|_| ()).map_err(|error| {
                io::Error::other(format!("cannot load the BuildKit image: {error}"))
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn receive(&self, mut body: RecvStream) -> io::Result<()> {
        {
            let mut state = self.0.lock().unwrap();
            if !matches!(*state, ArchiveState::Waiting) {
                return Err(io::Error::other("duplicate BuildKit image archive"));
            }
            *state = ArchiveState::Receiving;
        }
        let dirs = crate::paths::dirs().map_err(io::Error::other)?;
        let limit = crate::config::load(&dirs)
            .map_err(io::Error::other)?
            .store_limit;
        let mut file = tempfile::tempfile_in(&dirs.data)?;
        let free = rustix::fs::fstatvfs(&file)?;
        let capacity = limit.min(
            free.f_bavail
                .saturating_mul(free.f_frsize)
                .saturating_sub(512 << 20),
        );
        let mut buffer = BytesMut::new();
        let mut written = 0u64;
        while let Some(chunk) = grpc::data(&mut body).await? {
            buffer.extend_from_slice(&chunk);
            while let Some((compressed, message)) = grpc::read_frame(&mut buffer)? {
                if compressed {
                    return Err(io::Error::other(
                        "compressed BuildKit archive frames are unsupported",
                    ));
                }
                let data = SessionBytes::decode(message)
                    .map_err(io::Error::other)?
                    .data;
                written = written.saturating_add(data.len() as u64);
                if written > capacity {
                    return Err(io::Error::other(
                        "the BuildKit archive exceeds available image store space",
                    ));
                }
                file.write_all(&data)?;
            }
        }
        if !buffer.is_empty() || written == 0 {
            return Err(io::Error::other("incomplete BuildKit image archive"));
        }
        *self.0.lock().unwrap() = ArchiveState::Complete(file);
        Ok(())
    }
}

pub(super) async fn session(
    upstream: SendRequest<Bytes>,
    request: Request<RecvStream>,
    reply: SendResponse<Bytes>,
    exports: Exports,
    session: String,
) -> io::Result<()> {
    if request.uri().path() == FILE_SEND
        && let Some(index) = request
            .headers()
            .get(EXPORTER_ID)
            .and_then(|value| value.to_str().ok()?.parse().ok())
        && let Some(archive) = exports.archive(&session, index)
    {
        return match archive.receive(request.into_body()).await {
            Ok(()) => grpc::status(reply, "0", ""),
            Err(error) => grpc::status(reply, "13", &error.to_string()),
        };
    }
    grpc::forward(upstream, request, reply).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_only_the_selected_exporter_type() {
        let fields = SolveFields {
            session: "session".into(),
            exporters: vec![
                Exporter {
                    kind: "local".into(),
                    attrs: HashMap::new(),
                },
                Exporter {
                    kind: "moby".into(),
                    attrs: HashMap::from([("name".into(), "example:loaded".into())]),
                },
            ],
            ..Default::default()
        };
        let mut input = fields.encode_to_vec();
        // A field not interpreted by Podbag, such as an LLB definition.
        let unknown = [0x12, 0x03, 0x00, 0xff, 0xff];
        input.extend_from_slice(&unknown);
        let result = rewrite(&input, fields.load_index().unwrap().unwrap(), false).unwrap();
        assert!(result.ends_with(&unknown));
        let updated = SolveFields::decode(result.as_slice()).unwrap();
        assert_eq!(updated.session, fields.session);
        assert_eq!(updated.exporters[0].kind, "local");
        assert_eq!(updated.exporters[1].kind, "docker");
        assert_eq!(updated.exporters[1].attrs, fields.exporters[1].attrs);
    }

    #[test]
    fn changes_the_legacy_exporter_without_dropping_its_attributes() {
        let fields = SolveFields {
            deprecated_kind: "moby".into(),
            deprecated_attrs: HashMap::from([("name".into(), "example:legacy".into())]),
            session: "session".into(),
            ..Default::default()
        };
        let result = rewrite(
            &fields.encode_to_vec(),
            fields.load_index().unwrap().unwrap(),
            true,
        )
        .unwrap();
        let updated = SolveFields::decode(result.as_slice()).unwrap();
        assert_eq!(updated.deprecated_kind, "docker");
        assert_eq!(updated.deprecated_attrs, fields.deprecated_attrs);
    }

    #[test]
    fn reserves_one_archive_for_the_exact_session_and_exporter() {
        let exports = Exports::default();
        let forward1 = exports.reserve("client".into(), None).unwrap();
        let forward2 = exports.reserve("client".into(), None).unwrap();
        assert!(exports.reserve("client".into(), Some(0)).is_err());
        drop(forward1);
        assert!(exports.reserve("client".into(), Some(0)).is_err());
        drop(forward2);
        let load = exports.reserve("client".into(), Some(1)).unwrap();
        assert!(exports.reserve("client".into(), None).is_err());
        assert!(exports.reserve("client".into(), Some(2)).is_err());
        assert!(exports.archive("client", 0).is_none());
        assert!(exports.archive("other", 1).is_none());
        assert!(exports.archive("client", 1).is_some());
        drop(load);
        assert!(exports.archive("client", 1).is_none());
        assert!(exports.reserve("client".into(), None).is_ok());
    }
}
