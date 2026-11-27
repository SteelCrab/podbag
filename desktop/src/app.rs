//! The window's state: what the engine reported, where the user is, and the work in
//! flight. Backend events and job results come in through `pump`; the shell and the
//! views paint from this state (`shell`, `views`, `dialogs`).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use podbag_desktop_core::dockercmd::DockerCommand;
use podbag_desktop_core::kubernetes::Resource;
use podbag_desktop_core::updates::{Check, Updates};
use podbag_desktop_core::vm::{VmSettings, VmStatus};
use podbag_desktop_core::{
    Backend, Connection, EngineStatus, Event, Features, Kind, Monitor, Settings, ThemePreference,
};
use podbag_ui::ui::{Theme, Ui, UiApp};
use podbag_ui::widgets::logs::LogView;
use podbag_ui::widgets::scroll::ScrollState;
use podbag_ui::widgets::toast::Toasts;
use podbag_ui::winit::keyboard::{Key, NamedKey};
use podbag_ui::{Color, Cx, Id, KeyPress, NavButton, TextInput, TextStyle, VirtualList};
use serde_json::Value;

use crate::dialogs::{AskFor, Dialog, Then};
use crate::fmt;
use crate::jobs::{Done, Fetched, Jobs};
use crate::lists::{Lists, is_up, kind_index};
use crate::tr::Tr;
use crate::views;
use crate::views::kubernetes::KubeState;
use crate::views::pull::PullPanel;

pub const SIDEBAR_WIDTH: f32 = 216.;
pub const SIDEBAR_MIN: f32 = 160.;
pub const SIDEBAR_MAX: f32 = 360.;
pub const RAIL: f32 = 56.;
/// A window this narrow shows the rail until the user opens the sidebar.
pub const NARROW: f32 = 720.;

/// A page of the window.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Page {
    Migrate,
    Containers,
    Images,
    Volumes,
    Networks,
    Training,
    /// The practice session's containers and volumes (Training).
    LabContainers,
    LabVolumes,
    Kubernetes,
    Settings,
}

impl Page {
    pub const ALL: [Page; 10] = [
        Page::Migrate,
        Page::Containers,
        Page::Images,
        Page::Volumes,
        Page::Networks,
        Page::Training,
        Page::LabContainers,
        Page::LabVolumes,
        Page::Kubernetes,
        Page::Settings,
    ];

    pub fn kind(self) -> Option<Kind> {
        match self {
            Page::Containers => Some(Kind::Containers),
            Page::Images => Some(Kind::Images),
            Page::Volumes => Some(Kind::Volumes),
            Page::Networks => Some(Kind::Networks),
            _ => None,
        }
    }

    pub fn of(kind: Kind) -> Page {
        match kind {
            Kind::Containers => Page::Containers,
            Kind::Images => Page::Images,
            Kind::Volumes => Page::Volumes,
            Kind::Networks => Page::Networks,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Page::Migrate => "migrate",
            Page::Containers => "containers",
            Page::Images => "images",
            Page::Volumes => "volumes",
            Page::Networks => "networks",
            Page::Training => "training",
            Page::LabContainers => "lab-containers",
            Page::LabVolumes => "lab-volumes",
            Page::Kubernetes => "kubernetes",
            Page::Settings => "settings",
        }
    }

    pub fn parse(name: &str) -> Option<Page> {
        Page::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// The Kubernetes resources of the sub-navigation.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum KubeKind {
    Pods,
    Services,
    Deployments,
    Claims,
}

impl KubeKind {
    pub const ALL: [KubeKind; 4] = [
        KubeKind::Pods,
        KubeKind::Services,
        KubeKind::Deployments,
        KubeKind::Claims,
    ];

    pub fn key(self) -> &'static str {
        match self {
            KubeKind::Pods => "pods",
            KubeKind::Services => "services",
            KubeKind::Deployments => "deployments",
            KubeKind::Claims => "claims",
        }
    }

