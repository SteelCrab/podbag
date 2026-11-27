//! The backend a UI talks to: [`Api`], on the real engine ([`Desktop`]) or on sample data
//! (`mock` feature), and [`Backend`], which runs long work in the background.
//!
//! A UI calls the quick methods directly. Long work (a pull, a VM start, a migration)
//! goes through the `start_*` methods of [`Backend`]; streams (logs, the engine's
//! events) are started by `logs_start` and `watch`. Their results arrive as [`Event`]s on
//! the channel that [`Backend::real`] or [`Backend::mock`] returns: drain it each frame
//! with `try_recv`, after the waker asked for a redraw.

use crate::autostart::LoginItem;
use crate::compression::{self, Compression};
use crate::desktop::{Connection, Desktop};
use crate::dockercmd::DockerCommand;
use crate::engine::{EngineStatus, List};
use crate::events::{Event, Events};
use crate::images::{self, ImportImage, LocationInfo, Search};
use crate::kubernetes::{self, Resource, Snapshot};
use crate::migration::{self, Request};
use crate::settings::Settings;
use crate::terminal_cli;
use crate::training;
use crate::tray::TrayModel;
use crate::updates::{self, Check, Updates, Upgraded};
use crate::vm::{VmSettings, VmStatus};
use podbag::client::{
    ContainerSummary, Features, ImageSummary, Kind, NetworkSummary, Pruned, VolumeSummary,
};
use podbag::dashboard::lab;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// What the window can ask. Errors are the text the UI shows. The engine calls use the
/// current connection; events carry its token.
pub trait Api: Send + Sync {
    fn events(&self) -> &Events;

    // The engine.
    fn connection(&self) -> Connection;
    /// The docker contexts, for the engine menu.
    fn contexts(&self) -> Vec<String>;
    /// Whether the engine answers (`start`: start a stopped VM). Slow when it starts one.
    fn engine(&self, start: bool) -> EngineStatus;
    /// Use another engine (`podbag`, `docker`, …); then ask `connection` and `engine`.
    fn set_engine(&self, name: &str) -> Result<(), String>;

    // Lists and objects.
    fn containers(&self) -> Result<Vec<ContainerSummary>, String>;
    fn images(&self) -> Result<Vec<ImageSummary>, String>;
    fn volumes(&self) -> Result<Vec<VolumeSummary>, String>;
    fn networks(&self) -> Result<Vec<NetworkSummary>, String>;
    fn list(&self, kind: Kind) -> Result<List, String> {
        Ok(match kind {
            Kind::Containers => List::Containers(self.containers()?),
            Kind::Images => List::Images(self.images()?),
            Kind::Volumes => List::Volumes(self.volumes()?),
            Kind::Networks => List::Networks(self.networks()?),
        })
    }
    fn inspect(&self, kind: Kind, id: &str) -> Result<Value, String>;
    fn features(&self, sample: Option<&str>) -> Features;
    /// `stats` or `top` of a container.
    fn container_json(&self, id: &str, what: &str) -> Result<Value, String>;
    /// `start`, `stop`, `restart`, `pause` or `unpause`.
    fn act(&self, id: &str, action: &str) -> Result<(), String>;
    fn remove(&self, kind: Kind, id: &str, force: bool) -> Result<(), String>;
    fn prune(&self, kind: Kind) -> Result<Pruned, String>;
    fn rename(&self, id: &str, name: &str) -> Result<(), String>;
    fn create_volume(&self, name: &str) -> Result<(), String>;
    /// A terminal window with a shell in the container; the error is the command to run
    /// by hand.
    fn open_shell(&self, id: &str) -> Result<(), String>;
    /// `plain` or `upgrade`.
    fn open_terminal(&self, what: &str) -> Result<(), String>;

    // Streams.
    /// Follow a container's log: [`Event::Logs`] with the generation returned.
    fn logs_start(&self, id: &str) -> Result<u64, String>;
    fn logs_stop(&self);
    /// The engine's events while `on`: [`Event::Live`] and [`Event::Changed`].
    fn watch(&self, on: bool);

