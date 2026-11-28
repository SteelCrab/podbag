use crate::build::managed_worker::{Process, Workspace};
use crate::{Error, Result};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
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
    pub(in crate::api) fn start(data: PathBuf) -> Result<Self> {
        let (requests, next) = mpsc::channel();
        // Linux ties the worker's parent-death signal to the spawning thread.
        // Use one owner so closing an Engine connection cannot stop another build.
        let thread = thread::Builder::new()
            .name("buildkit-worker".into())
            .spawn(move || serve(next, &data))
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

fn serve(requests: mpsc::Receiver<Request>, data: &Path) {
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
                let _ = reply.send(connect(&mut worker, data));
            }
            None | Some(Request::Shutdown) => return,
        }
    }
}

fn connect(worker: &mut Option<Running>, data: &Path) -> Result<Connection> {
    if let Some(running) = worker
        && running
            .process
            .running()
            .map_err(|error| worker_error(&error))?
    {
        return running.connect().map_err(|error| worker_error(&error));
    }
    *worker = None;
    let work = Workspace::new(data)?;
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
        let stream = UnixStream::connect(self.work.socket())?;
        self.last_used = Instant::now();
        Ok(Connection {
            stream,
            _live: self.live.clone(),
            exports: self.exports.clone(),
        })
    }
}

fn worker_error(error: &std::io::Error) -> Error {
    Error::io(
        "cannot use the private BuildKit worker",
        error,
        "check buildkitd and its Linux prerequisites",
    )
}
