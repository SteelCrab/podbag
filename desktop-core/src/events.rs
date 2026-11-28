//! What the backend reports while work runs: typed events, given to a callback or put
//! on a channel that the UI drains each frame.

use crate::dockercmd::DockerCommand;
use crate::engine::EngineStatus;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

/// One event. `session` is the token of the engine connection that sent it (see
/// [`crate::Connection`]): after an engine change, events of the old one are stale.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The engine reported a change of this type (`container`, `image`, …): read the
    /// lists again.
    Changed { session: u64, kind: String },
    /// The engine's event stream is on (true) or off.
    Live { session: u64, on: bool },
    /// New lines of the log that `logs_start` follows.
    Logs {
        session: u64,
        generation: u64,
        lines: Vec<String>,
    },
    /// One line of pull progress.
    PullProgress { session: u64, line: String },
    /// The VM was started, stopped or restarted (the action): connect again.
    EngineChanged { session: u64, action: String },
    /// The VM's state changed outside the app: `running`, `stopped`, …
    VmStatus { session: u64, state: String },
    /// A Docker CLI switch started: `use` or `back`.
    DockerSwitching(String),
    /// Where the `docker` command goes, after a switch.
    DockerChanged(DockerCommand),
    /// What the tray shows may have changed: the engine's state, the running containers,
    /// a known update, the `docker` command or the language.
    StatusChanged,

    // The results of work that `Backend` runs in the background.
    /// The end of `Backend::start_pull`: the reference pulled, or the error.
    PullDone {
        session: u64,
        result: Result<String, String>,
    },
    /// The end of `Backend::start_run`: the new container's id, or the error.
    RunDone {
        session: u64,
        result: Result<String, String>,
    },
    /// The end of `Backend::start_engine`.
    Engine { session: u64, status: EngineStatus },
    /// The end of `Backend::start_vm`.
    VmDone {
        session: u64,
        action: String,
        result: Result<(), String>,
    },
    /// The end of `Backend::start_docker_switch`.
    DockerSwitched(Result<DockerCommand, String>),
    /// One step of `Backend::start_migration`: `prepare`, `image`, `files`, `verify`, …
    MigrationProgress(String),
    /// The end of `Backend::start_migration`.
    MigrationDone(Result<(), String>),
}

impl Event {
    /// The engine connection that sent it; None for events of the whole app.
    pub fn session(&self) -> Option<u64> {
        match self {
            Event::Changed { session, .. }
            | Event::Live { session, .. }
            | Event::Logs { session, .. }
            | Event::PullProgress { session, .. }
            | Event::EngineChanged { session, .. }
            | Event::VmStatus { session, .. }
            | Event::PullDone { session, .. }
            | Event::RunDone { session, .. }
            | Event::Engine { session, .. }
            | Event::VmDone { session, .. } => Some(*session),
            _ => None,
        }
    }
}

/// Called after each event on a channel, so the UI can ask for a redraw.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

type Sink = dyn Fn(Event) -> bool + Send + Sync;

/// Where events go. Cheap to clone. `send` returns false when nobody listens any more,
/// which ends a stream (a log that is followed).
#[derive(Clone)]
pub struct Events(Arc<Sink>);

impl Events {
    /// Each event goes to `f`, on the thread that sends it. `f` must not block for long,
    /// and must not call back into the backend while it holds a lock.
    pub fn callback(f: impl Fn(Event) -> bool + Send + Sync + 'static) -> Events {
        Events(Arc::new(f))
    }

    /// Events go on a channel; `waker` runs after each one.
    pub fn channel(waker: impl Fn() + Send + Sync + 'static) -> (Events, Receiver<Event>) {
        let (tx, rx) = channel();
        let events = Events::callback(move |e| {
            let sent = tx.send(e).is_ok();
            waker();
            sent
        });
        (events, rx)
    }

    /// Events go nowhere.
    pub fn none() -> Events {
        Events::callback(|_| true)
    }

    pub fn send(&self, event: Event) -> bool {
        (self.0)(event)
    }
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Events")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_channel_wakes_the_ui_and_ends_when_dropped() {
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        let (events, rx) = Events::channel(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        assert!(events.send(Event::StatusChanged));
        assert!(events.send(Event::Live {
            session: 3,
            on: true
        }));
        let got: Vec<Event> = rx.try_iter().collect();
        assert_eq!(got[0], Event::StatusChanged);
        assert_eq!(got[1].session(), Some(3));
        assert_eq!(woken.load(Ordering::SeqCst), 2);
        drop(rx);
        assert!(!events.send(Event::StatusChanged));
    }
}