    // Images.
    /// Pull, with [`Event::PullProgress`] for each line; gives the reference pulled. Slow:
    /// see [`Backend::start_pull`].
    fn pull(
        &self,
        image: &str,
        location: Option<&str>,
        host: Option<&str>,
    ) -> Result<String, String>;
    fn search(&self, term: &str) -> Search;
    /// The pull locations and who is logged in there. `hosts`: (location id, host).
    fn registries(&self, hosts: &[(String, String)]) -> Vec<LocationInfo>;
    fn registry_login(
        &self,
        location: &str,
        host: &str,
        user: &str,
        password: &str,
    ) -> Result<(), String>;
    fn docker_images(&self, hosts: &[(String, String)]) -> Result<Vec<ImportImage>, String>;
    fn import_images(&self, names: &[String]) -> Result<(), String>;

    // The `docker` command.
    fn docker_command(&self) -> DockerCommand;
    /// `use` or `back`: [`Event::DockerSwitching`], then [`Event::DockerChanged`].
    fn docker_switch(&self, to: &str) -> Result<DockerCommand, String>;

    // The VM.
    /// `start`, `stop` or `restart`, then [`Event::EngineChanged`]. Slow: see
    /// [`Backend::start_vm`].
    fn vm(&self, action: &str) -> Result<(), String>;
    /// None for an engine without a VM (Docker, Linux).
    fn vm_status(&self) -> Result<Option<VmStatus>, String>;
    fn vm_set(&self, settings: &VmSettings) -> Result<String, String>;
    /// Image compression (podbag only): read it (None) or set it. On a Mac or Windows
    /// it runs in the VM, which it starts when it is stopped. Slow.
    fn compression(&self, mode: Option<compression::Mode>) -> Result<Compression, String>;
    /// Read the engine's state again ([`Event::VmStatus`] when the VM changed). `asked`:
    /// also ask the API while the window is hidden.
    fn refresh(&self, asked: bool);

    // Kubernetes.
    fn kube_contexts(&self) -> Result<Vec<String>, String>;
    fn kube_counts(
        &self,
        context: Option<&str>,
        all_namespaces: bool,
    ) -> Result<BTreeMap<String, usize>, String>;
    fn kube_list(
        &self,
        kind: Resource,
        all_namespaces: bool,
        context: Option<&str>,
    ) -> Result<Snapshot, String>;

    // Migration.
    fn migration_sources(&self) -> Vec<String>;
    fn migration_list(&self, source: &str, kind: Kind) -> Result<List, String>;
    /// Copy; `progress` gets each step. Slow: see [`Backend::start_migration`].
    fn migrate(&self, request: &Request, progress: &dyn Fn(&str)) -> Result<(), String>;

    // Settings and the app.
    fn settings(&self) -> Settings;
    fn set_settings(&self, settings: Settings) -> Result<(), String>;
    /// The language to show (`en`, `ko`, …).
    fn language(&self) -> &'static str;
    fn take_star_prompt(&self) -> bool;
    fn updates(&self, check: Check) -> Updates;
    fn upgrade(&self, version: Option<&str>) -> Upgraded;
    /// Install a new package of the app and start it; the UI ends the app after.
    fn update_app(&self, version: Option<&str>) -> Result<(), String>;
    fn terminal_command(&self) -> Option<terminal_cli::Status>;
    fn install_terminal_command(&self) -> Result<Option<terminal_cli::Status>, String>;
    fn open_releases(&self) -> Result<(), String>;
    fn open_repository(&self) -> Result<(), String>;
    fn open_training_guide(&self, id: &str, lang: &str) -> Result<(), String>;

    // The window, the tray and the system.
    /// Whether the app starts at login (see [`crate::autostart`]).
    fn launch_at_login(&self) -> Result<bool, String>;
    fn set_launch_at_login(&self, on: bool) -> Result<(), String>;
    /// What the tray (menu bar) item shows now. Ask again after [`Event::StatusChanged`].
    fn tray(&self) -> TrayModel;
    /// The window was shown (true) or hidden. Hidden, its streams end and the engine is
    /// asked only when the user asks (a click on the tray).
    fn set_visible(&self, visible: bool);
    /// The tray's VM item: start the VM when it is stopped, else stop it. Slow.
    fn toggle_vm(&self) -> Result<(), String>;
    /// The app ends: streams stop and the tutorial lab is cleaned up.
    fn shutdown(&self);

