//! Docker's multiplexed stream, and clients attached to containers that have not started.

use crate::helper::Io;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

/// A frame of Docker's multiplexed stream: an 8-byte header (stream 1 stdout, 2 stderr;
/// big-endian length), then the bytes.
pub fn frame(stream_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&[stream_id, 0, 0, 0]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Which of the container's streams a client wants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wanted {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

/// A client attached to a container that has not started (yet).
pub struct Attachment {
    /// The client's connection (a clone: its request thread keeps the original).
    pub socket: UnixStream,
    pub wanted: Wanted,
    /// Bytes the client sent after its request: the start of its stdin.
    pub pending: Vec<u8>,
    /// Told when the container has ended and the connection is closed.
    pub done: mpsc::Sender<()>,
    token: u64,
}

/// How an attached start by this service ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Running,
    Code(i32),
}

/// Clients waiting for a container's start, and the ends of the starts this service made
/// (a `--rm` container is gone before its exit code can be read anywhere else).
#[derive(Default)]
pub struct Attachments {
    waiting: Mutex<HashMap<String, Vec<Attachment>>>,
    exits: Mutex<HashMap<String, (u64, Exit)>>,
    counter: AtomicU64,
    /// Counts the starts that began, ended or failed: a client waiting for a container
    /// wakes at once (a `--rm` container is removed before its start ends).
    changes: Mutex<u64>,
    changed: Condvar,
}

impl Attachments {
    fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn lock_waiting(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Attachment>>> {
        self.waiting.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lock_exits(&self) -> std::sync::MutexGuard<'_, HashMap<String, (u64, Exit)>> {
        self.exits.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wait for the next start of `id` by this service. `before` runs first, with the
    /// registry locked (it sends the response head: a start the client sends after that
    /// finds this attachment). Returns a token for [`Self::withdraw`]; None if `before`
    /// failed.
    pub fn add(
        &self,
        id: &str,
        mut socket: UnixStream,
        wanted: Wanted,
        pending: Vec<u8>,
        done: mpsc::Sender<()>,
        before: impl FnOnce(&mut UnixStream) -> std::io::Result<()>,
    ) -> Option<u64> {
        let token = self.next();
        let mut waiting = self.lock_waiting();
        before(&mut socket).ok()?;
        waiting.entry(id.to_string()).or_default().push(Attachment {
            socket,
            wanted,
            pending,
            done,
            token,
        });
        Some(token)
    }

    /// Every client waiting for `id`; the caller starts the container for them.
    pub fn take(&self, id: &str) -> Vec<Attachment> {
        self.lock_waiting().remove(id).unwrap_or_default()
    }

    /// Stop waiting. False: a start took the attachment already.
    pub fn withdraw(&self, id: &str, token: u64) -> bool {
        let mut waiting = self.lock_waiting();
        let Some(list) = waiting.get_mut(id) else {
            return false;
        };
        let before = list.len();
        list.retain(|a| a.token != token);
        let found = list.len() != before;
        if list.is_empty() {
            waiting.remove(id);
        }
        found
    }

    pub fn is_waiting(&self, id: &str, token: u64) -> bool {
        self.lock_waiting()
            .get(id)
            .is_some_and(|l| l.iter().any(|a| a.token == token))
    }

    /// An attached start of `id` begins. Returns its generation.
    pub fn begin(&self, id: &str) -> u64 {
        let generation = self.next();
        self.lock_exits()
            .insert(id.to_string(), (generation, Exit::Running));
        self.bump();
        generation
    }

    /// The start `generation` of `id` ended with `code`.
    pub fn finish(&self, id: &str, generation: u64, code: i32) {
        {
            let mut exits = self.lock_exits();
            if exits.get(id).is_some_and(|(g, _)| *g == generation) {
                exits.insert(id.to_string(), (generation, Exit::Code(code)));
            }
        }
        self.bump();
    }

    /// The start `generation` of `id` failed: it never ran.
    pub fn abandon(&self, id: &str, generation: u64) {
        {
            let mut exits = self.lock_exits();
            if exits.get(id).is_some_and(|(g, _)| *g == generation) {
                exits.remove(id);
            }
        }
        self.bump();
    }

    fn lock_changes(&self) -> std::sync::MutexGuard<'_, u64> {
        self.changes.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self) {
        *self.lock_changes() += 1;
        self.changed.notify_all();
    }

    /// The count of changes, for [`Self::wait_for_change`]: read it before looking.
    pub fn changes(&self) -> u64 {
        *self.lock_changes()
    }

    /// Wait until the count passes `seen`, or `limit` passes. Returns the count.
    pub fn wait_for_change(&self, seen: u64, limit: Duration) -> u64 {
        let guard = self.lock_changes();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, limit, |n| *n == seen)
            .unwrap_or_else(|p| p.into_inner());
        *guard
    }

