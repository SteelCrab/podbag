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
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, Weak};

pub(super) const SOLVE: &str = "/moby.buildkit.v1.Control/Solve";
pub(super) const FILE_SEND: &str = "/moby.filesync.v1.FileSend/DiffCopy";
const EXPORTER_ID: &str = "buildkit-attachable-exporter-id";
const NEW_SESSION: &str = "start a new BuildKit session after an incomplete export";

#[derive(Clone, Default)]
pub(super) struct Exports(Arc<Mutex<HashMap<String, Session>>>);

enum Session {
    // An explicit file exporter can use the same ID that an unrelated Solve
    // would assign to a local load. Do not allow those Solves to overlap.
    Forward { active: usize, incomplete: bool },
    Load { index: usize, archive: Archive },
    // BuildKit can resolve a session ID again after reconnecting. Keep this
    // entry for the worker generation, rather than reopening it on disconnect.
    Blocked,
}

enum FileSend {
    Forward,
    Archive(Archive),
    Reject(&'static str),
}

#[derive(Clone, Default)]
struct Archive(Arc<Mutex<ArchiveState>>);

#[derive(Default)]
enum ArchiveState {
    #[default]
    Waiting,
    Receiving,
    Complete(StagedArchive),
    Closed,
}

struct Lease {
    exports: Exports,
    id: String,
    archive: Option<Archive>,
    complete: bool,
}

impl Exports {
    fn reserve(&self, id: String, index: Option<usize>) -> io::Result<Lease> {
        use std::collections::hash_map::Entry;
        // BuildKit's SessionManager.Get strips a nonempty session prefix.
        let id = match id.split_once(':') {
            Some((_, suffix)) if !suffix.is_empty() => suffix.to_owned(),
            _ => id,
        };
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
                    entry.insert(Session::Forward {
                        active: 1,
                        incomplete: false,
                    });
                }
            }
            Entry::Occupied(mut entry) => match entry.get_mut() {
                Session::Forward { active, .. } if index.is_none() => *active += 1,
                Session::Blocked
                | Session::Forward {
                    incomplete: true, ..
                } => {
                    return Err(io::Error::other(NEW_SESSION));
                }
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
            complete: false,
        })
    }

    fn file_send(&self, id: &str, index: Option<usize>) -> FileSend {
        match self.0.lock().unwrap().get(id) {
            Some(Session::Load {
                index: expected,
                archive,
            }) => match index {
                Some(index) if index == *expected => FileSend::Archive(archive.clone()),
                Some(_) => FileSend::Forward,
                None => FileSend::Reject("BuildKit FileSend must provide one exporter ID"),
            },
            Some(Session::Blocked) => FileSend::Reject(NEW_SESSION),
            _ => FileSend::Forward,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(archive) = &self.archive {
            archive.close();
        }
        let mut sessions = self.exports.0.lock().unwrap();
        match sessions.get_mut(&self.id) {
            Some(Session::Forward { active, incomplete }) => {
                *active -= 1;
                // Late explicit FileSend requests still go to their client. They
                // must never be mistaken for an Engine archive on this session.
                *incomplete |= !self.complete;
                if *active == 0 && !*incomplete {
                    sessions.remove(&self.id);
                }
            }
            Some(Session::Load { .. }) if !self.complete => {
                sessions.insert(self.id.clone(), Session::Blocked);
            }
            _ => {
                sessions.remove(&self.id);
            }
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
        if attrs.get("tar").is_some_and(|value| {
            !matches!(value.to_ascii_lowercase().as_str(), "" | "true" | "1" | "t")
        }) {
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
    let mut lease = if !fields.session.is_empty()
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
        let complete = grpc::forward_response(reply, response).await?;
        if let Some(lease) = &mut lease {
            lease.complete = complete;
        }
        return Ok(());
    };
    if response.body().is_end_stream() {
        grpc::forward_response(reply, response).await?;
        return Ok(());
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
    if grpc::success(&response, trailers.as_ref()) {
        let staged = match archive.take_complete() {
            Ok(staged) => staged,
            Err(error) => return grpc::status(reply, "13", &error.to_string()),
        };
        // BuildKit finishes FileSend and observes its success before sending
        // a successful Solve response. There are no outstanding archive
        // sends once both results have been received.
        lease.as_mut().unwrap().complete = true;
        if let Err(error) = staged.load().await {
            return grpc::status(reply, "13", &error.to_string());
        }
    }
    let (headers, _) = response.into_parts();
    let mut send = reply
        .send_response(::http::Response::from_parts(headers, ()), false)
        .map_err(io::Error::other)?;
    grpc::send_data(&mut send, data.freeze()).await?;
    grpc::finish(&mut send, trailers)
}

impl Archive {
    fn begin_receive(&self) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        if !matches!(*state, ArchiveState::Waiting) {
            return Err(io::Error::other("duplicate BuildKit image archive"));
        }
        *state = ArchiveState::Receiving;
        Ok(())
    }

    fn finish_receive(&self, archive: StagedArchive) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        if !matches!(*state, ArchiveState::Receiving) {
            return Err(io::Error::other("the BuildKit image archive session ended"));
        }
        *state = ArchiveState::Complete(archive);
        Ok(())
    }

    fn take_complete(&self) -> io::Result<StagedArchive> {
        match std::mem::replace(&mut *self.0.lock().unwrap(), ArchiveState::Closed) {
            ArchiveState::Complete(archive) => Ok(archive),
            _ => Err(io::Error::other(
                "BuildKit did not send the Engine image archive",
            )),
        }
    }

    fn close(&self) {
        *self.0.lock().unwrap() = ArchiveState::Closed;
    }

    async fn receive(&self, mut body: RecvStream) -> io::Result<()> {
        self.begin_receive()?;
        let dirs = crate::paths::dirs().map_err(io::Error::other)?;
        let limit = crate::config::load(&dirs)
            .map_err(io::Error::other)?
            .store_limit;
        let mut archive = StagedArchive::new(&dirs.data, limit)?;
        let mut buffer = BytesMut::new();
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
                archive.append(&data)?;
            }
        }
        if !buffer.is_empty() || archive.written == 0 {
            return Err(io::Error::other("incomplete BuildKit image archive"));
        }
        self.finish_receive(archive)
    }
}