    // Training.
    /// The installed modules after `action`: `status`, `summary` (of `id` only),
    /// `install` (the module file's text `package`) or `remove` (`id`).
    fn training_modules(
        &self,
        action: &str,
        package: Option<&str>,
        id: Option<&str>,
    ) -> Result<Vec<training::LessonRecord>, String>;
    /// The practice session a terminal started (`podbag lab start`), after `action` on
    /// `name` in `session`: the GUI volumes lesson, the containers lesson (`containers`)
    /// or a course module (`course`). Null when none runs.
    fn practice(
        &self,
        action: &str,
        session: Option<&str>,
        name: Option<&str>,
        containers: bool,
        course: bool,
    ) -> Result<Value, String>;
    /// A step of the tutorial lab: `status`, `start`, `create`, `save`, `serve`,
    /// `request` or `end`.
    fn tutorial_lab(&self, action: &str, value: Option<&str>) -> Result<lab::Snapshot, String>;
    /// The JSON lesson installed in this app, with whether it is complete.
    fn saved_lesson(&self) -> Option<Value>;
    /// Keep the JSON lesson (None: remove it).
    fn save_lesson(&self, lesson: Option<&Value>) -> Result<(), String>;
}

impl Api for Desktop {
    fn events(&self) -> &Events {
        Desktop::events(self)
    }

    fn connection(&self) -> Connection {
        Desktop::connection(self)
    }

    fn contexts(&self) -> Vec<String> {
        podbag::client::choice::docker_contexts()
    }

    fn engine(&self, start: bool) -> EngineStatus {
        self.current().check_engine(start)
    }

    fn set_engine(&self, name: &str) -> Result<(), String> {
        Desktop::set_engine(self, self.current().id, name)
    }

    fn containers(&self) -> Result<Vec<ContainerSummary>, String> {
        self.current().containers()
    }

    fn images(&self) -> Result<Vec<ImageSummary>, String> {
        self.current().images()
    }

    fn volumes(&self) -> Result<Vec<VolumeSummary>, String> {
        self.current().volumes()
    }

    fn networks(&self) -> Result<Vec<NetworkSummary>, String> {
        self.current().networks()
    }

    fn inspect(&self, kind: Kind, id: &str) -> Result<Value, String> {
        self.current().inspect(kind, id)
    }

    fn features(&self, sample: Option<&str>) -> Features {
        self.current().features(sample)
    }

    fn container_json(&self, id: &str, what: &str) -> Result<Value, String> {
        self.current().container_json(id, what)
    }

    fn act(&self, id: &str, action: &str) -> Result<(), String> {
        self.current().act(id, action)
    }

    fn remove(&self, kind: Kind, id: &str, force: bool) -> Result<(), String> {
        self.current().remove(kind, id, force)
    }

    fn prune(&self, kind: Kind) -> Result<Pruned, String> {
        self.current().prune(kind)
    }

    fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        self.current().rename(id, name)
    }

    fn create_volume(&self, name: &str) -> Result<(), String> {
        self.current().create_volume(name)
    }

    fn open_shell(&self, id: &str) -> Result<(), String> {
        self.current().open_shell(id)
    }

    fn open_terminal(&self, what: &str) -> Result<(), String> {
        Desktop::open_terminal(self, what)
    }

    fn logs_start(&self, id: &str) -> Result<u64, String> {
        self.current().logs_start(id)
    }

    fn logs_stop(&self) {
        self.current().logs_stop()
    }

    fn watch(&self, on: bool) {
        self.current().watch(on)
    }

    fn pull(
        &self,
        image: &str,
        location: Option<&str>,
        host: Option<&str>,
    ) -> Result<String, String> {
        self.current().pull(image, location, host)
    }

    fn search(&self, term: &str) -> Search {
        self.current().search(term)
    }

    fn registries(&self, hosts: &[(String, String)]) -> Vec<LocationInfo> {
        images::locations(&images::current_logins(), hosts)
    }

    fn registry_login(
        &self,
        location: &str,
        host: &str,
        user: &str,
        password: &str,
    ) -> Result<(), String> {
        Desktop::registry_login(self, location, host, user, password)
    }

    fn docker_images(&self, hosts: &[(String, String)]) -> Result<Vec<ImportImage>, String> {
        Desktop::docker_images(self, hosts)
    }

    fn import_images(&self, names: &[String]) -> Result<(), String> {
        self.current().import_images(names)
    }

    fn docker_command(&self) -> DockerCommand {
        Desktop::docker_command(self)
    }

    fn docker_switch(&self, to: &str) -> Result<DockerCommand, String> {
        Desktop::docker_switch(self, to)
    }

    fn vm(&self, action: &str) -> Result<(), String> {
        Desktop::vm(self, &self.current(), action)
    }

    fn vm_status(&self) -> Result<Option<VmStatus>, String> {
        self.current().vm_status()
    }

    fn vm_set(&self, settings: &VmSettings) -> Result<String, String> {
        Desktop::vm_set(self, &self.current(), settings)
    }

    fn compression(&self, mode: Option<compression::Mode>) -> Result<Compression, String> {
        let reply = self
            .current()
            .compression(mode.map(compression::Mode::as_str))?;
        Ok(Compression::from_json(&reply))
    }

    fn refresh(&self, asked: bool) {
        Desktop::refresh(self, asked)
    }

    fn kube_contexts(&self) -> Result<Vec<String>, String> {
        kubernetes::contexts()
    }

    fn kube_counts(
        &self,
        context: Option<&str>,
        all_namespaces: bool,
    ) -> Result<BTreeMap<String, usize>, String> {
        kubernetes::counts(context.map(str::to_string), all_namespaces)
    }

    fn kube_list(
        &self,
        kind: Resource,
        all_namespaces: bool,
        context: Option<&str>,
    ) -> Result<Snapshot, String> {
        kubernetes::list(kind, all_namespaces, context.map(str::to_string))
    }

    fn migration_sources(&self) -> Vec<String> {
        migration::sources(self.podbag())
    }

    fn migration_list(&self, source: &str, kind: Kind) -> Result<List, String> {
        migration::list(self.podbag(), source, kind.segment())
    }

    fn migrate(&self, request: &Request, progress: &dyn Fn(&str)) -> Result<(), String> {
        Desktop::migrate(self, request, progress)
    }

    fn settings(&self) -> Settings {
        Desktop::settings(self)
    }

    fn set_settings(&self, settings: Settings) -> Result<(), String> {
        Desktop::set_settings(self, settings)
    }

    fn language(&self) -> &'static str {
        Desktop::language(self)
    }

    fn take_star_prompt(&self) -> bool {
        Desktop::take_star_prompt(self)
    }

    fn updates(&self, check: Check) -> Updates {
        Desktop::updates(self, check)
    }

    fn upgrade(&self, version: Option<&str>) -> Upgraded {
        Desktop::upgrade(self, version)
    }

    fn update_app(&self, version: Option<&str>) -> Result<(), String> {
        Desktop::update_app(self, version)
    }

    fn terminal_command(&self) -> Option<terminal_cli::Status> {
        Desktop::terminal_command(self)
    }

    fn install_terminal_command(&self) -> Result<Option<terminal_cli::Status>, String> {
        Desktop::install_terminal_command(self)
    }

    fn open_releases(&self) -> Result<(), String> {
        updates::open_releases()
    }

    fn open_repository(&self) -> Result<(), String> {
        updates::open_repository()
    }

    fn open_training_guide(&self, id: &str, lang: &str) -> Result<(), String> {
        training::open_training_guide(id, lang)
    }

    fn launch_at_login(&self) -> Result<bool, String> {
        LoginItem::for_this_app()?.is_enabled()
    }

    fn set_launch_at_login(&self, on: bool) -> Result<(), String> {
        LoginItem::for_this_app()?.set_enabled(on)
    }

    fn tray(&self) -> TrayModel {
        Desktop::tray(self)
    }

    fn set_visible(&self, visible: bool) {
        if visible {
            Desktop::set_visible(self, true)
        } else {
            Desktop::hidden(self)
        }
    }

    fn toggle_vm(&self) -> Result<(), String> {
        Desktop::toggle_vm(self)
    }

    fn shutdown(&self) {
        Desktop::shutdown(self)
    }

    fn training_modules(
        &self,
        action: &str,
        package: Option<&str>,
        id: Option<&str>,
    ) -> Result<Vec<training::LessonRecord>, String> {
        training::volume_tools_lab(action, package, id)
    }

    fn practice(
        &self,
        action: &str,
        session: Option<&str>,
        name: Option<&str>,
        containers: bool,
        course: bool,
    ) -> Result<Value, String> {
        training::practice_volumes(action, session, name, containers, course)
    }

    fn tutorial_lab(&self, action: &str, value: Option<&str>) -> Result<lab::Snapshot, String> {
        Desktop::tutorial_lab(self, training::lab_action(action)?, value)
    }

    fn saved_lesson(&self) -> Option<Value> {
        Desktop::saved_lesson(self)
    }

    fn save_lesson(&self, lesson: Option<&Value>) -> Result<(), String> {
        Desktop::save_lesson(self, lesson)
    }
}