    /// The newest generation of `id` (0: none), for [`Self::exit_after`].
    pub fn generation(&self, id: &str) -> u64 {
        self.lock_exits().get(id).map_or(0, |(g, _)| *g)
    }

    /// How the last start of `id` ended, if it is newer than `after`.
    pub fn exit_after(&self, id: &str, after: u64) -> Option<Exit> {
        self.lock_exits()
            .get(id)
            .filter(|(g, _)| *g > after)
            .map(|(_, e)| *e)
    }
}

/// One attached client of a running container or exec.
pub struct Sink {
    socket: Mutex<UnixStream>,
    wanted: Wanted,
    /// False after a failed write: the client left; the output still drains.
    alive: AtomicBool,
}

impl Sink {
    pub fn new(socket: UnixStream, wanted: Wanted) -> Sink {
        Sink {
            socket: Mutex::new(socket),
            wanted,
            alive: AtomicBool::new(true),
        }
    }
}

/// Start `handle` attached to `clients`: its output goes to each of them (raw with a
/// terminal, framed without), and the first client that wants stdin feeds it (when the
/// container was created with OpenStdin). Returns once the container runs; threads relay
/// until it ends, then record its exit code and close the clients.
pub fn start_attached(
    registry: &Arc<Attachments>,
    handle: &crate::ContainerHandle,
    dirs: &crate::paths::Dirs,
    mut clients: Vec<Attachment>,
) -> crate::Result<()> {
    let id = handle.id().to_string();
    let fail = |clients: Vec<Attachment>, e: crate::Error| {
        close(clients);
        Err(e)
    };
    let spec = match crate::state::read_config(dirs, &id) {
        Ok(s) => s,
        Err(e) => return fail(clients, e),
    };
    let feeder = if spec.interactive {
        clients.iter().position(|c| c.wanted.stdin)
    } else {
        None
    };
    let pipes = (|| -> std::io::Result<_> {
        let stdin = match feeder {
            Some(_) => Some(std::io::pipe()?),
            None => None,
        };
        let out = std::io::pipe()?;
        let err = if spec.tty {
            None
        } else {
            Some(std::io::pipe()?)
        };
        Ok((stdin, out, err))
    })();
    let (stdin, (out_r, out_w), err) = match pipes {
        Ok(p) => p,
        Err(e) => {
            let e = crate::Error::io("cannot make pipes for the container", &e, "try again");
            return fail(clients, e);
        }
    };
    let (stdin_r, stdin_w) = match stdin {
        Some((r, w)) => (Io::Fd(r.into()), Some(w)),
        None => (Io::Null, None),
    };
    let (err_r, err_w) = match err {
        Some((r, w)) => (Some(r), Io::Fd(w.into())),
        None => (None, Io::Null),
    };
    let generation = registry.begin(&id);
    let running = match handle.start_attached(stdin_r, Io::Fd(out_w.into()), err_w) {
        Ok(r) => r,
        Err(e) => {
            registry.abandon(&id, generation);
            return fail(clients, e);
        }
    };
    if let (Some(i), Some(mut w)) = (feeder, stdin_w) {
        let pending = std::mem::take(&mut clients[i].pending);
        if let Ok(mut from) = clients[i].socket.try_clone() {
            std::thread::spawn(move || {
                if w.write_all(&pending).is_ok() {
                    pump(&mut from, &mut w);
                }
                // Dropping the pipe ends the container's input: the client half-closed.
            });
        }
    }
    let mut dones = Vec::new();
    let mut sinks = Vec::new();
    for c in clients {
        dones.push(c.done);
        sinks.push(Sink::new(c.socket, c.wanted));
    }
    let sinks = Arc::new(sinks);
    let tty = spec.tty;
    let relay = |from: std::io::PipeReader, stream_id: u8| {
        let sinks = sinks.clone();
        std::thread::spawn(move || copy_out(from, stream_id, tty, &sinks))
    };
    let out = relay(out_r, 1);
    let err = err_r.map(|r| relay(r, 2));
    let registry = registry.clone();
    std::thread::spawn(move || {
        let _ = out.join();
        if let Some(e) = err {
            let _ = e.join();
        }
        let code = running.wait().unwrap_or(-1);
        registry.finish(&id, generation, code);
        for s in sinks.iter() {
            if let Ok(sock) = s.socket.lock() {
                let _ = sock.shutdown(Shutdown::Both);
            }
        }
        for d in dones {
            let _ = d.send(());
        }
    });
    Ok(())
}

