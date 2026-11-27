//! Work that must not block a frame: engine and kubectl calls run on threads and their
//! results come back as `Done`, next to the core's `Event`s. In tests (`inline`) the
//! same work runs at once, so a test sees its result on the next frame.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use podbag_desktop_core::images::Search;
use podbag_desktop_core::kubernetes::Snapshot;
use podbag_desktop_core::vm::VmStatus;
use podbag_desktop_core::{
    Api, Backend, ContainerSummary, Event, Features, ImageSummary, Kind, NetworkSummary, Pruned,
    VolumeSummary,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// The result of a job. `session` is the engine connection the job used.
pub enum Done {
    Lists {
        session: u64,
        lists: Box<Fetched>,
    },
    Features {
        session: u64,
        features: Features,
    },
    /// A container action, a remove or a prune; the toast to show on success.
    Action {
        session: u64,
        id: String,
        /// A remove: the object's page closes.
        removed: bool,
        result: Result<String, String>,
    },
    Pruned {
        session: u64,
        kind: Kind,
        result: Result<Pruned, String>,
    },
    /// The work of an ask dialog (rename, create volume, socket).
    Asked(Result<Option<String>, String>),
    Inspect {
        session: u64,
        id: String,
        result: Result<Value, String>,
    },
    ContainerJson {
        session: u64,
        id: String,
        what: &'static str,
        result: Result<Value, String>,
    },
    LogsStarted {
        session: u64,
        id: String,
        result: Result<u64, String>,
    },
    Search {
        term: String,
        result: Search,
    },
    VmStatus {
        session: u64,
        result: Box<Result<Option<VmStatus>, String>>,
    },
    VmSet {
        restart: bool,
        result: Result<String, String>,
    },
    KubeList {
        generation: u64,
        result: Result<Snapshot, String>,
    },
    KubeContexts(Result<Vec<String>, String>),
    KubeCounts {
        generation: u64,
        result: Result<BTreeMap<String, usize>, String>,
    },
    KubeOpenShift {
        generation: u64,
        result: Result<bool, String>,
    },
    /// The result of a page's job, applied to the window (Training, Migrate, …).
    View(Box<dyn FnOnce(&mut crate::app::Desktop) + Send>),
}

/// The four lists, as the engine gave them.
pub struct Fetched {
    pub containers: Result<Vec<ContainerSummary>, String>,
    pub images: Result<Vec<ImageSummary>, String>,
    pub volumes: Result<Vec<VolumeSummary>, String>,
    pub networks: Result<Vec<NetworkSummary>, String>,
}

/// Runs jobs on threads, or at once when `inline`.
pub struct Jobs {
    backend: Backend,
    inline: bool,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    waker: Arc<dyn Fn() + Send + Sync>,
}

impl Jobs {
    pub fn new(backend: Backend, inline: bool, waker: Arc<dyn Fn() + Send + Sync>) -> Jobs {
        let (tx, rx) = channel();
        Jobs {
            backend,
            inline,
            tx,
            rx,
            waker,
        }
    }

    pub fn inline(&self) -> bool {
        self.inline
    }

    /// Run `job` with the API; its result comes back from `drain`.
    pub fn run(&self, job: impl FnOnce(&dyn Api) -> Done + Send + 'static) {
        if self.inline {
            let done = job(&*self.backend);
            let _ = self.tx.send(done);
            return;
        }
        let (api, tx, waker) = (self.backend.clone(), self.tx.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let done = job(&*api);
            if tx.send(done).is_ok() {
                waker();
            }
        });
    }

    /// The results that came in.
    pub fn drain(&self) -> Vec<Done> {
        self.rx.try_iter().collect()
    }

    // The core's background work: its result is an `Event` on the backend's channel.

    /// `Backend::start_engine`: `Event::Engine`.
    pub fn engine(&self, start: bool) {
        if !self.inline {
            return self.backend.start_engine(start);
        }
        let api = &*self.backend;
        let session = api.connection().token;
        let status = api.engine(start);
        api.events().send(Event::Engine { session, status });
    }

    /// `Backend::start_pull`: `Event::PullProgress`, then `Event::PullDone`.
    pub fn pull(&self, image: &str, location: Option<&str>, host: Option<&str>) {
        if !self.inline {
            return self.backend.start_pull(image, location, host);
        }
        let api = &*self.backend;
        let session = api.connection().token;
        let result = api.pull(image, location, host);
        api.events().send(Event::PullDone { session, result });
    }

    /// `Backend::start_run`: `Event::PullProgress` while a missing image is pulled, then
    /// `Event::RunDone`.
    pub fn run_container(&self, spec: podbag_desktop_core::RunSpec) {
        if !self.inline {
            return self.backend.start_run(spec);
        }
        let api = &*self.backend;
        let session = api.connection().token;
        let result = api.run_container(&spec);
        api.events().send(Event::RunDone { session, result });
    }

    /// `Backend::start_vm`: `Event::EngineChanged`, then `Event::VmDone`.
    pub fn vm(&self, action: &str) {
        if !self.inline {
            return self.backend.start_vm(action);
        }
        let api = &*self.backend;
        let session = api.connection().token;
        let result = api.vm(action);
        api.events().send(Event::VmDone {
            session,
            action: action.to_string(),
            result,
        });
    }

    /// `Backend::start_migration`: `Event::MigrationProgress` for each step, then
    /// `Event::MigrationDone`.
    pub fn migrate(&self, request: podbag_desktop_core::migration::Request) {
        if !self.inline {
            return self.backend.start_migration(request);
        }
        let api = &*self.backend;
        let events = api.events().clone();
        let progress = |step: &str| {
            events.send(Event::MigrationProgress(step.to_string()));
        };
        let result = api.migrate(&request, &progress);
        events.send(Event::MigrationDone(result));
    }

    /// `Backend::start_docker_switch`: `Event::DockerSwitching`, `Event::DockerChanged`,
    /// then `Event::DockerSwitched`.
    pub fn docker_switch(&self, to: &str) {
        if !self.inline {
            return self.backend.start_docker_switch(to);
        }
        let api = &*self.backend;
        let result = api.docker_switch(to);
        api.events().send(Event::DockerSwitched(result));
    }
}
