//! The app's state apart from its windows: the engine session, the settings, the
//! `docker` command, updates, and what the tray shows.

use crate::dockercmd::{self, DockerCommand};
use crate::events::{Event, Events};
use crate::images::{self, ImportImage};
use crate::lock;
use crate::migration;
use crate::session::Session;
use crate::settings::{self, Settings};
use crate::shell;
use crate::strings;
use crate::terminal_cli;
use crate::tray::{EngineState, TrayInput, TrayModel};
use crate::updates::{self, Check, Updates, Upgraded};
use crate::vm::VmSettings;
use podbag::client::{Choice, Client, Endpoint, Target, podbag_program};
use podbag::dashboard::app_update::{self, Installer};
use podbag::dashboard::{cli, lab};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The engine this window uses: its token, its setting name, and whether it is podbag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub token: u64,
    /// `podbag`, `docker`, `podman`, `kiri`, `context:<name>` or a socket URL.
    pub name: String,
    pub local: bool,
}

pub struct Desktop {
    app_version: &'static str,
    events: Events,
    podbag: PathBuf,
    /// How this app updates itself; None when it did not come from a package.
    installer: Option<Installer>,
    /// The engine this window uses, and its client.
    session: Mutex<Arc<Session>>,
    /// The engine's version (`podbag --version`), once asked.
    engine_version: Mutex<Option<String>>,
    version_asked: AtomicBool,
    /// Where the `docker` command goes, as last read (files only).
    docker: Mutex<Option<DockerCommand>>,
    /// The window and the tray share the same Docker CLI configuration.
    docker_switch: Mutex<()>,
    migration: Mutex<()>,
    star_prompt: AtomicBool,
    settings: Mutex<Settings>,
    settings_file: Mutex<Option<PathBuf>>,
    /// The window is shown: the tray may ask the engine's API.
    visible: AtomicBool,
    lab: Mutex<lab::Lab>,
}

/// podbag's own socket (where `docker` goes after "Use podbag for docker").
fn podbag_socket(podbag: &Path) -> PathBuf {
    match Endpoint::podbag_default(podbag) {
        Ok(Endpoint::Socket { path, .. }) => path,
        _ => PathBuf::new(),
    }
}

/// Docker, to copy images from. It is only read, never started.
pub(crate) fn docker_client(podbag: &Path) -> Result<Client, String> {
    Target::find(Some(&Choice::Docker), podbag).map(|t| Client::new(t.endpoint))
}

impl Desktop {
    /// The app of version `app_version` (its package's version), with the engine the
    /// user chose last. Events go to `events`.
    pub fn new(app_version: &'static str, events: Events) -> Desktop {
        let podbag = podbag_program();
        let target = Target::find(None, &podbag).unwrap_or_else(|_| Target {
            choice: Choice::Podbag,
            endpoint: Endpoint::Socket {
                path: PathBuf::from("docker.sock"),
                own: false,
            },
        });
        let installer = std::env::current_exe()
            .and_then(|e| e.canonicalize())
            .ok()
            .and_then(|e| Installer::detect(&e));
        Desktop {
            app_version,
            session: Mutex::new(Arc::new(Session::new(
                1,
                target,
                podbag.clone(),
                events.clone(),
            ))),
            events,
            podbag,
            installer,
            engine_version: Mutex::new(None),
            version_asked: AtomicBool::new(false),
            docker: Mutex::new(None),
            docker_switch: Mutex::new(()),
            migration: Mutex::new(()),
            star_prompt: AtomicBool::new(false),
            settings: Mutex::new(Settings::default()),
            settings_file: Mutex::new(None),
            visible: AtomicBool::new(true),
            lab: Mutex::new(lab::Lab::default()),
        }
    }