/// Copy until the end of `from` or an error. Not `std::io::copy`: from a socket into a
/// pipe it splices, and a splice holds the pipe's lock while it waits for the socket;
/// the shim, closing its end of that pipe as it exits, then hangs in the kernel.
pub fn pump(from: &mut impl Read, to: &mut impl Write) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Close clients whose container did not start.
fn close(clients: Vec<Attachment>) {
    for c in clients {
        let _ = c.socket.shutdown(Shutdown::Both);
        let _ = c.done.send(());
    }
}

/// Copy a process's output pipe to its clients until the pipe ends: raw with a terminal,
/// framed without. A client that left gets nothing more; the pipe still drains, so the
/// process never blocks on it.
pub fn copy_out(mut from: impl Read, stream_id: u8, tty: bool, sinks: &[Sink]) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let bytes = if tty {
            buf[..n].to_vec()
        } else {
            frame(stream_id, &buf[..n])
        };
        for s in sinks {
            let wanted = if stream_id == 1 {
                s.wanted.stdout
            } else {
                s.wanted.stderr
            };
            if wanted && s.alive.load(Ordering::Relaxed) {
                let written = s
                    .socket
                    .lock()
                    .map(|mut sock| sock.write_all(&bytes).is_ok())
                    .unwrap_or(false);
                if !written {
                    s.alive.store(false, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_waiter_wakes_as_soon_as_a_start_ends() {
        let a = Arc::new(Attachments::default());
        let generation = a.begin("c");
        let seen = a.changes();
        let ender = {
            let a = a.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                a.finish("c", generation, 0);
            })
        };
        let asked = std::time::Instant::now();
        assert!(a.wait_for_change(seen, Duration::from_secs(5)) > seen);
        assert!(
            asked.elapsed() < Duration::from_secs(1),
            "{:?}",
            asked.elapsed()
        );
        ender.join().unwrap();
        // Nothing new: the wait ends at its limit.
        let seen = a.changes();
        assert_eq!(a.wait_for_change(seen, Duration::from_millis(10)), seen);
    }

    #[test]
    fn frames_have_docker_headers() {
        assert_eq!(frame(1, b"hi"), vec![1, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']);
        let big = frame(2, &[7u8; 300]);
        assert_eq!(big[..8], [2, 0, 0, 0, 0, 0, 1, 44]);
        assert_eq!(big.len(), 308);
        assert_eq!(frame(1, b""), vec![1, 0, 0, 0, 0, 0, 0, 0]);
    }

    fn attach(a: &Attachments, id: &str) -> (u64, mpsc::Receiver<()>, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let wanted = Wanted {
            stdin: false,
            stdout: true,
            stderr: true,
        };
        let token = a.add(id, ours, wanted, b"x".to_vec(), tx, |s| {
            s.write_all(b"head")
        });
        (token.unwrap(), rx, theirs)
    }

    #[test]
    fn an_attachment_is_taken_once() {
        let a = Attachments::default();
        let (token, _rx, mut peer) = attach(&a, "c");
        let mut head = [0u8; 4];
        peer.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"head", "sent before it was listed");
        assert!(a.is_waiting("c", token));
        let taken = a.take("c");
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].pending, b"x");
        assert!(!a.is_waiting("c", token));
        assert!(!a.withdraw("c", token), "a start took it");
        assert!(a.take("c").is_empty());
    }

    #[test]
    fn a_withdrawn_attachment_is_not_started() {
        let a = Attachments::default();
        let (first, _rx1, _p1) = attach(&a, "c");
        let (second, _rx2, _p2) = attach(&a, "c");
        assert!(a.withdraw("c", first));
        assert!(!a.withdraw("c", first));
        assert!(a.is_waiting("c", second));
        assert_eq!(a.take("c").len(), 1);
    }

    #[test]
    fn exits_newer_than_a_wait_count() {
        let a = Attachments::default();
        let before = a.generation("c");
        assert_eq!(a.exit_after("c", before), None);
        let g = a.begin("c");
        assert_eq!(a.exit_after("c", before), Some(Exit::Running));
        a.finish("c", g, 3);
        assert_eq!(a.exit_after("c", before), Some(Exit::Code(3)));
        assert_eq!(a.exit_after("c", g), None, "a wait that began after it");
        let failed = a.begin("c");
        a.abandon("c", failed);
        assert_eq!(a.exit_after("c", before), None);
        a.finish("c", g, 4);
        assert_eq!(
            a.exit_after("c", before),
            None,
            "an old start cannot finish a new one"
        );
    }
}