struct StagedArchive {
    // Field drop order keeps the reservation until the file has closed.
    file: Arc<File>,
    reservation: Reservation,
    limit: u64,
    written: u64,
    fragment_size: u64,
}

#[derive(Default)]
struct SpaceBudget {
    files: HashMap<usize, ReservedFile>,
    next_id: usize,
    pending: u64,
}

struct ReservedFile {
    file: Weak<File>,
    bytes: u64,
    pending: u64,
}

impl ReservedFile {
    fn refresh(&mut self) {
        let allocated = self
            .file
            .upgrade()
            .and_then(|file| file.metadata().ok())
            .map_or(0, |info| info.blocks().saturating_mul(512));
        self.pending = self.bytes.saturating_sub(allocated);
    }
}

struct Reservation {
    budget: Arc<Mutex<SpaceBudget>>,
    id: usize,
}

static ARCHIVE_BUDGETS: LazyLock<Mutex<HashMap<u64, Weak<Mutex<SpaceBudget>>>>> =
    LazyLock::new(Mutex::default);

impl StagedArchive {
    async fn load(self) -> io::Result<()> {
        tokio::task::spawn_blocking(move || {
            let cleanup =
                crate::helper::load_staged_archive(self.file.try_clone()?).map_err(|error| {
                    io::Error::other(format!("cannot load the BuildKit image: {error}"))
                })?;
            if let Some(error) = cleanup {
                eprintln!("podbag: BuildKit image loaded. Image store maintenance failed: {error}");
            }
            // Keep the file and its reservation until the helper exits.
            drop(self);
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    fn new(dir: &Path, limit: u64) -> io::Result<Self> {
        let file = Arc::new(tempfile::tempfile_in(dir)?);
        let device = file.metadata()?.dev();
        let fs = rustix::fs::fstatvfs(&*file)?;
        let mut devices = ARCHIVE_BUDGETS.lock().unwrap();
        devices.retain(|_, budget| budget.strong_count() != 0);
        let entry = devices.entry(device).or_default();
        let budget = entry.upgrade().unwrap_or_else(|| {
            let budget = Arc::new(Mutex::new(SpaceBudget::default()));
            *entry = Arc::downgrade(&budget);
            budget
        });
        drop(devices);
        let reservation = Reservation::new(budget, &file);
        Ok(Self {
            file,
            reservation,
            written: 0,
            limit,
            fragment_size: fs.f_frsize.max(1),
        })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let written = self.written.saturating_add(bytes.len() as u64);
        if written > self.limit {
            return Err(space_error());
        }
        let rounded = written
            .div_ceil(self.fragment_size)
            .saturating_mul(self.fragment_size);
        let mut budget = self.reservation.budget.lock().unwrap();
        let id = self.reservation.id;
        budget.reserve(id, rounded, || {
            Ok(usable_space(&rustix::fs::fstatvfs(&*self.file)?))
        })?;
        (&*self.file).write_all(bytes)?;
        budget.refresh_file(id);
        self.written = written;
        Ok(())
    }
}

impl SpaceBudget {
    fn reserve(
        &mut self,
        id: usize,
        total: u64,
        free: impl Fn() -> io::Result<u64>,
    ) -> io::Result<()> {
        let extra = total - self.files[&id].bytes;
        if extra == 0 {
            return Ok(());
        }
        let mut available = free()?;
        if extra <= available && extra > available.saturating_sub(self.pending) {
            // Another file's delayed allocation may now be reflected. Only
            // scan the other files under pressure. Sample free space again
            // *after* the file stats, so newer allocations count against us.
            self.pending = self
                .files
                .values_mut()
                .map(|file| {
                    file.refresh();
                    file.pending
                })
                .sum();
            available = free()?;
        }
        if extra > available.saturating_sub(self.pending) {
            return Err(space_error());
        }
        let file = self.files.get_mut(&id).unwrap();
        file.bytes = total;
        file.pending += extra;
        self.pending += extra;
        Ok(())
    }

    fn refresh_file(&mut self, id: usize) {
        let file = self.files.get_mut(&id).unwrap();
        self.pending -= file.pending;
        file.refresh();
        self.pending += file.pending;
    }
}

impl Reservation {
    fn new(budget: Arc<Mutex<SpaceBudget>>, file: &Arc<File>) -> Self {
        let id = {
            let mut budget = budget.lock().unwrap();
            let id = budget.next_id;
            budget.next_id += 1;
            budget.files.insert(
                id,
                ReservedFile {
                    file: Arc::downgrade(file),
                    bytes: 0,
                    pending: 0,
                },
            );
            id
        };
        Self { budget, id }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut budget = self.budget.lock().unwrap();
        if let Some(file) = budget.files.remove(&self.id) {
            budget.pending -= file.pending;
        }
    }
}

fn usable_space(fs: &rustix::fs::StatVfs) -> u64 {
    fs.f_bavail
        .saturating_mul(fs.f_frsize)
        .saturating_sub(crate::store::DISK_RESERVE)
}

fn space_error() -> io::Error {
    io::Error::other("the BuildKit archive exceeds available image store space")
}

pub(super) async fn session(
    upstream: SendRequest<Bytes>,
    request: Request<RecvStream>,
    reply: SendResponse<Bytes>,
    exports: Exports,
    session: String,
) -> io::Result<()> {
    if request.uri().path() == FILE_SEND {
        let mut headers = request.headers().get_all(EXPORTER_ID).iter();
        let index = headers
            .next()
            .and_then(|value| value.to_str().ok()?.parse().ok());
        let index = index.filter(|_| headers.next().is_none());
        match exports.file_send(&session, index) {
            FileSend::Forward => {}
            FileSend::Archive(archive) => {
                return match archive.receive(request.into_body()).await {
                    Ok(()) => grpc::status(reply, "0", ""),
                    Err(error) => grpc::status(reply, "13", &error.to_string()),
                };
            }
            FileSend::Reject(message) => return grpc::status(reply, "9", message),
        }
    }
    grpc::forward(upstream, request, reply).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_files_on_the_same_device_share_a_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let first = StagedArchive::new(tmp.path(), u64::MAX).unwrap();
        let second = StagedArchive::new(tmp.path(), u64::MAX).unwrap();
        assert!(Arc::ptr_eq(
            &first.reservation.budget,
            &second.reservation.budget
        ));
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn the_budget_refreshes_free_space_and_keeps_pending_reservations() {
        let budget = Arc::new(Mutex::new(SpaceBudget::default()));
        let first_file = Arc::new(tempfile::tempfile().unwrap());
        let first = Reservation::new(budget.clone(), &first_file);
        let second_file = Arc::new(tempfile::tempfile().unwrap());
        let second = Reservation::new(budget.clone(), &second_file);
        let mut shared = budget.lock().unwrap();

        shared
            .reserve(first.id, 128 * MIB, || Ok(256 * MIB))
            .unwrap();
        assert!(
            shared
                .reserve(second.id, 300 * MIB, || Ok(256 * MIB))
                .is_err()
        );
        shared
            .reserve(second.id, 300 * MIB, || Ok(2304 * MIB))
            .unwrap();
        assert!(
            shared
                .reserve(second.id, 2200 * MIB, || Ok(2304 * MIB))
                .is_err()
        );
        drop(shared);
        drop(first_file);
        drop(first);

        budget
            .lock()
            .unwrap()
            .reserve(second.id, 2200 * MIB, || Ok(2304 * MIB))
            .unwrap();
    }

    #[test]
    fn the_budget_rechecks_delayed_allocations_before_declining_a_write() {
        use std::cell::Cell;

        let budget = Arc::new(Mutex::new(SpaceBudget::default()));
        let written = Arc::new(tempfile::tempfile().unwrap());
        let first = Reservation::new(budget.clone(), &written);
        let empty = Arc::new(tempfile::tempfile().unwrap());
        let second = Reservation::new(budget.clone(), &empty);
        let mut shared = budget.lock().unwrap();

        shared.reserve(first.id, 2 * MIB, || Ok(3 * MIB)).unwrap();
        (&*written).write_all(&vec![0x53; MIB as usize]).unwrap();
        written.sync_data().unwrap();
        let allocated = written.metadata().unwrap().blocks() * 512;
        assert!(allocated > 0 && allocated <= 2 * MIB);

        // This is the old free-space snapshot, followed by a newer one in
        // which the first file's allocation is reflected. Its pending bytes
        // still count after refreshing this snapshot.
        let reads = Cell::new(0);
        let free = || {
            let count = reads.get();
            reads.set(count + 1);
            Ok(if count == 0 {
                2 * MIB
            } else {
                3 * MIB - allocated
            })
        };
        shared.reserve(second.id, MIB, free).unwrap();
        assert_eq!(reads.get(), 2);
        assert_eq!(shared.pending, 3 * MIB - allocated);
        assert!(
            shared
                .reserve(second.id, MIB + 1, || Ok(3 * MIB - allocated))
                .is_err()
        );
    }

    #[test]
    fn simultaneous_archives_share_the_filesystem_reserve_until_drop() {
        let free = 1536 * MIB;
        let available = free - crate::store::DISK_RESERVE;
        let budget = Arc::new(Mutex::new(SpaceBudget::default()));
        let ready = std::sync::Barrier::new(2);
        let attempted = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let receive = || {
                let file = Arc::new(tempfile::tempfile().unwrap());
                let reservation = Reservation::new(budget.clone(), &file);
                ready.wait();
                let accepted = budget
                    .lock()
                    .unwrap()
                    .reserve(reservation.id, 700 * MIB, || Ok(available))
                    .is_ok();
                attempted.wait();
                drop(file);
                drop(reservation);
                accepted
            };
            let first = scope.spawn(receive);
            let second = scope.spawn(receive);
            assert_ne!(first.join().unwrap(), second.join().unwrap());
        });
        assert_eq!(budget.lock().unwrap().pending, 0);
        let file = Arc::new(tempfile::tempfile().unwrap());
        let next = Reservation::new(budget.clone(), &file);
        budget
            .lock()
            .unwrap()
            .reserve(next.id, 700 * MIB, || Ok(available))
            .unwrap();
    }

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
    fn an_empty_tar_value_selects_the_docker_archive() {
        let mut fields = SolveFields {
            session: "client".into(),
            exporters: vec![Exporter {
                kind: "moby".into(),
                attrs: HashMap::from([("tar".into(), String::new())]),
            }],
            ..Default::default()
        };
        assert_eq!(fields.load_index().unwrap(), Some(0));
        fields.exporters[0]
            .attrs
            .insert("tar".into(), "false".into());
        assert!(fields.load_index().is_err());
        fields.deprecated_kind = "moby".into();
        fields.deprecated_attrs.insert("tar".into(), String::new());
        fields.exporters.clear();
        assert_eq!(fields.load_index().unwrap(), Some(0));
    }

    #[test]
    fn a_taken_archive_stays_closed_to_new_receives() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = Archive::default();
        archive.begin_receive().unwrap();
        archive
            .finish_receive(StagedArchive::new(tmp.path(), u64::MAX).unwrap())
            .unwrap();
        let staged = archive.take_complete().unwrap();
        assert!(archive.begin_receive().is_err());
        assert!(archive.take_complete().is_err());
        drop(staged);
        assert!(archive.begin_receive().is_err());
    }

