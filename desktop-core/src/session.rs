//! One connection to an engine. Switching the engine retires the session, with its
//! streams; work for a retired session fails or ends, so nothing reaches the wrong engine.

use crate::engine::{self, EngineStatus, List};
use crate::events::{Event, Events};
use crate::images::{self, Search};
use crate::lock;
use crate::run;
use crate::shell;
use crate::tray::EngineState;
use crate::vm::{self, VmResources, VmStatus};
use podbag::client::{
    Choice, Client, Closer, ContainerSummary, Features, ImageSummary, Kind, NetworkSummary, Pruned,
    RunSpec, Target, VolumeSummary,
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Session {
    /// The token of this connection: it grows with each engine change.
    pub id: u64,
    retired: AtomicBool,
    pub target: Target,
    pub client: Client,
    podbag: PathBuf,
    events: Events,
    /// The events stream, while the window is open.
    watching: AtomicBool,
    watch_generation: AtomicU64,
    watch: Mutex<Option<Closer>>,
    /// The open log stream and its generation.
    logs: Mutex<Option<Closer>>,
    log_generation: AtomicU64,
    engine: Mutex<EngineState>,
    /// Running containers, as last seen.
    running: Mutex<Option<usize>>,
}

impl Session {
    pub(crate) fn new(id: u64, target: Target, podbag: PathBuf, events: Events) -> Session {
        Session {
            id,
            retired: AtomicBool::new(false),
            client: Client::new(target.endpoint.clone()),
            target,
            podbag,
            events,
            watching: AtomicBool::new(false),
            watch_generation: AtomicU64::new(0),
            watch: Mutex::new(None),
            logs: Mutex::new(None),
            log_generation: AtomicU64::new(0),
            engine: Mutex::new(EngineState::Unknown),
            running: Mutex::new(None),
        }
    }

    pub(crate) fn emit(&self, event: Event) -> bool {
        self.events.send(event)
    }

    /// The engine changed: this session is no longer used.
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.stop_streams();
    }

    /// This window uses podbag (not Docker or another engine).
    pub fn uses_podbag(&self) -> bool {
        self.target.choice == Choice::Podbag
    }

    pub fn engine_state(&self) -> EngineState {
        *lock(&self.engine)
    }

    pub(crate) fn set_engine_state(&self, state: EngineState) -> EngineState {
        std::mem::replace(&mut *lock(&self.engine), state)
    }

    /// Running containers, as last seen.
    pub fn running(&self) -> Option<usize> {
        *lock(&self.running)
    }

    pub(crate) fn set_running(&self, n: usize) {
        *lock(&self.running) = Some(n);
    }

    pub fn stop_watch(&self) {
        self.watching.store(false, Ordering::SeqCst);
        self.watch_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = lock(&self.watch).take() {
            c.close();
        }
    }

    pub fn stop_streams(&self) {
        self.stop_watch();
        self.logs_stop();
    }

    /// Find out whether the engine answers (`start`: start a stopped VM).
    pub fn check_engine(&self, start: bool) -> EngineStatus {
        let status = engine::engine(&self.target, &self.client, &self.podbag, start);
        self.set_engine_state(EngineState::of(&status));
        self.emit(Event::StatusChanged);
        status
    }

    pub fn list(&self, kind: Kind) -> Result<List, String> {
        Ok(match kind {
            Kind::Containers => List::Containers(self.containers()?),
            Kind::Images => List::Images(self.images()?),
            Kind::Volumes => List::Volumes(self.volumes()?),
            Kind::Networks => List::Networks(self.networks()?),
        })
    }

    /// The containers; the tray learns how many run.
    pub fn containers(&self) -> Result<Vec<ContainerSummary>, String> {
        let list = self.client.containers().map_err(|e| e.message)?;
        self.set_running(list.iter().filter(|c| c.state == "running").count());
        self.emit(Event::StatusChanged);
        Ok(list)
    }

    pub fn images(&self) -> Result<Vec<ImageSummary>, String> {
        self.client.images().map_err(|e| e.message)
    }

    pub fn volumes(&self) -> Result<Vec<VolumeSummary>, String> {
        self.client.volumes().map_err(|e| e.message)
    }

    pub fn networks(&self) -> Result<Vec<NetworkSummary>, String> {
        self.client.networks().map_err(|e| e.message)
    }

    pub fn inspect(&self, kind: Kind, id: &str) -> Result<Value, String> {
        engine::inspect(&self.client, kind, id)
    }

    /// `start`, `stop`, `restart`, `pause` or `unpause`.
    pub fn act(&self, id: &str, action: &str) -> Result<(), String> {
        engine::act(&self.client, id, action)
    }

    pub fn remove(&self, kind: Kind, id: &str, force: bool) -> Result<(), String> {
        engine::remove(&self.client, kind, id, force)
    }

    pub fn create_volume(&self, name: &str) -> Result<(), String> {
        self.client.create_volume(name).map_err(|e| e.message)
    }

    pub fn prune(&self, kind: Kind) -> Result<Pruned, String> {
        engine::prune(&self.client, kind)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        engine::rename(&self.client, id, name)
    }

    /// `stats` or `top` of a container.
    pub fn container_json(&self, id: &str, what: &str) -> Result<Value, String> {
        engine::container_json(&self.client, id, what)
    }

    pub fn features(&self, sample: Option<&str>) -> Features {
        self.client.features(sample)
    }

    /// Pull `image` from the location `location` (with `host` for ECR, GAR, ACR or
    /// another registry), with the login for its registry when the engine is local. Each
    /// line of progress is a [`Event::PullProgress`]. Gives the reference pulled.
    pub fn pull(
        &self,
        image: &str,
        location: Option<&str>,
        host: Option<&str>,
    ) -> Result<String, String> {
        let location = match location {
            Some(id) => Some(images::location(id, host.unwrap_or(""))?),
            None => None,
        };
        let reference = images::reference(image, location.as_ref())?;
        images::pull(
            &self.client,
            &self.target,
            &reference,
            podbag::client::registry_auth,
            |line| {
                self.emit(Event::PullProgress {
                    session: self.id,
                    line: line.to_string(),
                });
            },
        )?;
        Ok(reference)
    }

    /// Create and start a container; a missing image is pulled first, each line of it a
    /// [`Event::PullProgress`]. Gives the container's id.
    pub fn run_container(&self, spec: &RunSpec) -> Result<String, String> {
        run::run(
            &self.client,
            &self.target,
            spec,
            podbag::client::registry_auth,
            |line| {
                self.emit(Event::PullProgress {
                    session: self.id,
                    line: line.to_string(),
                });
            },
        )
    }

    /// Search Docker Hub through the engine.
    pub fn search(&self, term: &str) -> Search {
        images::search(&self.client, term)
    }

    /// Copy images from Docker to podbag (this window must use podbag).
    pub fn import_images(&self, names: &[String]) -> Result<(), String> {
        if !self.uses_podbag() {
            return Err("this window does not use podbag".into());
        }
        let from = crate::desktop::docker_client(&self.podbag)?;
        images::import(&from, &self.client, names)
    }

    /// Follow a container's log: [`Event::Logs`] with this generation, until
    /// [`Session::logs_stop`] or another `logs_start`.
    pub fn logs_start(self: &Arc<Self>, id: &str) -> Result<u64, String> {
        if let Some(c) = lock(&self.logs).take() {
            c.close();
        }
        let generation = self.log_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let lines = self.client.logs(id, 500, true).map_err(|e| e.message)?;
        {
            let mut active = lock(&self.logs);
            if self.is_retired() || generation != self.log_generation.load(Ordering::SeqCst) {
                lines.closer().close();
                return Err("The log view has closed.".into());
            }
            *active = Some(lines.closer());
        }
        let session = self.clone();
        std::thread::spawn(move || {
            engine::follow_logs(lines, move |lines| {
                let current = session.log_generation.load(Ordering::SeqCst);
                current == generation
                    && session.emit(Event::Logs {
                        session: session.id,
                        generation,
                        lines,
                    })
            });
        });
        Ok(generation)
    }

    pub fn logs_stop(&self) {
        self.log_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = lock(&self.logs).take() {
            c.close();
        }
    }

    /// Watch the engine's events while `on`: [`Event::Changed`] for each change, and
    /// [`Event::Live`] when the stream opens or breaks.
    pub fn watch(self: &Arc<Self>, on: bool) {
        if !on {
            self.stop_watch();
            return;
        }
        if self.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        // A thread of an earlier watch that still sleeps sees another generation and ends.
        let generation = self.watch_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let session = self.clone();
        std::thread::spawn(move || {
            let current = || {
                !session.is_retired()
                    && session.watching.load(Ordering::SeqCst)
                    && session.watch_generation.load(Ordering::SeqCst) == generation
            };
            let live = |on| {
                session.emit(Event::Live {
                    session: session.id,
                    on,
                })
            };
            while current() {
                if let Ok(events) = session.client.events() {
                    {
                        let mut stream = lock(&session.watch);
                        if !current() {
                            events.closer().close();
                            break;
                        }
                        *stream = Some(events.closer());
                    }
                    live(true);
                    for e in events {
                        let Ok(e) = e else { break };
                        let quiet = e.action.starts_with("exec_")
                            || matches!(e.action.as_str(), "attach" | "detach" | "resize");
                        if !quiet {
                            session.emit(Event::Changed {
                                session: session.id,
                                kind: e.kind,
                            });
                        }
                    }
                }
                live(false);
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    }

    /// Open a terminal window with a shell in the container. When none can be opened,
    /// the error is the command to run by hand.
    pub fn open_shell(&self, id: &str) -> Result<(), String> {
        let docker = podbag::client::choice::docker_cli();
        let shell = shell::shell_command(&self.target, &self.podbag, &docker, id)?;
        shell::open_shell(&shell)
    }

    /// `podbag vm resources`, through the same CLI as the VM's boot.
    pub fn vm_resources(&self, values: Option<VmResources>) -> Result<Value, String> {
        if !cfg!(target_os = "macos") || !self.uses_podbag() {
            return Err("VM resources require the local macOS engine".into());
        }
        vm::resources(&self.podbag, values)
    }

    /// `podbag system compression [idle|off] --json`: `{"mode", "after"}`. With no mode it
    /// only reads. On macOS the command runs in the VM (it starts the VM when none runs).
    pub fn compression(&self, mode: Option<&str>) -> Result<Value, String> {
        if !self.uses_podbag() {
            return Err("Image compression requires the podbag engine".into());
        }
        podbag::dashboard::cli::compression(&self.podbag, mode)
    }

    /// `podbag vm status --json`: null when the engine is not podbag, or on Linux (no VM).
    pub fn vm_status_json(&self) -> Result<Value, String> {
        if !self.uses_podbag() || engine::platform() == "linux" {
            return Ok(Value::Null);
        }
        podbag::dashboard::cli::vm_status(&self.podbag)
    }

    /// [`Session::vm_status_json`], typed.
    pub fn vm_status(&self) -> Result<Option<VmStatus>, String> {
        VmStatus::from_json(self.vm_status_json()?)
    }
}