/// A shared handle to an [`Api`]: cheap to clone, `Send` and `Sync`. It derefs to the
/// API, and runs long work on background threads.
#[derive(Clone)]
pub struct Backend(Arc<dyn Api>);

impl std::ops::Deref for Backend {
    type Target = dyn Api;

    fn deref(&self) -> &(dyn Api + 'static) {
        &*self.0
    }
}

impl Backend {
    pub fn new(api: impl Api + 'static) -> Backend {
        Backend(Arc::new(api))
    }

    /// The real engine, for an app of version `app_version` whose settings are in
    /// `config_dir`. `webview_data`: where an older app kept its web page's data (see
    /// [`Desktop::load_config`]). `waker` runs after each event (ask the UI for a redraw
    /// there).
    pub fn real(
        app_version: &'static str,
        config_dir: Option<&Path>,
        webview_data: Option<&Path>,
        waker: impl Fn() + Send + Sync + 'static,
    ) -> (Backend, Receiver<Event>) {
        let (events, rx) = Events::channel(waker);
        let desktop = Desktop::new(app_version, events);
        if let Some(dir) = config_dir {
            desktop.load_config(dir, webview_data);
        }
        (Backend::new(desktop), rx)
    }

    /// Sample data and no engine.
    #[cfg(feature = "mock")]
    pub fn mock(
        options: crate::mock::Options,
        waker: impl Fn() + Send + Sync + 'static,
    ) -> (Backend, Receiver<Event>) {
        let (events, rx) = Events::channel(waker);
        (Backend::new(crate::mock::Mock::new(options, events)), rx)
    }

    fn spawn(&self, job: impl FnOnce(&dyn Api) + Send + 'static) {
        let api = self.0.clone();
        std::thread::spawn(move || job(&*api));
    }

    /// [`Api::engine`] in the background: [`Event::Engine`].
    pub fn start_engine(&self, start: bool) {
        self.spawn(move |api| {
            let session = api.connection().token;
            let status = api.engine(start);
            api.events().send(Event::Engine { session, status });
        });
    }

    /// [`Api::pull`] in the background: [`Event::PullProgress`], then
    /// [`Event::PullDone`].
    pub fn start_pull(&self, image: &str, location: Option<&str>, host: Option<&str>) {
        let (image, location, host) = (
            image.to_string(),
            location.map(str::to_string),
            host.map(str::to_string),
        );
        self.spawn(move |api| {
            let session = api.connection().token;
            let result = api.pull(&image, location.as_deref(), host.as_deref());
            api.events().send(Event::PullDone { session, result });
        });
    }

    /// [`Api::vm`] in the background: [`Event::EngineChanged`], then [`Event::VmDone`].
    pub fn start_vm(&self, action: &str) {
        let action = action.to_string();
        self.spawn(move |api| {
            let session = api.connection().token;
            let result = api.vm(&action);
            api.events().send(Event::VmDone {
                session,
                action,
                result,
            });
        });
    }

    /// [`Api::docker_switch`] in the background: [`Event::DockerSwitching`],
    /// [`Event::DockerChanged`], then [`Event::DockerSwitched`].
    pub fn start_docker_switch(&self, to: &str) {
        let to = to.to_string();
        self.spawn(move |api| {
            let result = api.docker_switch(&to);
            api.events().send(Event::DockerSwitched(result));
        });
    }

    /// [`Api::migrate`] in the background: [`Event::MigrationProgress`] for each step,
    /// then [`Event::MigrationDone`].
    pub fn start_migration(&self, request: Request) {
        self.spawn(move |api| {
            let events = api.events().clone();
            let progress = |step: &str| {
                events.send(Event::MigrationProgress(step.to_string()));
            };
            let result = api.migrate(&request, &progress);
            events.send(Event::MigrationDone(result));
        });
    }

    /// [`Api::refresh`] every `every` on a thread, until the [`Monitor`] is dropped or
    /// the backend is gone.
    pub fn start_monitor(&self, every: Duration) -> Monitor {
        let stop = Arc::new(AtomicBool::new(false));
        let api: Weak<dyn Api> = Arc::downgrade(&self.0);
        let done = stop.clone();
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let Some(api) = api.upgrade() else { break };
                api.refresh(false);
                drop(api);
                std::thread::sleep(every);
            }
        });
        Monitor(stop)
    }
}

/// Stops the monitor thread of [`Backend::start_monitor`] when dropped.
#[derive(Debug)]
pub struct Monitor(Arc<AtomicBool>);

impl Drop for Monitor {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