    pub fn resource(self) -> Resource {
        match self {
            KubeKind::Pods => Resource::Pods,
            KubeKind::Services => Resource::Services,
            KubeKind::Deployments => Resource::Deployments,
            KubeKind::Claims => Resource::Claims,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Info,
    Logs,
    Inspect,
}

impl Tab {
    pub fn key(self) -> &'static str {
        match self {
            Tab::Info => "detail.info",
            Tab::Logs => "detail.logs",
            Tab::Inspect => "detail.inspect",
        }
    }
}

/// An object's page.
pub struct Detail {
    pub kind: Kind,
    pub id: String,
    pub tab: Tab,
    /// The inspect JSON, once loaded.
    pub inspect: LogView,
    pub inspect_loaded: bool,
    /// `top` and `stats` of a running container: None while asked.
    pub top: Option<String>,
    pub stats: Option<String>,
}

impl Detail {
    fn new(kind: Kind, id: String, tab: Tab) -> Detail {
        Detail {
            kind,
            id,
            tab,
            inspect: LogView::default(),
            inspect_loaded: false,
            top: None,
            stats: None,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
struct Entry {
    page: Page,
    detail: Option<(Kind, String, Tab)>,
    kube: Option<KubeKind>,
    training: Option<String>,
}

/// Messages to the window.
pub enum Msg {
    /// A backend event or a job result came in.
    Wake,
    /// A command for previews and screenshots (`nav images`, `theme dark`, …).
    Command(String),
}

/// How the app runs.
pub struct Options {
    /// Run jobs at once instead of on threads (tests).
    pub inline: bool,
    /// Poll the VM's state on a thread (the real engine).
    pub monitor: bool,
    /// Runs when a job ends (ask the window for a frame).
    pub waker: Arc<dyn Fn() + Send + Sync>,
}

impl Options {
    /// Jobs at once and no monitor: for tests.
    pub fn inline() -> Options {
        Options {
            inline: true,
            monitor: false,
            waker: Arc::new(|| {}),
        }
    }
}

pub struct VmForm {
    pub cpus: TextInput,
    pub memory: TextInput,
    pub idle: TextInput,
    pub dirty: bool,
    pub error: String,
    pub applying: bool,
}

pub struct SidebarDrag {
    pub x: f32,
    pub start: f32,
    pub width: f32,
}

pub struct Desktop {
    pub api: Backend,
    events: Receiver<Event>,
    pub jobs: Jobs,
    _monitor: Option<Monitor>,
    /// The engine connection whose events count.
    pub session: u64,
    pub tr: Tr,
    text_lang: Option<&'static str>,
    pub page: Page,
    pub detail: Option<Detail>,
    pub connection: Connection,
    pub contexts: Vec<String>,
    /// None while the engine is being checked.
    pub engine: Option<EngineStatus>,
    pub lists: Lists,
    pub loaded: bool,
    pub live: bool,
    pub features: Features,
    features_asked: bool,
    pub settings: Settings,
    pub busy: HashSet<String>,
    pub search: [TextInput; 4],
    pub tables: [VirtualList; 4],
    pub logs: LogView,
    pub logs_generation: u64,
    pub logs_error: Option<String>,
    pub vm: Option<VmStatus>,
    pub vm_error: Option<String>,
    pub vm_busy: bool,
    pub vm_form: VmForm,
    pub updates: Option<Updates>,
    pub docker: Option<DockerCommand>,
    pub docker_switching: Option<String>,
    docker_requested: bool,
    pub banner_closed: bool,
    pub kube: KubeState,
    pub kube_counts: BTreeMap<String, usize>,
    kube_counts_generation: u64,
    pub kube_expanded: bool,
    pub dialog: Option<Dialog>,
    pub pull: Option<PullPanel>,
    pub pull_hosts: Vec<(String, String)>,
    pub toasts: Toasts,
    pub sidebar_drag: Option<SidebarDrag>,
    pub expanded_in_narrow: bool,
    pub narrow: bool,
    pub settings_scroll: ScrollState,
    pub sidebar_scroll: ScrollState,
    pub detail_scroll: ScrollState,
    pub open_sections: HashSet<&'static str>,
    /// Give the focus to this widget on the next frame.
    pub focus_next: Option<Id>,
    history: Vec<Entry>,
    history_at: usize,
    refresh_at: Option<Instant>,
    poll_at: Instant,
    vm_poll_at: Instant,
    pub quit: bool,
    /// Quit at this time (after `update_app`, so the new app can start).
    quit_at: Option<Instant>,
    /// The window is hidden (closed to the tray): no polling until it shows.
    pub hidden: bool,
    /// The core reported `StatusChanged` (the tray's texts) since this was cleared.
    pub status_changed: bool,
    /// Whether the app starts at login; None until read.
    launch_at_login: Option<Result<bool, String>>,
    pub training: views::training::TrainingState,
    pub practice: views::practice::PracticeState,
    pub migrate: views::migrate::MigrateState,
    pub update: views::updates::UpdateState,
    pub notices: views::notices::Notices,
    pub compression: views::compression::CompressionState,
}

fn input(id: Id, placeholder: &str) -> TextInput {
    TextInput::new(
        id,
        placeholder,
        TextStyle::new(14.).line_height(20.),
        Color::TRANSPARENT,
    )
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// JSON as the app shows it (2 spaces).
pub fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

impl Desktop {
    pub fn new(api: Backend, events: Receiver<Event>, options: Options) -> Desktop {
        let settings = api.settings();
        let tr = Tr::new(api.language());
        let connection = api.connection();
        let placeholder = tr.t("search.placeholder");
        let tr_socket = tr.t("engine.socket");
        let now = Instant::now();
        let monitor = options
            .monitor
            .then(|| api.start_monitor(Duration::from_secs(5)));
        let jobs = Jobs::new(api.clone(), options.inline, options.waker);
        let mut d = Desktop {
            session: connection.token,
            contexts: api.contexts(),
            connection,
            events,
            jobs,
            _monitor: monitor,
            tr,
            text_lang: None,
            page: Page::Migrate,
            detail: None,
            engine: None,
            lists: Lists::default(),
            loaded: false,
            live: false,
            features: Features::default(),
            features_asked: false,
            busy: HashSet::new(),
            search: std::array::from_fn(|i| input(Id::of(("search", i)), &placeholder)),
            tables: std::array::from_fn(|i| VirtualList::new(if i == 0 { 58. } else { 44. }, 40.)),
            logs: LogView::default(),
            logs_generation: 0,
            logs_error: None,
            vm: None,
            vm_error: None,
            vm_busy: false,
            vm_form: VmForm {
                cpus: input(Id::of(("vm", "cpus")), ""),
                memory: input(Id::of(("vm", "memory")), ""),
                idle: input(Id::of(("vm", "idle")), ""),
                dirty: false,
                error: String::new(),
                applying: false,
            },
            updates: None,
            docker: None,
            docker_switching: None,
            docker_requested: false,
            banner_closed: false,
            kube: KubeState::new(&placeholder),
            kube_counts: BTreeMap::new(),
            kube_counts_generation: 0,
            kube_expanded: false,
            dialog: None,
            pull: None,
            pull_hosts: Vec::new(),
            toasts: Toasts::default(),
            sidebar_drag: None,
            expanded_in_narrow: false,
            narrow: false,
            settings_scroll: ScrollState::default(),
            sidebar_scroll: ScrollState::default(),
            detail_scroll: ScrollState::default(),
            open_sections: HashSet::new(),
            focus_next: None,
            history: Vec::new(),
            history_at: 0,
            refresh_at: None,
            poll_at: now + Duration::from_secs(3),
            vm_poll_at: now + Duration::from_secs(5),
            quit: false,
            quit_at: None,
            hidden: false,
            status_changed: false,
            launch_at_login: None,
            training: views::training::TrainingState::new(),
            practice: views::practice::PracticeState::default(),
            migrate: views::migrate::MigrateState::new(&tr_socket),
            update: views::updates::UpdateState::default(),
            notices: views::notices::Notices::default(),
            compression: views::compression::CompressionState::default(),
            api,
            settings,
        };
        d.load_updates();
        d.refresh_kube_counts();
        views::training::start(&mut d);
        views::entered(&mut d);
        d.remember();
        d.connect(false);
        views::notices::start(&mut d);
        d.pump();
        d
    }

    // ---- words and numbers ----

    pub fn t(&self, key: &str) -> String {
        self.tr.t(key)
    }

    pub fn tv(&self, key: &str, vars: &[(&str, &str)]) -> String {
        self.tr.tv(key, vars)
    }

    pub fn lang(&self) -> &'static str {
        self.tr.lang
    }

    pub fn ago(&self, when: i64) -> String {
        fmt::ago(when, now_secs(), self.lang())
    }

    /// Texts kept inside widgets (placeholders) follow the language.
    fn relabel(&mut self) {
        let placeholder = self.t("search.placeholder");
        for s in &mut self.search {
            s.set_placeholder(&placeholder);
        }
        self.kube.search.set_placeholder(&placeholder);
    }

    pub fn set_language(&mut self, setting: &str) {
        self.settings.language = setting.to_string();
        self.save_settings();
        self.tr = Tr::new(self.api.language());
        self.relabel();
    }

    pub fn set_theme(&mut self, theme: ThemePreference) {
        self.settings.theme = theme;
        self.save_settings();
    }

    pub fn save_settings(&mut self) {
        if let Err(e) = self.api.set_settings(self.settings.clone()) {
            self.toasts.push(e, true);
        }
    }

    // ---- the engine ----

    pub fn is_podbag(&self) -> bool {
        self.engine.as_ref().is_none_or(|e| e.engine == "podbag")
    }

    pub fn has_vm(&self) -> bool {
        self.is_podbag() && self.engine.as_ref().is_some_and(|e| e.platform != "linux")
    }

    pub fn reachable(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.reachable)
    }

    /// Ask the engine whether it answers (`start`: start a stopped VM).
    pub fn connect(&mut self, start: bool) {
        self.engine = None;
        self.jobs.engine(start);
    }

    /// After an engine switch: a new connection, whose events are the ones that count.
    pub fn reconnect(&mut self) {
        self.connection = self.api.connection();
        self.session = self.connection.token;
        self.contexts = self.api.contexts();
        self.features = Features::default();
        self.features_asked = false;
        self.live = false;
        self.loaded = false;
        self.lists = Lists::default();
        views::compression::reset(self);
        self.close_detail_quiet();
        self.connect(false);
    }

    /// Use the engine `name` (`podbag`, `docker`, `context:x`, a socket).
    pub fn switch_engine(&mut self, name: &str) -> Result<(), String> {
        if self.connection.name == name {
            return Ok(());
        }
        self.api.set_engine(name)?;
        self.reconnect();
        Ok(())
    }

    fn engine_status(&mut self, status: EngineStatus) {
        let reachable = status.reachable;
        self.engine = Some(status);
        self.refresh_vm();
        self.refresh_docker();
        if self.page == Page::Settings {
            views::compression::read(self);
        }
        if reachable {
            self.refresh();
            self.api.watch(true);
        } else {
            self.loaded = false;
        }
    }

    /// Read the four lists again.
    pub fn refresh(&mut self) {
        self.refresh_at = None;
        let session = self.session;
        self.jobs.run(move |api| Done::Lists {
            session,
            lists: Box::new(Fetched {
                containers: api.containers(),
                images: api.images(),
                volumes: api.volumes(),
                networks: api.networks(),
            }),
        });
    }

    pub fn refresh_soon(&mut self) {
        let wait = if self.jobs.inline() { 0 } else { 200 };
        self.refresh_at = Some(Instant::now() + Duration::from_millis(wait));
    }

    fn lists_done(&mut self, fetched: Fetched) {
        let Fetched {
            containers,
            images,
            volumes,
            networks,
        } = fetched;
        let l = &mut self.lists;
        let mut ok = false;
        macro_rules! take {
            ($field:ident, $result:expr, $i:expr) => {
                match $result {
                    Ok(v) => {
                        l.$field = v;
                        l.errors[$i] = None;
                        ok = true;
                    }
                    Err(e) => {
                        l.$field = Vec::new();
                        l.errors[$i] = Some(e);
                    }
                }
            };
        }
        take!(containers, containers, 0);
        take!(images, images, 1);
        take!(volumes, volumes, 2);
        take!(networks, networks, 3);
        l.index();
        self.loaded = ok;
        if !ok && let Some(e) = &mut self.engine {
            e.reachable = false;
            e.error = self.lists.errors[0].clone();
        }
        if !self.features_asked && ok && self.lists.errors[0].is_none() {
            self.features_asked = true;
            let sample = self
                .lists
                .containers
                .iter()
                .find(|c| c.state == "running")
                .map(|c| c.id.clone());
            let session = self.session;
            self.jobs.run(move |api| Done::Features {
                session,
                features: api.features(sample.as_deref()),
            });
        }
        if let Some(d) = &self.detail {
            if self.lists.find(d.kind, &d.id).is_none() {
                if self.loaded {
                    self.close_detail();
                }
            } else if d.tab == Tab::Info {
                self.load_extras();
            }
        }
    }

    // ---- actions ----

    pub fn act(&mut self, id: &str, name: &str, action: &'static str) {
        self.busy.insert(id.to_string());
        let ok = self.tv(&format!("done.{action}"), &[("name", name)]);
        let (session, id) = (self.session, id.to_string());
        self.jobs.run(move |api| Done::Action {
            session,
            removed: false,
            result: api.act(&id, action).map(|()| ok),
            id,
        });
    }

    pub fn ask_remove(&mut self, kind: Kind, i: usize) {
        let name = self.lists.name(kind, i);
        let id = self.lists.id(kind, i);
        let up = kind == Kind::Containers && is_up(&self.lists.containers[i]);
        let key = match kind {
            Kind::Containers if up => "confirm.removeRunning",
            Kind::Containers => "confirm.removeContainer",
            Kind::Images => "confirm.removeImage",
            Kind::Volumes => "confirm.removeVolume",
            Kind::Networks => "confirm.removeNetwork",
        };
        let text = self.tv(key, &[("name", &name)]);
        let then = Then::Remove {
            kind,
            id,
            name,
            force: up,
        };
        self.open_dialog(Dialog::confirm(&self.tr, text, None, then));
    }

    pub fn remove(&mut self, kind: Kind, id: String, name: String, force: bool) {
        self.busy.insert(id.clone());
        let ok = self.tv("done.remove", &[("name", &name)]);
        let session = self.session;
        self.jobs.run(move |api| Done::Action {
            session,
            removed: true,
            result: api.remove(kind, &id, force).map(|()| ok),
            id,
        });
    }

    pub fn ask_prune(&mut self, kind: Kind) {
        let key = match kind {
            Kind::Containers => "confirm.pruneContainers",
            Kind::Images => "confirm.pruneImages",
            Kind::Volumes => "confirm.pruneVolumes",
            Kind::Networks => "confirm.pruneNetworks",
        };
        let (text, yes) = (self.t(key), self.t("action.prune"));
        self.open_dialog(Dialog::confirm(
            &self.tr,
            text,
            Some(yes),
            Then::Prune(kind),
        ));
    }

    pub fn prune(&mut self, kind: Kind) {
        let session = self.session;
        self.jobs.run(move |api| Done::Pruned {
            session,
            kind,
            result: api.prune(kind),
        });
    }

    pub fn ask_rename(&mut self, i: usize) {
        let c = &self.lists.containers[i];
        let (id, name) = (c.id.clone(), crate::lists::container_name(c));
        let dialog = Dialog::ask(
            self.t("rename.title"),
            self.t("rename.label"),
            &name,
            "",
            self.t("action.rename"),
            AskFor::Rename(id),
        );
        self.open_dialog(dialog);
    }

    pub fn ask_create_volume(&mut self) {
        let title = self.t("volume.create");
        let dialog = Dialog::ask(
            title.clone(),
            self.t("col.name"),
            "",
            "data",
            title,
            AskFor::CreateVolume,
        );
        self.open_dialog(dialog);
    }

    pub fn ask_socket(&mut self) {
        let dialog = Dialog::ask(
            self.t("engine.socket"),
            self.t("settings.socket"),
            "",
            "unix:///path/to/docker.sock",
            self.t("engine.connect"),
            AskFor::Socket,
        );
        self.open_dialog(dialog);
    }

    /// Run the work of the ask dialog that is open.
    pub fn run_ask(&mut self, text: String) {
        let Some(Dialog::Ask { then, .. }) = &self.dialog else {
            return;
        };
        match then {
            AskFor::Rename(id) => {
                let id = id.clone();
                let ok = self.tv("done.rename", &[("name", &text)]);
                self.jobs
                    .run(move |api| Done::Asked(api.rename(&id, &text).map(|()| Some(ok))));
            }
            AskFor::CreateVolume => {
                self.jobs
                    .run(move |api| Done::Asked(api.create_volume(&text).map(|()| None)));
            }
            AskFor::Socket => {
                let result = self.switch_engine(&text).map(|()| None);
                self.asked(result);
            }
            AskFor::ModuleFile => {
                if let Err(e) = views::training::chose_file(self, &text) {
                    self.asked(Err(e));
                }
            }
        }
    }

    fn asked(&mut self, result: Result<Option<String>, String>) {
        let Some(Dialog::Ask {
            input,
            error,
            working,
            then,
            ..
        }) = &mut self.dialog
        else {
            return;
        };
        *working = false;
        match result {
            Ok(toast) => {
                let refresh = !matches!(then, AskFor::Socket);
                self.dialog = None;
                if let Some(text) = toast {
                    self.toasts.push(text, false);
                }
                if refresh {
                    self.refresh_soon();
                }
            }
            Err(e) => {
                *error = e;
                input.select_all();
                let id = input.id;
                self.focus_next = Some(id);
            }
        }
    }

    pub fn open_shell(&mut self, id: &str) {
        if let Err(command) = self.api.open_shell(id) {
            let text = format!("{}\n\n{command}", self.t("shell.manual"));
            let dialog = Dialog::Confirm {
                title: self.t("action.shell"),
                text,
                yes: self.t("action.copy"),
                cancel: self.t("action.back"),
                danger: false,
                then: Then::Copy(command),
            };
            self.open_dialog(dialog);
        }
    }

    pub fn open_terminal(&mut self) {
        if self.api.open_terminal("plain").is_err() {
            let text = self.t("terminal.none");
            self.toasts.push(text, true);
        }
    }

    pub fn open_dialog(&mut self, dialog: Dialog) {
        self.focus_next = Some(dialog.first_focus());
        self.dialog = Some(dialog);
    }

    pub fn copy(&mut self, text: &str, ui: &mut Ui) {
        ui.p.clipboard().write(text.to_string());
        let t = self.t("action.copied");
        self.toasts.push(t, false);
    }

    // ---- the VM ----

    pub fn refresh_vm(&mut self) {
        if !self.has_vm() {
            self.vm = None;
            return;
        }
        let session = self.session;
        self.jobs.run(move |api| Done::VmStatus {
            session,
            result: Box::new(api.vm_status()),
        });
    }

    fn vm_done(&mut self, result: Result<Option<VmStatus>, String>) {
        match result {
            Ok(vm) => {
                self.vm = vm;
                self.vm_error = None;
            }
            Err(e) => {
                self.vm = Some(VmStatus {
                    state: "unknown".into(),
                    ..VmStatus::default()
                });
                self.vm_error = Some(e);
            }
        }
        if !self.has_vm() {
            self.vm = None;
        }
        if !self.vm_form.dirty {
            views::settings::fill_vm_form(self);
        }
        views::compression::vm_changed(self);
    }

    pub fn vm_action(&mut self, action: &'static str) {
        let n = self.vm.as_ref().and_then(|v| v.containers).unwrap_or(0) as usize;
        if matches!(action, "stop" | "restart") && n > 0 {
            let (key, yes) = if action == "stop" {
                ("confirm.stopVm", "action.stopVm")
            } else {
                ("confirm.restartVm", "action.restartVm")
            };
            let (text, yes) = (self.tr.tn(key, n), self.t(yes));
            let dialog = Dialog::confirm(&self.tr, text, Some(yes), Then::Vm(action));
            self.open_dialog(dialog);
            return;
        }
        self.run_vm_action(action);
    }

    pub fn run_vm_action(&mut self, action: &str) {
        self.vm_busy = true;
        self.jobs.vm(action);
    }

    pub fn save_vm(&mut self, change: VmSettings) {
        self.vm_form.applying = true;
        let restart = change.restart;
        self.jobs.run(move |api| Done::VmSet {
            restart,
            result: api.vm_set(&change),
        });
    }

    // ---- the docker command ----

    pub fn refresh_docker(&mut self) {
        if self.docker_switching.is_none() {
            self.docker = Some(self.api.docker_command());
        }
    }

    pub fn switch_docker(&mut self, to: &str) {
        if self.docker_switching.is_some() || self.docker_requested {
            return;
        }
        self.docker_requested = true;
        self.jobs.docker_switch(to);
    }

    fn docker_switched(&mut self, result: Result<DockerCommand, String>) {
        self.docker_requested = false;
        match result {
            Ok(current) => {
                let text = if !current.installed {
                    (self.t("docker.missing"), true)
                } else if current.is_podbag {
                    (self.t("docker.nowPodbag"), false)
                } else {
                    (
                        self.tv("docker.nowOther", &[("name", &current.name)]),
                        false,
                    )
                };
                self.toasts.push(text.0, text.1);
            }
            Err(e) => {
                self.toasts.push(e, true);
                self.refresh_docker();
            }
        }
    }

    /// What the docker command does now, for the settings, the banner and the status bar.
    pub fn docker_route(&self) -> String {
        if let Some(to) = &self.docker_switching {
            let name = if to == "use" {
                "podbag".to_string()
            } else {
                self.docker
                    .as_ref()
                    .and_then(|d| d.previous.clone())
                    .unwrap_or_else(|| "default".into())
            };
            return self.tv("docker.switching", &[("name", &name)]);
        }
        match &self.docker {
            Some(d) if d.is_podbag => self.t("docker.applied"),
            Some(d) => self.tv("docker.using", &[("name", &d.name)]),
            None => String::new(),
        }
    }

    // ---- Kubernetes ----

    pub fn refresh_kube_counts(&mut self) {
        self.kube_counts_generation += 1;
        self.kube_counts.clear();
        let generation = self.kube_counts_generation;
        let (context, all) = (self.kube.context.clone(), self.kube.all_namespaces);
        self.jobs.run(move |api| Done::KubeCounts {
            generation,
            result: api.kube_counts(context.as_deref(), all),
        });
    }

    /// The list of the chosen resource.
    pub fn kube_load(&mut self) {
        self.kube.generation += 1;
        self.kube.loading = true;
        self.kube.items.clear();
        self.kube.error = None;
        self.kube.shown_context.clear();
        self.kube.table.set_offset(0.);
        let generation = self.kube.generation;
        let (kind, all, context) = (
            self.kube.kind.resource(),
            self.kube.all_namespaces,
            self.kube.context.clone(),
        );
        self.jobs.run(move |api| Done::KubeList {
            generation,
            result: api.kube_list(kind, all, context.as_deref()),
        });
    }

    /// The counts, the contexts and the list.
    pub fn kube_reload(&mut self) {
        self.refresh_kube_counts();
        self.jobs.run(|api| Done::KubeContexts(api.kube_contexts()));
        self.kube_load();
    }

    // ---- updates ----

    fn load_updates(&mut self) {
        views::updates::load(self, Check::IfDue);
    }

    // ---- navigation ----

    pub fn go(&mut self, page: Page) {
        if self.detail.is_some() {
            self.close_detail_quiet();
        }
        let fresh = self.page != page;
        self.page = page;
        if page == Page::Kubernetes {
            self.kube_expanded = true;
            if fresh {
                self.kube.search.set_content("");
                self.kube_reload();
            }
        }
        views::entered(self);
        self.remember();
    }

    pub fn go_kube(&mut self, kind: KubeKind) {
        if self.detail.is_some() {
            self.close_detail_quiet();
        }
        let changed = kind != self.kube.kind;
        self.kube.kind = kind;
        self.kube_expanded = true;
        if self.page != Page::Kubernetes {
            self.page = Page::Kubernetes;
            self.kube.search.set_content("");
            self.kube_reload();
        } else if changed {
            self.kube.search.set_content("");
            self.kube_load();
        }
        self.remember();
    }

    pub fn open_detail(&mut self, kind: Kind, id: &str, tab: Tab) {
        self.close_detail_quiet();
        self.page = Page::of(kind);
        self.detail = Some(Detail::new(kind, id.to_string(), tab));
        self.detail_scroll = ScrollState::default();
        self.enter_tab();
        self.remember();
    }

    /// Start what a detail tab needs: the log stream, the JSON.
    fn enter_tab(&mut self) {
        let Some(d) = &self.detail else { return };
        let (kind, id, session) = (d.kind, d.id.clone(), self.session);
        match d.tab {
            Tab::Logs => {
                self.logs.clear();
                self.logs_generation = 0;
                self.logs_error = None;
                self.jobs.run(move |api| Done::LogsStarted {
                    session,
                    result: api.logs_start(&id),
                    id,
                });
            }
            Tab::Inspect if !d.inspect_loaded => {
                self.jobs.run(move |api| Done::Inspect {
                    session,
                    result: api.inspect(kind, &id),
                    id,
                });
            }
            Tab::Info => self.load_extras(),
            _ => {}
        }
    }

    /// `top` and `stats` of a running container, where the engine has them.
    fn load_extras(&mut self) {
        let Some(d) = &self.detail else { return };
        if d.kind != Kind::Containers {
            return;
        }
        let running = self
            .lists
            .find(d.kind, &d.id)
            .is_some_and(|i| self.lists.containers[i].state == "running");
        if !running {
            return;
        }
        for (on, what) in [(self.features.top, "top"), (self.features.stats, "stats")] {
            if !on {
                continue;
            }
            let (id, session) = (d.id.clone(), self.session);
            self.jobs.run(move |api| Done::ContainerJson {
                session,
                what,
                result: api.container_json(&id, what),
                id,
            });
        }
    }

    pub fn set_tab(&mut self, tab: Tab) {
        let Some(d) = &mut self.detail else { return };
        if d.tab == Tab::Logs && tab != Tab::Logs {
            self.api.logs_stop();
        }
        d.tab = tab;
        self.enter_tab();
    }

    pub fn close_detail(&mut self) {
        self.close_detail_quiet();
        self.remember();
    }

    fn close_detail_quiet(&mut self) {
        if self.detail.as_ref().is_some_and(|d| d.tab == Tab::Logs) {
            self.api.logs_stop();
        }
        self.detail = None;
    }

    fn entry(&self) -> Entry {
        Entry {
            page: self.page,
            detail: self.detail.as_ref().map(|d| (d.kind, d.id.clone(), d.tab)),
            kube: (self.page == Page::Kubernetes).then_some(self.kube.kind),
            training: (self.page == Page::Training)
                .then(|| self.training.view.clone())
                .flatten(),
        }
    }

    fn remember(&mut self) {
        let e = self.entry();
        if self.history.get(self.history_at) == Some(&e) {
            return;
        }
        self.history.truncate(self.history_at + 1);
        self.history.push(e);
        if self.history.len() > 50 {
            self.history.remove(0);
        }
        self.history_at = self.history.len() - 1;
    }

    pub fn can_travel(&self, step: isize) -> bool {
        let i = self.history_at as isize + step;
        i >= 0 && (i as usize) < self.history.len()
    }

    pub fn travel(&mut self, step: isize) {
        if !self.can_travel(step) {
            return;
        }
        self.history_at = (self.history_at as isize + step) as usize;
        let e = self.history[self.history_at].clone();
        self.close_detail_quiet();
        let fresh = self.page != e.page;
        self.page = e.page;
        if let Some(kind) = e.kube {
            self.kube_expanded = true;
            if kind != self.kube.kind || fresh {
                self.kube.kind = kind;
                self.kube_load();
            }
        }
        if let Some((kind, id, tab)) = e.detail {
            self.detail = Some(Detail::new(kind, id, tab));
            self.enter_tab();
        }
        if e.page == Page::Training {
            self.training.view = e.training;
        }
        views::entered(self);
    }

    // ---- the window ----

    /// The window was hidden (closed to the tray): no polling, and the core leaves the
    /// engine alone, until it shows again.
    pub fn window_hidden(&mut self) {
        self.hidden = true;
        self.api.set_visible(false);
    }

    /// The window was shown: check the engine and the updates.
    pub fn window_shown(&mut self) {
        self.hidden = false;
        self.api.set_visible(true);
        self.connect(false);
        self.load_updates();
    }

    /// Whether the app starts at login, read once.
    pub fn launch_at_login(&mut self) -> Result<bool, String> {
        if self.launch_at_login.is_none() {
            self.launch_at_login = Some(self.api.launch_at_login());
        }
        self.launch_at_login.clone().unwrap_or(Ok(false))
    }

    /// Change it, then read it again.
    pub fn set_launch_at_login(&mut self, on: bool) {
        if let Err(e) = self.api.set_launch_at_login(on) {
            self.toasts.push(e, true);
        }
        self.launch_at_login = None;
    }

    /// Read it again (after an error).
    pub fn reread_launch_at_login(&mut self) {
        self.launch_at_login = None;
    }

    /// End the app after `after` (e.g. once `update_app` started the new one).
    pub fn quit_soon(&mut self, after: Duration) {
        self.quit_at = Some(Instant::now() + after);
    }

    /// Open the settings with one section open.
    pub fn open_section(&mut self, key: &'static str) {
        if self.page != Page::Settings || self.detail.is_some() {
            self.go(Page::Settings);
        }
        self.open_sections.insert(key);
    }

    // ---- the sidebar ----

    pub fn sidebar_width(&self) -> f32 {
        if let Some(d) = &self.sidebar_drag {
            return d.width;
        }
        if self.settings.sidebar_collapsed || (self.narrow && !self.expanded_in_narrow) {
            RAIL
        } else {
            clamp_width(self.settings.sidebar_width as f32)
        }
    }

    pub fn collapsed(&self) -> bool {
        self.sidebar_width() == RAIL
    }

    pub fn set_sidebar_width(&mut self, width: f32) {
        let collapsed = width == RAIL;
        self.settings.sidebar_collapsed = collapsed;
        if !collapsed {
            self.settings.sidebar_width = clamp_width(width) as u32;
        }
        self.expanded_in_narrow = !collapsed && (self.narrow || self.expanded_in_narrow);
        self.save_settings();
    }

    pub fn toggle_sidebar(&mut self) {
        if self.sidebar_drag.is_some() {
            return;
        }
        let w = if self.collapsed() {
            clamp_width(self.settings.sidebar_width as f32)
        } else {
            RAIL
        };
        self.set_sidebar_width(w);
    }

    // ---- the pull panel ----

    pub fn open_pull(&mut self) {
        if self.engine.as_ref().is_some_and(|e| !e.reachable) {
            return;
        }
        let locations = self.api.registries(&self.pull_hosts);
        let placeholder = self.t("pull.searchPlaceholder");
        let panel = PullPanel::new(locations, &placeholder);
        self.focus_next = Some(panel.input.id);
        self.pull = Some(panel);
    }

    // ---- events, jobs and timers ----

    /// Take in what the backend and the jobs reported, and run what is due. True if
    /// anything happened.
    pub fn pump(&mut self) -> bool {
        let mut any = false;
        for _ in 0..20 {
            let events: Vec<Event> = self.events.try_iter().collect();
            let done = self.jobs.drain();
            let due = self.timers();
            if events.is_empty() && done.is_empty() && !due {
                break;
            }
            any = true;
            for e in events {
                self.event(e);
            }
            for d in done {
                self.done(d);
            }
        }
        any
    }

    fn event(&mut self, ev: Event) {
        if ev.session().is_some_and(|s| s != self.session) {
            return; // from the engine used before
        }
        match ev {
            Event::Changed { .. } => self.refresh_soon(),
            Event::Live { on, .. } => self.live = on,
            Event::Logs {
                generation, lines, ..
            } => {
                let showing = self.detail.as_ref().is_some_and(|d| d.tab == Tab::Logs);
                if showing && (generation == self.logs_generation || self.logs_generation == 0) {
                    self.logs.push(lines);
                }
            }
            Event::PullProgress { line, .. } => {
                if let Some(p) = &mut self.pull {
                    p.progress(line);
                }
            }
            Event::PullDone { result, .. } => {
                if let Ok(name) = &result {
                    let text = self.tv("done.pull", &[("name", name)]);
                    self.toasts.push(text, false);
                    self.refresh_soon();
                }
                if let Some(p) = &mut self.pull {
                    p.done(result);
                    self.focus_next = Some(views::pull::CLOSE);
                }
            }
            Event::EngineChanged { .. } => self.connect(false),
            Event::VmStatus { state, .. } => self.vm_state(state),
            Event::DockerSwitching(to) => self.docker_switching = Some(to),
            Event::DockerChanged(cmd) => {
                self.docker_switching = None;
                self.docker = Some(cmd);
            }
            Event::Engine { status, .. } => self.engine_status(status),
            Event::VmDone { result, .. } => {
                self.vm_busy = false;
                if let Err(e) = result {
                    self.toasts.push(e, true);
                }
                self.refresh_vm();
            }
            Event::DockerSwitched(result) => self.docker_switched(result),
            Event::StatusChanged => self.status_changed = true,
            Event::MigrationProgress(step) => views::migrate::progress(self, &step),
            Event::MigrationDone(result) => views::migrate::finished(self, result),
        }
    }

    /// The VM's state changed outside the app.
    fn vm_state(&mut self, state: String) {
        let Some(e) = &mut self.engine else { return };
        if e.platform != "macos" || e.vm.is_none() || e.vm.as_deref() == Some(state.as_str()) {
            return;
        }
        e.vm = Some(state.clone());
        if state == "stopped" {
            e.reachable = false;
            self.loaded = false;
            self.refresh_at = None;
            self.api.watch(false);
        } else if !e.reachable && !self.vm_busy {
            self.connect(false);
            return;
        }
        self.refresh_vm();
    }

    fn done(&mut self, done: Done) {
        let stale = |s: &u64| *s != self.session;
        match done {
            Done::Lists { session, .. } if stale(&session) => {}
            Done::Lists { lists, .. } => self.lists_done(*lists),
            Done::Features { session, features } => {
                if !stale(&session) {
                    self.features = features;
                    if self.detail.as_ref().is_some_and(|d| d.tab == Tab::Info) {
                        self.load_extras();
                    }
                }
            }
            Done::Action {
                session,
                id,
                removed,
                result,
            } => {
                self.busy.remove(&id);
                if stale(&session) {
                    return;
                }
                match result {
                    Ok(text) => {
                        self.toasts.push(text, false);
                        if removed && self.detail.as_ref().is_some_and(|d| d.id == id) {
                            self.close_detail();
                        }
                    }
                    Err(e) => self.toasts.push(e, true),
                }
                self.refresh_soon();
            }
            Done::Pruned {
                session, result, ..
            } => {
                if stale(&session) {
                    return;
                }
                match result {
                    Ok(p) => {
                        let size = fmt::size(p.bytes, self.lang());
                        let text = self.tv(
                            "done.prune",
                            &[("n", &p.count.to_string()), ("size", &size)],
                        );
                        self.toasts.push(text, false);
                    }
                    Err(e) => self.toasts.push(e, true),
                }
                self.refresh_soon();
            }
            Done::Asked(result) => self.asked(result),
            Done::Inspect {
                session,
                id,
                result,
            } => {
                if let Some(d) = &mut self.detail
                    && !stale(&session)
                    && d.id == id
                {
                    let text = match result {
                        Ok(v) => pretty(&v),
                        Err(e) => e,
                    };
                    d.inspect.follow = false;
                    d.inspect.set_text(&text);
                    d.inspect_loaded = true;
                }
            }
            Done::ContainerJson {
                session,
                id,
                what,
                result,
            } => {
                if let Some(d) = &mut self.detail
                    && !stale(&session)
                    && d.id == id
                {
                    let text = match result {
                        Ok(v) => pretty(&v),
                        Err(e) => e,
                    };
                    if what == "top" {
                        d.top = Some(text);
                    } else {
                        d.stats = Some(text);
                    }
                }
            }
            Done::LogsStarted {
                session,
                id,
                result,
            } => {
                let showing = self
                    .detail
                    .as_ref()
                    .is_some_and(|d| d.tab == Tab::Logs && d.id == id);
                if !showing || stale(&session) {
                    return;
                }
                match result {
                    Ok(g) => self.logs_generation = g,
                    Err(e) => self.logs_error = Some(e),
                }
            }
            Done::Search { term, result } => {
                if let Some(p) = &mut self.pull {
                    p.searched(&term, result);
                }
            }
            Done::VmStatus { session, result } => {
                if !stale(&session) {
                    self.vm_done(*result);
                }
            }
            Done::VmSet { restart, result } => {
                self.vm_form.applying = false;
                match result {
                    Ok(_) => {
                        self.vm_form.dirty = false;
                        let key = if restart {
                            "settings.vmRestarted"
                        } else {
                            "settings.vmSaved"
                        };
                        let text = self.t(key);
                        self.toasts.push(text, false);
                    }
                    Err(e) => self.vm_form.error = e,
                }
                self.refresh_vm();
            }
            Done::KubeList { generation, result } => {
                if generation != self.kube.generation {
                    return;
                }
                self.kube.loading = false;
                match result {
                    Ok(list) => {
                        self.kube.items = list.items;
                        self.kube.shown_context = list.context;
                        self.kube.namespace = list.namespace;
                    }
                    Err(e) => self.kube.error = Some(e),
                }
            }
            Done::KubeContexts(result) => {
                if let Ok(names) = result {
                    self.kube.contexts = names;
                }
            }
            Done::KubeCounts { generation, result } => {
                if generation == self.kube_counts_generation
                    && let Ok(counts) = result
                {
                    self.kube_counts = counts;
                }
            }
            Done::View(apply) => apply(self),
        }
    }

    /// Run what is due; true if something ran.
    fn timers(&mut self) -> bool {
        let now = Instant::now();
        if self.quit_at.is_some_and(|t| t <= now) {
            self.quit = true;
            return true;
        }
        if self.hidden {
            return false;
        }
        let mut ran = false;
        if self.refresh_at.is_some_and(|t| t <= now) {
            self.refresh();
            ran = true;
        }
        if self.poll_at <= now {
            self.poll_at = now + Duration::from_secs(3);
            if !self.live && self.reachable() {
                self.refresh();
                ran = true;
            }
        }
        if self.vm_poll_at <= now {
            self.vm_poll_at = now + Duration::from_secs(5);
            if self.has_vm() {
                self.refresh_vm();
                ran = true;
            }
        }
        if let Some(p) = &mut self.pull
            && let Some(term) = p.due_search(now)
        {
            self.jobs.run(move |api| Done::Search {
                result: api.search(&term),
                term,
            });
            ran = true;
        }
        ran |= views::tick(self, now);
        ran
    }

    fn next_due(&self) -> Instant {
        if let Some(at) = self.quit_at {
            return at;
        }
        let mut t = self.vm_poll_at;
        if !self.live {
            t = t.min(self.poll_at);
        }
        if let Some(r) = self.refresh_at {
            t = t.min(r);
        }
        if let Some(at) = self.pull.as_ref().and_then(|p| p.search_at) {
            t = t.min(at);
        }
        if let Some(at) = views::next_due(self) {
            t = t.min(at);
        }
        t
    }

    // ---- keys ----

    pub fn in_dialog(&self) -> bool {
        self.dialog.is_some() || self.pull.is_some()
    }

    /// The window's shortcuts.
    fn keys(&mut self, ui: &mut Ui) {
        // Mouse buttons 4 and 5: back and forward.
        for b in ui.take_nav() {
            if !self.in_dialog() {
                self.travel(if b == NavButton::Back { -1 } else { 1 });
            }
        }
        let mac = cfg!(target_os = "macos");
        let keys: Vec<(usize, KeyPress)> = ui.keys().map(|(i, k)| (i, k.clone())).collect();
        for (i, k) in keys {
            let m = k.mods;
            let cmd = podbag_ui::shortcut_modifier(m);
            let letter = podbag_ui::shortcut_letter(&k);
            let ch = match &k.logical_key {
                Key::Character(s) => Some(s.as_str()),
                _ => None,
            };
            let toggle = if mac {
                m.control_key() && m.super_key() && letter == Some('s')
            } else {
                m.control_key() && !m.shift_key() && letter == Some('b')
            };
            let back = if mac {
                m.super_key() && ch == Some("[")
            } else {
                m.alt_key() && k.is(NamedKey::ArrowLeft)
            };
            let forward = if mac {
                m.super_key() && ch == Some("]")
            } else {
                m.alt_key() && k.is(NamedKey::ArrowRight)
            };
            if toggle {
                self.toggle_sidebar();
            } else if cmd && !m.shift_key() && matches!(letter, Some('f') | Some('k')) {
                if self.in_dialog() {
                    continue;
                }
                match self.search_field() {
                    Some(id) => ui.focus(Some(id)),
                    None => continue,
                }
            } else if back || forward {
                if !self.in_dialog() {
                    self.travel(if back { -1 } else { 1 });
                }
            } else if cmd && m.shift_key() && letter == Some('p') && !self.in_dialog() {
                self.open_pull();
            } else if !mac && m.control_key() && ch == Some(",") {
                self.go(Page::Settings);
            } else if mac && cmd && letter == Some('q') {
                ui.p.quit();
            } else if k.is(NamedKey::Escape) && self.detail.is_some() && !self.in_dialog() {
                self.close_detail();
            } else {
                continue;
            }
            ui.consume_key(i);
        }
    }

    /// The search box of the page, if it shows one.
    fn search_field(&self) -> Option<Id> {
        match self.page {
            Page::Kubernetes if self.kube.error.is_none() && !self.kube.loading => {
                Some(self.kube.search.id)
            }
            p => {
                let kind = p.kind()?;
                (self.detail.is_none() && self.engine.as_ref().is_none_or(|e| e.reachable))
                    .then(|| self.search[kind_index(kind)].id)
            }
        }
    }

    // ---- preview commands ----

    /// `nav <page>`, `kube <kind>`, `detail <kind> <n> [tab]`, `theme <t>`, `lang <l>`,
    /// `section <key>`, `pull [text]`, `remove <kind> <n>`, `rename <n>`, `quit`.
    pub fn command(&mut self, line: &str) {
        let mut words = line.split_whitespace();
        let (cmd, a, b, c) = (words.next(), words.next(), words.next(), words.next());
        let kind = |s: Option<&str>| s.and_then(Page::parse).and_then(Page::kind);
        let index = |s: Option<&str>| s.and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
        match cmd {
            Some("nav") => {
                if let Some(p) = a.and_then(Page::parse) {
                    self.dialog = None;
                    self.pull = None;
                    if p == Page::Training {
                        self.training.view = None;
                    }
                    self.go(p);
                }
            }
            Some("kube") => {
                if let Some(k) = KubeKind::ALL.into_iter().find(|k| Some(k.key()) == a) {
                    self.go_kube(k);
                }
            }
            Some("detail") => {
                if let Some(kind) = kind(a)
                    && index(b) < self.lists.len(kind)
                {
                    let tab = match c {
                        Some("logs") => Tab::Logs,
                        Some("inspect") => Tab::Inspect,
                        _ => Tab::Info,
                    };
                    let id = self.lists.id(kind, index(b));
                    self.open_detail(kind, &id, tab);
                }
            }
            Some("theme") => {
                let theme = match a {
                    Some("dark") => ThemePreference::Dark,
                    Some("light") => ThemePreference::Light,
                    _ => ThemePreference::System,
                };
                self.set_theme(theme);
            }
            Some("lang") => self.set_language(a.unwrap_or("system")),
            Some("section") => {
                for key in ["engine", "vm", "updates", "terminalCommand", "about"] {
                    if Some(key) == a {
                        self.open_section(key);
                    }
                }
            }
            Some("pull") => {
                self.open_pull();
                if let (Some(p), Some(text)) = (&mut self.pull, line.strip_prefix("pull ")) {
                    p.input.set_content(text.trim());
                    p.on_input(self.jobs.inline());
                }
            }
            Some("remove") => {
                if let Some(kind) = kind(a)
                    && index(b) < self.lists.len(kind)
                {
                    self.ask_remove(kind, index(b));
                }
            }
            Some("rename") => {
                if index(a) < self.lists.containers.len() {
                    self.ask_rename(index(a));
                }
            }
            Some("quit") => self.quit = true,
            _ => views::command(self, line),
        }
    }
}

pub fn clamp_width(w: f32) -> f32 {
    w.round().clamp(SIDEBAR_MIN, SIDEBAR_MAX)
}

impl UiApp for Desktop {
    type Message = Msg;

    fn ui(&mut self, ui: &mut Ui) {
        if self.pump() {
            ui.repaint();
        }
        if self.text_lang != Some(self.lang()) {
            self.text_lang = Some(self.lang());
            ui.p.text.set_language(self.lang());
        }
        if self.quit {
            ui.p.quit();
        }
        if let Some(id) = self.focus_next.take() {
            ui.focus(Some(id));
        }
        ui.wake_at(self.next_due());
        self.narrow = ui.bounds().w <= NARROW;
        self.keys(ui);
        crate::shell::paint(self, ui);
    }

    fn message(&mut self, msg: Msg, cx: &mut Cx) {
        match msg {
            Msg::Wake => {
                if self.pump() {
                    cx.notify();
                }
            }
            Msg::Command(line) => {
                self.command(&line);
                self.pump();
                cx.notify();
            }
        }
    }

    fn tick(&mut self, cx: &mut Cx) -> bool {
        let changed = self.pump();
        cx.wake_at(self.next_due());
        changed || self.toasts.expire()
    }

    fn theme(&self, system_dark: bool) -> Theme {
        crate::theme::theme(self.settings.theme, system_dark)
    }
}