    #[test]
    fn a_clean_solve_can_reuse_the_same_session() {
        let exports = Exports::default();
        let mut forward1 = exports.reserve("context:client".into(), None).unwrap();
        let mut forward2 = exports.reserve("client".into(), None).unwrap();
        assert!(exports.reserve("client".into(), Some(0)).is_err());
        forward1.complete = true;
        drop(forward1);
        assert!(exports.reserve("client".into(), Some(0)).is_err());
        forward2.complete = true;
        drop(forward2);

        let mut load = exports.reserve("context:client".into(), Some(1)).unwrap();
        assert!(exports.reserve("client".into(), None).is_err());
        assert!(exports.reserve("client".into(), Some(2)).is_err());
        assert!(matches!(
            exports.file_send("client", Some(0)),
            FileSend::Forward
        ));
        assert!(matches!(
            exports.file_send("other", Some(1)),
            FileSend::Forward
        ));
        assert!(matches!(
            exports.file_send("client", Some(1)),
            FileSend::Archive(_)
        ));
        assert!(matches!(
            exports.file_send("client", None),
            FileSend::Reject(_)
        ));
        load.complete = true;
        drop(load);
        assert!(exports.reserve("client".into(), Some(1)).is_ok());
    }

    #[test]
    fn a_cancelled_load_keeps_old_uploads_out_of_future_solves() {
        let exports = Exports::default();
        let load = exports.reserve("client".into(), Some(0)).unwrap();
        let FileSend::Archive(archive) = exports.file_send("client", Some(0)) else {
            panic!("expected the load's archive");
        };
        archive.begin_receive().unwrap();
        drop(load);

        let tmp = tempfile::tempdir().unwrap();
        assert!(
            archive
                .finish_receive(StagedArchive::new(tmp.path(), u64::MAX).unwrap())
                .is_err()
        );
        assert!(archive.begin_receive().is_err());
        assert!(matches!(
            exports.file_send("client", Some(0)),
            FileSend::Reject(_)
        ));
        assert!(exports.reserve("client".into(), Some(0)).is_err());
        assert!(exports.reserve("context:client".into(), None).is_err());
        assert!(exports.reserve("replacement".into(), Some(0)).is_ok());
    }

    #[test]
    fn an_incomplete_forward_never_becomes_an_engine_archive() {
        let exports = Exports::default();
        let forward = exports.reserve("client".into(), None).unwrap();
        let mut other = exports.reserve("client".into(), None).unwrap();
        drop(forward);
        other.complete = true;
        drop(other);

        assert!(matches!(
            exports.file_send("client", Some(0)),
            FileSend::Forward
        ));
        assert!(exports.reserve("client".into(), Some(0)).is_err());
        assert!(exports.reserve("client".into(), None).is_ok());
    }
}