    /// Read the settings from `dir` (the app's config folder), where later changes are
    /// saved. `webview_data`: where an older app kept its web page's data; without it the
    /// star prompt is not offered.
    pub fn load_config(&self, dir: &Path, webview_data: Option<&Path>) {
        self.star_prompt.store(
            webview_data.is_some_and(|path| settings::take_star_prompt(dir, path)),
            Ordering::SeqCst,
        );
        let file = dir.join("settings.json");
        *lock(&self.settings) = Settings::load(&file);
        *lock(&self.settings_file) = Some(file);
    }

    pub fn events(&self) -> &Events {
        &self.events
    }

    fn status_changed(&self) {
        self.events.send(Event::StatusChanged);
    }

    /// The `podbag` program the app runs.
    pub fn podbag(&self) -> &Path {
        &self.podbag
    }

    pub fn app_version(&self) -> &'static str {
        self.app_version
    }

    // The engine session.

    pub fn current(&self) -> Arc<Session> {
        lock(&self.session).clone()
    }

    /// The session of `token`; an error once the engine has changed.
    pub fn session(&self, token: u64) -> Result<Arc<Session>, String> {
        let session = self.current();
        if session.id != token {
            return Err("The engine has changed. Reopen this view.".into());
        }
        Ok(session)
    }

    pub fn connection(&self) -> Connection {
        let current = self.current();
        Connection {
            token: current.id,
            name: current.target.choice.to_setting(),
            local: current.target.choice == Choice::Podbag,
        }
    }

    /// Switch the session of `token` to the engine `name`, saved in `file` (`ui.json`).
    pub fn switch(&self, token: u64, name: &str, file: &Path) -> Result<(), String> {
        let mut current = lock(&self.session);
        if current.id != token {
            return Err("The engine has changed.".into());
        }
        let target = crate::engine::choose(name, file, &self.podbag)?;
        current.retire();
        *current = Arc::new(Session::new(
            token + 1,
            target,
            self.podbag.clone(),
            self.events.clone(),
        ));
        Ok(())
    }

    /// Switch this window to `podbag`, `docker`, …; saved for the TUI too.
    pub fn set_engine(&self, token: u64, name: &str) -> Result<(), String> {
        let file = podbag::client::ui_dir()
            .ok_or("cannot find the folder for ui.json")?
            .join("ui.json");
        self.switch(token, name, &file)?;
        self.status_changed();
        Ok(())
    }

    // The VM.

    /// `podbag vm start`, `stop` or `restart` (macOS and Windows), then
    /// [`Event::EngineChanged`].
    pub fn vm(&self, session: &Session, action: &str) -> Result<(), String> {
        if !session.uses_podbag() {
            return Err("VM controls require Podbag".into());
        }
        if matches!(action, "stop" | "restart") {
            session.stop_streams();
        }
        let done = cli::vm_action(&self.podbag, action);
        self.refresh(false);
        session.emit(Event::EngineChanged {
            session: session.id,
            action: action.to_string(),
        });
        done
    }

    /// Start the VM when it is stopped, else stop it (the tray's item).
    pub fn toggle_vm(&self) -> Result<(), String> {
        let session = self.current();
        let running = session.engine_state() == EngineState::VmRunning;
        self.vm(&session, if running { "stop" } else { "start" })
    }

    /// `podbag vm set`: what it printed. With `restart`, the VM restarts now.
    pub fn vm_set(&self, session: &Session, settings: &VmSettings) -> Result<String, String> {
        if !session.uses_podbag() {
            return Err("VM controls require Podbag".into());
        }
        if settings.restart {
            session.stop_streams();
        }
        let done = cli::vm_set(&self.podbag, settings);
        if settings.restart {
            self.refresh(false);
            session.emit(Event::EngineChanged {
                session: session.id,
                action: "restart".into(),
            });
        }
        done
    }

    // The `docker` command.

    /// Where the `docker` command goes. It reads files only.
    pub fn docker_command(&self) -> DockerCommand {
        let now = dockercmd::current(&podbag_socket(&self.podbag));
        *lock(&self.docker) = Some(now.clone());
        now
    }

    /// Point the `docker` command at podbag (`use`), or back where it was (`back`):
    /// [`Event::DockerSwitching`], then [`Event::DockerChanged`].
    pub fn docker_switch(&self, to: &str) -> Result<DockerCommand, String> {
        let _switch = self
            .docker_switch
            .try_lock()
            .map_err(|_| "a Docker CLI switch is already in progress")?;
        let docker = podbag::client::find_docker_cli().ok_or("the docker CLI is not installed")?;
        let socket = podbag_socket(&self.podbag);
        self.events.send(Event::DockerSwitching(to.to_string()));
        let result = match to {
            "use" => podbag::client::use_podbag(&docker, &socket),
            "back" => podbag::client::switch_back(&docker).map(|_| ()),
            _ => Err(format!("unknown switch {to}")),
        };
        let now = dockercmd::current(&socket);
        *lock(&self.docker) = Some(now.clone());
        self.status_changed();
        self.events.send(Event::DockerChanged(now.clone()));
        result.map(|()| now)
    }

    /// Docker's images, to copy to podbag. Only read; Docker is never started. `hosts`:
    /// the registry hosts the user gave, by location id.
    pub fn docker_images(&self, hosts: &[(String, String)]) -> Result<Vec<ImportImage>, String> {
        let client = docker_client(&self.podbag)?;
        if client.ping().is_err() {
            return Err(podbag::client::choice::docker_down().into());
        }
        let configured: Vec<_> = hosts
            .iter()
            .filter(|(id, host)| id != "custom" && !host.is_empty())
            .filter_map(|(id, host)| images::location(id, host).ok())
            .filter(|location| location.needs_host())
            .collect();
        let images = client.images().map_err(|e| e.message)?;
        Ok(images
            .into_iter()
            .filter_map(|image| ImportImage::from_image(image, &configured))
            .collect())
    }

    /// Log in to a location. The password goes to the login program's stdin only.
    pub fn registry_login(
        &self,
        location: &str,
        host: &str,
        user: &str,
        password: &str,
    ) -> Result<(), String> {
        let loc = images::location(location, host)?;
        if loc.needs_host() && loc.host().is_empty() {
            return Err("the registry needs a host".into());
        }
        let server = if loc == podbag::client::Location::DockerHub {
            String::new()
        } else {
            loc.host()
        };
        let docker = podbag::client::find_docker_cli();
        podbag::client::login(&self.podbag, docker.as_deref(), &server, user, password)
    }

    // Terminals.

    /// A terminal window: `plain`, or `upgrade` (`sudo podbag upgrade`, where the user
    /// types the password). The error is the command to run by hand.
    pub fn open_terminal(&self, what: &str) -> Result<(), String> {
        match what {
            "plain" => shell::open_terminal(None)
                .then_some(())
                .ok_or_else(String::new),
            "upgrade" if !cfg!(windows) => {
                let podbag = self.cli_to_upgrade().ok_or("no podbag to update")?;
                let argv = [
                    "sudo".to_string(),
                    podbag.display().to_string(),
                    "upgrade".to_string(),
                ];
                shell::open_terminal(Some(&argv))
                    .then_some(())
                    .ok_or_else(|| cli::admin_command(&podbag))
            }
            _ => Err(format!("unknown terminal {what}")),
        }
    }

    /// What a terminal runs for `podbag`. None: this app is a build of its own, with no
    /// podbag to offer.
    pub fn terminal_command(&self) -> Option<terminal_cli::Status> {
        self.installer.as_ref()?;
        Some(terminal_cli::current(&self.podbag))
    }

    /// Make the `podbag` command from the app's own podbag. Ok(None): the user did not
    /// give the administrator password.
    pub fn install_terminal_command(&self) -> Result<Option<terminal_cli::Status>, String> {
        if self.installer.is_none() {
            return Err("this app has no podbag of its own to install".into());
        }
        let prompt = strings::text(self.language(), "cli.prompt", None);
        terminal_cli::install(&self.podbag, &prompt)
    }

    // Updates.

    /// The engine's version, asked once (`podbag --version` needs no engine).
    pub fn engine_version(&self) -> Option<String> {
        if !self.version_asked.swap(true, Ordering::SeqCst) {
            *lock(&self.engine_version) = cli::engine_version(&self.podbag);
        }
        lock(&self.engine_version).clone()
    }

    /// A newer release, as the last check found it (a file; no network). A packaged app
    /// compares its own version: its engine comes with it.
    pub fn known_update(&self) -> Option<String> {
        let (_, latest) = cli::saved_check(&updates::update_file()?)?;
        let current = if self.installer.is_some() {
            self.app_version.to_string()
        } else {
            lock(&self.engine_version).clone()?
        };
        cli::is_newer(&latest, &current).then_some(latest)
    }

    /// A `podbag` for the terminal that this app's package does not update.
    fn terminal_podbag(&self) -> Option<PathBuf> {
        self.installer.as_ref()?;
        podbag::client::terminal_podbag(&self.podbag)
    }

    /// The `podbag` that Update upgrades with `podbag upgrade`.
    fn cli_to_upgrade(&self) -> Option<PathBuf> {
        cli::cli_to_upgrade(
            &self.podbag,
            self.installer.is_some(),
            self.terminal_podbag().as_deref(),
        )
    }

    /// The versions and the last update check.
    pub fn updates(&self, check: Check) -> Updates {
        let file = updates::update_file();
        let mut u = cli::updates(&self.podbag, file.as_deref(), check, updates::unix_now());
        cli::plan(
            &mut u,
            self.installer.as_ref().map(Installer::name),
            self.terminal_podbag().as_deref(),
        );
        *lock(&self.engine_version) = u.engine.clone();
        self.version_asked.store(true, Ordering::SeqCst);
        self.status_changed();
        u
    }

    /// `podbag upgrade --yes [--version V]` on the `podbag` that Update upgrades: in a
    /// packaged app, a separate one for the terminal (the app's own comes with the app).
    pub fn upgrade(&self, version: Option<&str>) -> Upgraded {
        let Some(podbag) = self.cli_to_upgrade() else {
            return Upgraded {
                ok: true,
                ..Upgraded::default()
            };
        };
        let done = cli::upgrade_to(&podbag, version);
        if done.ok && podbag == self.podbag {
            *lock(&self.engine_version) = cli::engine_version(&self.podbag);
            self.status_changed();
        }
        done
    }

    /// Install the app's package of release `version` (None: the latest) and start the
    /// new app (or its installer). The caller ends this app soon after.
    pub fn update_app(&self, version: Option<&str>) -> Result<(), String> {
        let installer = self
            .installer
            .clone()
            .ok_or("This app was not installed from a package, so it cannot update itself.")?;
        let wanted = match version {
            Some(v) => Some(
                podbag::upgrade::Version::parse(v)
                    .ok_or_else(|| format!("{v:?} is not a version"))?,
            ),
            None => None,
        };
        let current =
            podbag::upgrade::Version::parse(self.app_version).ok_or("this app has no version")?;
        let restart = app_update::install(&installer, wanted.as_ref(), &current)?;
        self.current().stop_streams();
        restart.spawn()
    }

    // Settings.

    /// True once, on the first launch of a new installation.
    pub fn take_star_prompt(&self) -> bool {
        self.star_prompt.swap(false, Ordering::SeqCst)
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    /// Keep and save `settings` (checked first).
    pub fn set_settings(&self, settings: Settings) -> Result<(), String> {
        let settings = settings.checked();
        *lock(&self.settings) = settings.clone();
        self.status_changed();
        match lock(&self.settings_file).as_ref() {
            Some(file) => settings.save(file),
            None => Ok(()),
        }
    }

    /// The language to show: the setting, or the system's.
    pub fn language(&self) -> &'static str {
        strings::resolve(&lock(&self.settings).language, sys_locale::get_locale())
    }

    // The window and the tray.

    /// The window was shown (true) or hidden.
    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::SeqCst);
    }

    /// The window was hidden: its streams end.
    pub fn hidden(&self) {
        self.set_visible(false);
        self.current().stop_streams();
    }

    /// Read the engine's state for the tray. The VM's state costs nothing (macOS reads
    /// files); the API is asked only while the window is shown, or when `asked` (a click
    /// on the tray), so a hidden app keeps no VM or service awake.
    pub fn refresh(&self, asked: bool) {
        let session = self.current();
        // Once: `podbag --version` needs no engine and wakes nothing.
        let _ = self.engine_version();
        // The docker CLI's files only.
        *lock(&self.docker) = Some(dockercmd::current(&podbag_socket(&self.podbag)));
        let own = session.uses_podbag()
            && matches!(session.client.endpoint, Endpoint::Socket { own: true, .. });
        let ask_api = asked || self.visible.load(Ordering::SeqCst);
        let vm = if own && cfg!(target_os = "macos") {
            crate::engine::vm_status(&self.podbag)
        } else {
            None
        };
        let engine = match vm.as_deref() {
            Some("running") => EngineState::VmRunning,
            Some(_) => EngineState::VmStopped,
            None if ask_api => {
                if session.client.ping().is_ok() {
                    EngineState::Ready
                } else {
                    EngineState::Down
                }
            }
            None => session.engine_state(),
        };
        let previous = session.set_engine_state(engine);
        if previous != engine
            && let Some(vm) = vm
        {
            session.emit(Event::VmStatus {
                session: session.id,
                state: vm,
            });
        }
        if ask_api
            && matches!(engine, EngineState::Ready | EngineState::VmRunning)
            && let Ok(list) = session.client.containers()
        {
            session.set_running(list.iter().filter(|c| c.state == "running").count());
        }
        self.status_changed();
    }

    /// What the tray shows now.
    pub fn tray(&self) -> TrayModel {
        let session = self.current();
        TrayModel::new(&TrayInput {
            language: self.language(),
            engine: session.engine_state(),
            running: session.running(),
            engine_name: session.target.choice.label(),
            uses_podbag: session.uses_podbag(),
            update: self.known_update(),
            docker: lock(&self.docker).clone(),
        })
    }

    /// The app ends: streams stop and the tutorial lab is cleaned up.
    pub fn shutdown(&self) {
        self.current().stop_streams();
        self.end_lab();
    }

    // Training.

    /// A step of the tutorial lab (a volume and a local web service).
    pub fn tutorial_lab(
        &self,
        action: lab::Action,
        value: Option<&str>,
    ) -> Result<lab::Snapshot, String> {
        lock(&self.lab)
            .run(action, value)
            .map_err(|e| e.to_string())
    }

    /// End the tutorial lab, if one runs.
    pub fn end_lab(&self) {
        let _ = lock(&self.lab).run(lab::Action::End, None);
    }

    /// The JSON lesson, kept next to the settings.
    pub fn saved_lesson(&self) -> Option<serde_json::Value> {
        let file = lock(&self.settings_file).clone()?;
        crate::training::read_lesson(&crate::training::lesson_file(file.parent()?))
    }

    /// Keep the JSON lesson next to the settings (None: remove it).
    pub fn save_lesson(&self, lesson: Option<&serde_json::Value>) -> Result<(), String> {
        let file = lock(&self.settings_file)
            .clone()
            .ok_or("This app has no folder for its settings.")?;
        let dir = file
            .parent()
            .ok_or("This app has no folder for its settings.")?;
        crate::training::write_lesson(&crate::training::lesson_file(dir), lesson)
    }

    // Migration.

    /// Copy as `request` says; `progress` gets each step. One at a time.
    pub fn migrate(
        &self,
        request: &migration::Request,
        progress: impl Fn(&str),
    ) -> Result<(), String> {
        let _guard = self
            .migration
            .try_lock()
            .map_err(|_| "Another migration is in progress")?;
        migration::migrate(&self.podbag, request, progress)
    }
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use podbag::client::Kind;
    use podbag::client::fake::{FakeEngine, small_engine};

    #[test]
    fn switching_rejects_stale_actions_even_when_resource_names_match() {
        let first = FakeEngine::start(small_engine);
        let second = FakeEngine::start(small_engine);
        let state = Desktop::new("0.0.0", Events::none());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ui.json");
        state
            .switch(1, &first.client.endpoint.describe(), &file)
            .unwrap();
        let pending = state.session(2).unwrap();
        state
            .switch(2, &second.client.endpoint.describe(), &file)
            .unwrap();
        assert!(pending.is_retired());
        assert!(state.session(2).is_err());
        assert!(state.switch(2, "podbag", &file).is_err());
        state
            .session(3)
            .unwrap()
            .remove(Kind::Volumes, "pgdata", false)
            .unwrap();
        assert!(first.requests().is_empty());
        assert_eq!(second.requests(), ["DELETE /volumes/pgdata"]);
        assert_eq!(state.connection().token, 3);
    }

    fn on_fake(engine: &FakeEngine) -> (Desktop, std::sync::mpsc::Receiver<Event>) {
        let (events, rx) = Events::channel(|| {});
        let desktop = Desktop::new("0.0.0", events);
        let dir = tempfile::tempdir().unwrap();
        desktop
            .switch(
                1,
                &engine.client.endpoint.describe(),
                &dir.path().join("ui.json"),
            )
            .unwrap();
        (desktop, rx)
    }

    #[test]
    fn listing_containers_updates_the_tray() {
        let engine = FakeEngine::start(small_engine);
        let (desktop, rx) = on_fake(&engine);
        let session = desktop.session(2).unwrap();
        assert_eq!(session.running(), None);
        assert_eq!(session.containers().unwrap().len(), 2);
        assert_eq!(session.running(), Some(1));
        assert_eq!(rx.try_recv(), Ok(Event::StatusChanged));
        let tray = desktop.tray();
        assert_eq!(
            tray.running,
            strings::text(desktop.language(), "tray.running", Some(1))
        );
        assert!(!tray.vm_enabled, "a socket is not podbag");
    }

    #[test]
    fn a_hidden_window_asks_the_api_nothing() {
        let engine = FakeEngine::start(small_engine);
        let (desktop, _rx) = on_fake(&engine);
        desktop.hidden();
        desktop.refresh(false);
        assert!(engine.requests().is_empty());
        assert_eq!(desktop.current().engine_state(), EngineState::Unknown);
        desktop.refresh(true);
        assert_eq!(desktop.current().engine_state(), EngineState::Ready);
        assert_eq!(desktop.current().running(), Some(1));
    }

    #[test]
    fn vm_controls_need_podbag() {
        let engine = FakeEngine::start(small_engine);
        let (desktop, _rx) = on_fake(&engine);
        let session = desktop.current();
        assert_eq!(
            desktop.vm(&session, "stop"),
            Err("VM controls require Podbag".into())
        );
        assert_eq!(
            desktop.vm_set(&session, &VmSettings::default()),
            Err("VM controls require Podbag".into())
        );
        assert_eq!(session.vm_status(), Ok(None));
        assert!(engine.requests().is_empty());
    }

    #[test]
    fn settings_are_kept_checked_and_saved() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = Desktop::new("0.0.0", Events::none());
        desktop.load_config(dir.path(), None);
        assert!(!desktop.take_star_prompt());
        desktop
            .set_settings(Settings {
                language: "ko".into(),
                sidebar_width: 9000,
                ..Settings::default()
            })
            .unwrap();
        assert_eq!(desktop.language(), "ko");
        assert_eq!(desktop.settings().sidebar_width, settings::SIDEBAR_MAX);
        let saved = Settings::load(&dir.path().join("settings.json"));
        assert_eq!(saved, desktop.settings());
    }
}
