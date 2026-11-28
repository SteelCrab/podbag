use crate::build::managed_worker::{Process, Workspace};
use crate::paths::Dirs;
use crate::{Error, Result};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const WORKER_IDLE: Duration = Duration::from_secs(15);

pub(in crate::api) struct Owner {
    requests: mpsc::Sender<Request>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(in crate::api) struct Client(mpsc::Sender<Request>);

pub(super) struct Connection {
    pub stream: UnixStream,
    pub exports: super::export::Exports,
    _live: Arc<()>,
}

enum Request {
    Connect(mpsc::Sender<Result<Connection>>),
    Shutdown,
}

struct Running {
    process: Process,
    work: Workspace,
    live: Arc<()>,
    last_used: Instant,
    exports: super::export::Exports,
}

impl Owner {
    pub(in crate::api) fn start(dirs: Dirs) -> Result<Self> {
        let (requests, next) = mpsc::channel();
        // Linux ties the worker's parent-death signal to the spawning thread.
        // Use one owner so closing an Engine connection cannot stop another build.
        let thread = thread::Builder::new()
            .name("buildkit-worker".into())
            .spawn(move || serve(next, &dirs))
            .map_err(|error| worker_error(&error))?;
        Ok(Self {
            requests,
            thread: Some(thread),
        })
    }

    pub(in crate::api) fn client(&self) -> Client {
        Client(self.requests.clone())
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Client {
    pub(super) fn connect(&self) -> Result<Connection> {
        let (reply, result) = mpsc::channel();
        let stopped = || {
            worker_error(&std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the BuildKit service stopped",
            ))
        };
        self.0
            .send(Request::Connect(reply))
            .map_err(|_| stopped())?;
        result.recv().map_err(|_| stopped())?
    }
}

fn serve(requests: mpsc::Receiver<Request>, dirs: &Dirs) {
    let mut worker: Option<Running> = None;
    loop {
        let request = if let Some(running) = &mut worker {
            match requests.recv_timeout(Duration::from_secs(1)) {
                Ok(request) => Some(request),
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if running.idle() {
                        worker = None;
                    }
                    continue;
                }
            }
        } else {
            requests.recv().ok()
        };
        match request {
            Some(Request::Connect(reply)) => {
                let _ = reply.send(connect(&mut worker, dirs));
            }
            None | Some(Request::Shutdown) => return,
        }
    }
}

fn connect(worker: &mut Option<Running>, dirs: &Dirs) -> Result<Connection> {
    if let Some(running) = worker
        && running
            .process
            .running()
            .map_err(|error| worker_error(&error))?
    {
        return running.connect().map_err(|error| worker_error(&error));
    }
    *worker = None;
    let work = Workspace::new(dirs)?;
    let process = work.start()?;
    let mut running = Running {
        process,
        work,
        live: Arc::new(()),
        last_used: Instant::now(),
        exports: super::export::Exports::default(),
    };
    let connection = running.connect().map_err(|error| worker_error(&error))?;
    *worker = Some(running);
    Ok(connection)
}

impl Running {
    fn idle(&mut self) -> bool {
        if Arc::strong_count(&self.live) > 1 {
            self.last_used = Instant::now();
        }
        self.last_used.elapsed() >= WORKER_IDLE
    }

    fn connect(&mut self) -> std::io::Result<Connection> {
        let stream = connect_socket(self.work.socket())?;
        self.last_used = Instant::now();
        Ok(Connection {
            stream,
            _live: self.live.clone(),
            exports: self.exports.clone(),
        })
    }
}

fn connect_socket(path: &Path) -> io::Result<UnixStream> {
    use rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect, socket_with,
    };
    // A worker that hasn't accepted its queued connections must never block shutdown.
    let socket = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )?;
    connect(&socket, &SocketAddrUnix::new(path)?)?;
    Ok(UnixStream::from(socket))
}

fn worker_error(error: &std::io::Error) -> Error {
    Error::io(
        "cannot use the private BuildKit worker",
        error,
        "check buildkitd and its Linux prerequisites",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, bind, listen, socket};

    #[test]
    fn a_full_unix_listener_rejects_another_connection_without_waiting() {
        let tmp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let path = tmp.path().join("buildkit.sock");
        let listener = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        bind(&listener, &SocketAddrUnix::new(&path).unwrap()).unwrap();
        listen(&listener, 0).unwrap();
        let _first = connect_socket(&path).unwrap();

        let (send, recv) = mpsc::channel();
        let client = thread::spawn(move || {
            let _ = send.send(connect_socket(&path));
        });
        let result = recv.recv_timeout(Duration::from_secs(1));
        drop(listener);
        client.join().unwrap();
        assert_eq!(
            result.unwrap().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
