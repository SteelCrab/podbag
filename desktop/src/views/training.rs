//! Training: the installed modules, each module's
//! progress through its practice session, the JSON lesson with its local lab, and the
//! sidebar's sub-navigation. Modules are data: a module file installs them, and the
//! practice session runs in a terminal (`podbag lab start <id>`) while the window
//! follows its stage.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use podbag_desktop_core::Api;
use podbag_desktop_core::lab::Snapshot;
use podbag_desktop_core::training::LessonRecord;
use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::Kind as ButtonKind;
use podbag_ui::widgets::scroll::{ScrollState, scroll_view};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{CursorIcon, Id, Rect, TextInput, icons};
use serde_json::Value;

use crate::app::{Desktop, Page};
use crate::dialogs::{AskFor, Dialog, Then};
use crate::icons::Icon;
use crate::views::blocks::{
    self, Buttons, card, code_box, heading, input, pre, small, spawn, text,
};
use crate::views::{TOOLBAR_H, title_bar};

/// The id of the JSON lesson (the one tutorial kept by the app itself).
pub const JSON_LESSON: &str = "volume-api";
/// The JSON lesson's file format.
const JSON_FORMAT: &str = "podbag-volume-api-lab-v1";
/// A module file is at most this large.
pub const LIMIT: u64 = 64 * 1024;
/// The formats of the modules that `podbag lab` runs, by module id.
const FORMATS: [(&str, &str); 8] = [
    ("podbag-container-basics-v1", "container-basics"),
    ("podbag-web-basics-v1", "web-basics"),
    ("podbag-volume-persistence-v1", "volume-persistence"),
    ("podbag-web-service-lifecycle-v1", "web-service-lifecycle"),
    ("podbag-web-service-recovery-v1", "web-service-recovery"),
    ("podbag-container-network-v1", "container-network"),
    ("podbag-cli-gui-volumes-v1", "volume-basics"),
    ("podbag-cli-tui-volumes-v1", "tui-volumes"),
];
/// How often a shown practice is read again.
const POLL: Duration = Duration::from_millis(800);
/// The Training page syncs the catalog at most this often (Refresh always does).
const SYNC_EVERY: Duration = Duration::from_secs(60);

pub const INSTALL: Id = Id(0x7472_6169_6e01);
pub const TOGGLE: Id = Id(0x7472_6169_6e03);
pub const LAB_START: Id = Id(0x7472_6169_6e04);
pub const LAB_END: Id = Id(0x7472_6169_6e05);
pub const LAB_NAME: Id = Id(0x7472_6169_6e06);
pub const LAB_CREATE: Id = Id(0x7472_6169_6e07);
pub const LAB_SOURCE: Id = Id(0x7472_6169_6e08);
pub const LAB_SAVE: Id = Id(0x7472_6169_6e09);
pub const LAB_SERVE: Id = Id(0x7472_6169_6e0a);
pub const LAB_EDIT: Id = Id(0x7472_6169_6e0b);
pub const LAB_SEND: Id = Id(0x7472_6169_6e0c);
pub const REFRESH: Id = Id(0x7472_6169_6e0d);

/// The sidebar item of a module.
pub fn module_nav_id(id: &str) -> Id {
    Id::of(("nav-training", id))
}

/// A module's link on the overview.
pub fn link_id(id: &str) -> Id {
    Id::of(("training-link", id))
}

/// A module's "Remove module" button.
pub fn remove_id(id: &str) -> Id {
    Id::of(("training-remove", id))
}

/// A module's "Open web guide" button.
pub fn guide_id(id: &str) -> Id {
    Id::of(("training-guide", id))
}

/// An installed module, in the words of each language.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub id: String,
    pub completed: bool,
    /// Language → (title, guide).
    pub words: BTreeMap<String, (String, String)>,
    /// The course steps, as the module file names them.
    pub steps: Vec<Step>,
}

/// A step of a practice: its key, its names by language (a course's module file names
/// its steps; the app names the built-in labs' steps), and its command.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub key: String,
    pub names: BTreeMap<String, String>,
    pub command: Option<String>,
}

impl Step {
    fn lab(key: &str, command: Option<&str>) -> Step {
        Step {
            key: key.into(),
            names: BTreeMap::new(),
            command: command.map(str::to_string),
        }
    }
}

impl Record {
    fn from(r: &LessonRecord) -> Record {
        let steps = r
            .steps
            .iter()
            .map(|s| Step {
                key: s.key.clone(),
                names: s.names.clone(),
                command: s.command.clone(),
            })
            .collect();
        Record {
            id: r.id.to_string(),
            completed: r.record.completed,
            words: r
                .record
                .package
                .translations
                .iter()
                .map(|(lang, w)| (lang.clone(), (w.title.clone(), w.guide.clone())))
                .collect(),
            steps,
        }
    }

    fn words(&self, lang: &str) -> (String, String) {
        self.words
            .get(lang)
            .or_else(|| self.words.get("en"))
            .cloned()
            .unwrap_or_default()
    }

    /// The step list of a course uses the volumes view.
    pub fn uses_volumes(&self) -> bool {
        self.steps.iter().any(|s| s.key == "inspect")
    }
}

/// The JSON lesson and whether it is complete.
#[derive(Clone, Debug, PartialEq)]
pub struct Lesson {
    pub pack: Value,
    pub complete: bool,
}

impl Lesson {
    fn from_saved(v: &Value) -> Option<Lesson> {
        let complete = v["complete"].as_bool()?;
        valid_lesson(&v["pack"]).then(|| Lesson {
            pack: v["pack"].clone(),
            complete,
        })
    }

    fn to_saved(&self) -> Value {
        serde_json::json!({ "pack": self.pack, "complete": self.complete })
    }

    /// The words of `lang` (English when it has none).
    fn word(&self, lang: &str, key: &str) -> String {
        let t = &self.pack["translations"];
        let words = if t[lang].is_object() {
            &t[lang]
        } else {
            &t["en"]
        };
        words[key].as_str().unwrap_or_default().to_string()
    }
}

/// A JSON lesson with a starter object and its words.
pub fn valid_lesson(pack: &Value) -> bool {
    let text = |v: &Value| v.as_str().is_some_and(|s| !s.trim().is_empty());
    if pack["format"] != JSON_FORMAT || !text(&pack["starter"]) {
        return false;
    }
    let starter: Option<Value> = pack["starter"]
        .as_str()
        .and_then(|s| serde_json::from_str(s).ok());
    if !starter.is_some_and(|s| s.is_object()) {
        return false;
    }
    let Some(words) = pack["translations"].as_object() else {
        return false;
    };
    words.contains_key("en")
        && words.values().all(|w| {
            ["title", "goal", "create", "write", "serve", "request"]
                .iter()
                .all(|k| text(&w[*k]))
        })
}

/// A lesson's title: "Lab 1 · x" reads "Lab · x".
pub fn title_of(title: &str) -> String {
    for word in ["Lab", "실습", "実習", "实验", "Práctica", "Bài thực hành"] {
        if let Some(rest) = title.strip_prefix(&format!("{word} 1 · ")) {
            return format!("{word} · {rest}");
        }
    }
    title.to_string()
}

/// The sidebar label: the title without its "Podbag Training · " or "Lab · ".
pub fn label_of(title: &str) -> String {
    for word in [
        "Podbag Training",
        "Lab",
        "실습",
        "実習",
        "实验",
        "Práctica",
        "Bài thực hành",
    ] {
        if let Some(rest) = title.strip_prefix(&format!("{word} · ")) {
            return rest.to_string();
        }
    }
    title.to_string()
}

pub fn start_command(id: &str) -> String {
    format!("podbag lab start {id}")
}

/// Which practice view a module's session uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavor {
    /// `volume-basics`: the GUI volumes lesson.
    Volumes,
    /// `container-basics`.
    Containers,
    /// A course module with its own steps.
    Course,
}

/// The steps a module's progress shows.
#[derive(Clone, Debug)]
pub struct Practice {
    pub id: String,
    pub namespace: &'static str,
    pub steps: Vec<Step>,
    pub flavor: Flavor,
}

impl Practice {
    /// A course step's name in `lang`, else in
    /// English, else its key. Only the start of a course has an app string.
    pub fn label(&self, d: &Desktop, key: &str) -> String {
        if self.flavor != Flavor::Course || key == "start" {
            return d.t(&format!("{}{key}", self.namespace));
        }
        let names = self.steps.iter().find(|s| s.key == key).map(|s| &s.names);
        names
            .and_then(|n| n.get(d.lang()).or_else(|| n.get("en")))
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }
}

pub fn practice_for(r: &Record) -> Option<Practice> {
    let steps = |s: &[(&str, Option<&str>)]| s.iter().map(|(k, c)| Step::lab(k, *c)).collect();
    match r.id.as_str() {
        "volume-basics" => Some(Practice {
            id: r.id.clone(),
            namespace: "practice.",
            steps: steps(&[
                ("start", None),
                ("create", Some("podbag volume create my-api")),
                ("inspect", None),
                ("remove", None),
                ("verify", Some("podbag volume ls")),
                ("finish", Some("exit")),
            ]),
            flavor: Flavor::Volumes,
        }),
        "container-basics" => Some(Practice {
            id: r.id.clone(),
            namespace: "training.container.",
            steps: steps(&[
                ("start", None),
                (
                    "run",
                    Some(
                        "podbag run -d --name hello-web --network none docker.io/library/busybox:1.37.0 httpd -f -p 8080",
                    ),
                ),
                ("stop", None),
                ("check", Some("podbag ps -a")),
                ("finish", Some("exit")),
            ]),
            flavor: Flavor::Containers,
        }),
        _ if r.steps.is_empty() => None,
        _ => Some(Practice {
            id: r.id.clone(),
            namespace: "training.course.",
            steps: std::iter::once(Step::lab("start", None))
                .chain(r.steps.iter().cloned())
                .collect(),
            flavor: Flavor::Course,
        }),
    }
}

/// The state of a course session, or of the GUI or containers lesson: `practice` with
/// the right flags. A course's state counts only for its own module.
pub fn practice_state(api: &dyn Api, flavor: Flavor, module: &str) -> Result<Value, String> {
    let v = match flavor {
        Flavor::Volumes => api.practice("status", None, None, false, false)?,
        Flavor::Containers => api.practice("status", None, None, true, false)?,
        Flavor::Course => {
            let v = api.practice("status", None, None, false, true)?;
            if v["module"] != module {
                return Ok(Value::Null);
            }
            v
        }
    };
    Ok(v)
}

/// Where a module's session is: its stage (`complete` once finished), and the command
/// it shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stage {
    pub stage: String,
    pub command: Option<String>,
}

/// The JSON lesson's lab, as the core reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LabView {
    pub active: bool,
    pub volume: Option<String>,
    pub source: Option<String>,
    pub url: Option<String>,
    pub response: Option<(u16, String)>,
}

impl From<Snapshot> for LabView {
    fn from(s: Snapshot) -> LabView {
        LabView {
            active: s.active,
            volume: s.volume,
            source: s.source,
            url: s.url,
            response: s.response.map(|r| (r.status, r.body)),
        }
    }
}

pub struct TrainingState {
    pub records: Vec<Record>,
    pub lesson: Option<Lesson>,
    /// The module shown; None: all of them.
    pub view: Option<String>,
    /// The sidebar shows the modules.
    pub expanded: bool,
    /// What went wrong (an invalid file, a failed action).
    pub status: String,
    /// Each module's stage, and why it could not be read.
    pub stages: HashMap<String, Stage>,
    pub errors: HashMap<String, String>,
    pub polling: bool,
    pub poll_at: Option<Instant>,
    /// The lab of the JSON lesson: None until its state is known.
    pub lab: Option<LabView>,
    pub lab_busy: bool,
    pub editing: bool,
    pub lab_name: TextInput,
    pub lab_source: TextInput,
    pub installing: bool,
    /// The module an install in flight opens when it is done.
    pub pending_install: Option<String>,
    /// When the Training catalog was last synced, and whether a sync runs.
    pub synced_at: Option<Instant>,
    pub syncing: bool,
    pub scroll: ScrollState,
}

impl TrainingState {
    pub fn new() -> TrainingState {
        TrainingState {
            records: Vec::new(),
            lesson: None,
            view: None,
            expanded: false,
            status: String::new(),
            stages: HashMap::new(),
            errors: HashMap::new(),
            polling: false,
            poll_at: None,
            lab: None,
            lab_busy: false,
            editing: false,
            lab_name: input(LAB_NAME, "my-api"),
            lab_source: input(LAB_SOURCE, ""),
            installing: false,
            pending_install: None,
            synced_at: None,
            syncing: false,
            scroll: ScrollState::default(),
        }
    }

    /// The sidebar shows "Practice volumes".
    pub fn practice_volumes(&self) -> bool {
        self.records
            .iter()
            .any(|r| r.id == "volume-basics" || r.uses_volumes())
    }

    /// The sidebar shows "Practice containers".
    pub fn practice_containers(&self) -> bool {
        self.records
            .iter()
            .any(|r| r.id == "container-basics" || !r.steps.is_empty())
    }
}

impl Default for TrainingState {
    fn default() -> Self {
        Self::new()
    }
}

/// (id, title, label) of each module, then the JSON lesson.
pub fn entries(d: &Desktop) -> Vec<(String, String, String)> {
    let lang = d.lang();
    let mut out: Vec<(String, String, String)> = d
        .training
        .records
        .iter()
        .map(|r| {
            let title = title_of(&r.words(lang).0);
            (r.id.clone(), label_of(&title), title)
        })
        .map(|(id, label, title)| (id, title, label))
        .collect();
    if let Some(l) = &d.training.lesson {
        let title = title_of(&l.word(lang, "title"));
        out.push((JSON_LESSON.into(), title.clone(), label_of(&title)));
    }
    out
}

// ---- loading ----

/// At start: the installed modules and the JSON lesson, then a sync, so new modules
/// from podbag.dev appear without an app update.
pub fn start(d: &mut Desktop) {
    d.training.lesson = d.api.saved_lesson().as_ref().and_then(Lesson::from_saved);
    spawn(
        d,
        |api| api.training_modules("summary", None, None),
        |d, r| {
            if let Ok(records) = r {
                set_records(d, &records);
            }
            d.training.synced_at = Some(Instant::now());
            // Offline: the installed modules stay.
            spawn(
                d,
                |api| api.training_modules("sync", None, None),
                |d, r| {
                    if let Ok(records) = r {
                        set_records(d, &records);
                    }
                },
            );
        },
    );
}

/// New and changed modules from podbag.dev, on Refresh
/// (`asked`), and on the Training page at most once a minute.
pub fn sync(d: &mut Desktop, asked: bool) {
    let recent = d
        .training
        .synced_at
        .is_some_and(|at| at.elapsed() < SYNC_EVERY);
    if d.training.syncing || (!asked && recent) {
        return;
    }
    d.training.synced_at = Some(Instant::now());
    d.training.syncing = true;
    d.training.status.clear();
    tools(d, "sync", None, None);
}

fn set_records(d: &mut Desktop, records: &[LessonRecord]) {
    d.training.records = records.iter().map(Record::from).collect();
    let view = d.training.view.clone();
    if let Some(v) = view
        && v != JSON_LESSON
        && !d.training.records.iter().any(|r| r.id == v)
        && d.page == Page::Training
    {
        d.training.view = None;
    }
}

/// The modules after `action`; Err for the status line.
fn tools(d: &mut Desktop, action: &'static str, package: Option<String>, id: Option<String>) {
    let then_go = matches!(action, "install" | "remove").then(|| id.clone());
    spawn(
        d,
        move |api| api.training_modules(action, package.as_deref(), id.as_deref()),
        move |d, r| {
            d.training.installing = false;
            if action == "sync" {
                d.training.syncing = false;
            }
            match r {
                Ok(records) => {
                    set_records(d, &records);
                    // An install opens the new module; a remove of the one shown goes
                    // back to all of them.
                    match (action, then_go) {
                        ("install", _) => {
                            if let Some(new) = d.training.pending_install.take() {
                                show(d, Some(new));
                            }
                        }
                        ("remove", Some(Some(id))) if d.training.view.as_deref() == Some(&id) => {
                            show(d, None)
                        }
                        _ => {}
                    }
                    poll(d);
                }
                Err(e) => {
                    d.training.pending_install = None;
                    if d.page == Page::Training {
                        d.training.status = if action == "sync" {
                            d.tv("training.syncFailed", &[("error", &e)])
                        } else {
                            e
                        };
                    }
                }
            }
            // The page's first read is followed by a sync when the last one is old.
            if action == "status" {
                sync(d, false);
            }
        },
    );
}

/// Show Training: all modules (None) or one.
pub fn show(d: &mut Desktop, view: Option<String>) {
    d.training.view = view;
    d.training.expanded = true;
    d.go(Page::Training);
}

/// The page was (re)entered: read the modules, or open the JSON lesson's lab.
pub fn entered(d: &mut Desktop) {
    let tr = &mut d.training;
    tr.status.clear();
    tr.errors.clear();
    tr.editing = false;
    tr.scroll = ScrollState::default();
    tr.expanded = true;
    if tr.view.as_deref() == Some(JSON_LESSON) && tr.lesson.is_some() {
        tr.lab = None;
        lab(d, "status", None);
    } else {
        tools(d, "status", None, None);
    }
}

/// Read each shown practice again (watchPractice's `refresh`).
pub fn poll(d: &mut Desktop) {
    let shown: Vec<(Practice, bool)> = d
        .training
        .records
        .iter()
        .filter(|r| d.training.view.as_ref().is_none_or(|v| *v == r.id))
        .filter_map(|r| practice_for(r).map(|p| (p, r.completed)))
        .collect();
    if d.page != Page::Training || shown.is_empty() || d.training.polling {
        d.training.poll_at = None;
        return;
    }
    d.training.polling = true;
    spawn(
        d,
        move |api| {
            shown
                .into_iter()
                .map(|(p, completed)| {
                    let read = || -> Result<Stage, String> {
                        let state = practice_state(api, p.flavor, &p.id)?;
                        if let Some(stage) = state["stage"].as_str() {
                            return Ok(Stage {
                                stage: stage.to_string(),
                                command: state["command"].as_str().map(str::to_string),
                            });
                        }
                        // The shell saves completion after it ends the session.
                        let done = api
                            .training_modules("summary", None, Some(&p.id))
                            .map(|r| r.first().is_some_and(|r| r.record.completed))
                            .unwrap_or(completed);
                        let stage = if done { "complete" } else { "start" };
                        Ok(Stage {
                            stage: stage.into(),
                            command: None,
                        })
                    };
                    (p.id.clone(), read())
                })
                .collect::<Vec<_>>()
        },
        |d, results| {
            let tr = &mut d.training;
            tr.polling = false;
            for (id, r) in results {
                match r {
                    Ok(stage) => {
                        if stage.stage == "complete"
                            && let Some(rec) = tr.records.iter_mut().find(|r| r.id == id)
                        {
                            rec.completed = true;
                        }
                        tr.errors.remove(&id);
                        tr.stages.insert(id, stage);
                    }
                    Err(e) => {
                        tr.errors.insert(id, e);
                    }
                }
            }
            tr.poll_at = (d.page == Page::Training).then(|| Instant::now() + POLL);
        },
    );
}

/// Poll when it is time; true if a poll started.
pub fn tick(d: &mut Desktop, now: Instant) -> bool {
    if d.page == Page::Training && d.training.poll_at.is_some_and(|t| t <= now) {
        d.training.poll_at = None;
        poll(d);
        return true;
    }
    false
}

pub fn next_due(d: &Desktop) -> Option<Instant> {
    (d.page == Page::Training)
        .then_some(d.training.poll_at)
        .flatten()
}

// ---- installing and removing ----

/// "Install module": the module file to install.
pub fn ask_file(d: &mut Desktop) {
    let title = d.t("tutorial.install");
    let dialog = Dialog::ask(
        title.clone(),
        d.t("training.file"),
        "",
        "lesson.podbag",
        title,
        AskFor::ModuleFile,
    );
    d.open_dialog(dialog);
}

/// The text of a module file at `path`, if it is small enough.
fn read_module(path: &str) -> Result<(String, String), String> {
    let path = std::path::Path::new(path.trim());
    let path = match path.strip_prefix("~") {
        Ok(rest) => std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(rest))
            .unwrap_or_else(|| path.to_path_buf()),
        Err(_) => path.to_path_buf(),
    };
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() > LIMIT {
        return Err(String::new());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((text, name))
}

/// A module file was chosen (by path, or by a file panel): check it and ask to
/// install it. Err: the dialog shows why.
pub fn chose_file(d: &mut Desktop, path: &str) -> Result<(), String> {
    let invalid = d.t("tutorial.invalid");
    let (text, name) = read_module(path).map_err(|e| {
        if e.is_empty() {
            invalid.clone()
        } else {
            format!("{invalid} {e}")
        }
    })?;
    let pack: Value = serde_json::from_str(&text).map_err(|_| invalid.clone())?;
    let module = FORMATS
        .iter()
        .find(|(f, _)| pack["format"] == *f)
        .map(|(_, id)| id.to_string());
    let ok = match &module {
        Some(_) => pack["translations"]["en"]["title"].is_string(),
        None => valid_lesson(&pack),
    };
    if !ok {
        return Err(invalid);
    }
    let lang = d.lang();
    let words = &pack["translations"];
    let words = if words[lang].is_object() {
        &words[lang]
    } else {
        &words["en"]
    };
    let title = title_of(words["title"].as_str().unwrap_or_default());
    let label = d.t("tutorial.install");
    let question = d.tv(
        "tutorial.installConfirm",
        &[("title", &title), ("file", &name)],
    );
    let dialog = Dialog::Confirm {
        title: label.clone(),
        text: question,
        yes: label,
        cancel: d.t("action.cancel"),
        danger: false,
        then: Then::InstallModule { module, text },
    };
    d.open_dialog(dialog);
    Ok(())
}

/// Install a module file's text that the user confirmed.
pub fn install(d: &mut Desktop, module: Option<String>, text: String) {
    d.training.status.clear();
    match module {
        Some(id) => {
            d.training.installing = true;
            d.training.pending_install = Some(id);
            tools(d, "install", Some(text), None);
        }
        None => {
            let Ok(pack) = serde_json::from_str::<Value>(&text) else {
                return;
            };
            let complete = d.training.lesson.as_ref().is_some_and(|l| l.complete);
            if save_lesson(d, Some(Lesson { pack, complete })) {
                show(d, Some(JSON_LESSON.into()));
            }
        }
    }
}

/// Keep the JSON lesson. False when it could not be saved.
fn save_lesson(d: &mut Desktop, lesson: Option<Lesson>) -> bool {
    match d
        .api
        .save_lesson(lesson.as_ref().map(Lesson::to_saved).as_ref())
    {
        Ok(()) => {
            d.training.lesson = lesson;
            true
        }
        Err(_) => {
            d.training.status = d.t("tutorial.saveError");
            false
        }
    }
}

pub fn ask_remove(d: &mut Desktop, id: String) {
    let (text, yes) = (d.t("tutorial.removeNote"), d.t("tutorial.uninstall"));
    let dialog = Dialog::confirm(&d.tr, text, Some(yes), Then::RemoveModule(id));
    d.open_dialog(dialog);
}

pub fn remove(d: &mut Desktop, id: String) {
    if id == JSON_LESSON {
        if save_lesson(d, None) {
            show(d, None);
        }
        return;
    }
    tools(d, "remove", None, Some(id));
}

fn open_guide(d: &mut Desktop, id: &str) {
    let lang = d.lang();
    if let Err(e) = d.api.open_training_guide(id, lang) {
        d.toasts.push(e, true);
    }
}

// ---- the JSON lesson's lab ----

/// A lab step; the card waits until it ends.
pub fn lab(d: &mut Desktop, action: &'static str, value: Option<String>) {
    if d.training.lab_busy {
        return;
    }
    d.training.lab_busy = true;
    spawn(
        d,
        move |api| api.tutorial_lab(action, value.as_deref()),
        move |d, r| {
            d.training.lab_busy = false;
            match r {
                Ok(snapshot) => {
                    let view = LabView::from(snapshot);
                    if action == "request"
                        && view.response.is_some()
                        && let Some(l) = d.training.lesson.clone()
                        && !l.complete
                        && !save_lesson(
                            d,
                            Some(Lesson {
                                complete: true,
                                ..l
                            }),
                        )
                    {
                        return;
                    }
                    d.training.editing = false;
                    if view.volume.is_none() {
                        d.training.lab_name.set_content("");
                    }
                    d.training.lab = Some(view);
                    d.training.status.clear();
                }
                Err(e) => d.training.status = e,
            }
        },
    );
}

pub fn ask_end_lab(d: &mut Desktop) {
    let (text, yes) = (d.t("lab.endNote"), d.t("lab.end"));
    let dialog = Dialog::confirm(&d.tr, text, Some(yes), Then::EndLab);
    d.open_dialog(dialog);
}

fn edit_source(d: &mut Desktop) {
    let source = d
        .training
        .lab
        .as_ref()
        .and_then(|l| l.source.clone())
        .or_else(|| {
            d.training
                .lesson
                .as_ref()
                .and_then(|l| l.pack["starter"].as_str().map(str::to_string))
        })
        .unwrap_or_default();
    d.training.lab_source.set_content(&source);
    d.training.editing = true;
    d.focus_next = Some(LAB_SOURCE);
}

// ---- the page ----

pub fn paint(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let mut r = r;
    let bar = r.cut_top(TOOLBAR_H);
    let entries = entries(d);
    let view = d.training.view.clone();
    let title = view
        .as_ref()
        .and_then(|v| entries.iter().find(|e| e.0 == *v))
        .map(|e| e.1.clone())
        .unwrap_or_else(|| d.t("nav.tutorials"));
    let label = d.t("tutorial.install");
    let bw = podbag_ui::widgets::button::width(ui, &label, Some(Icon::Import.id()), false);
    let install = Rect::new(bar.right() - 20. - bw, bar.y + 15., bw, 32.);
    let refresh_label = d.t("action.refresh");
    let rw = podbag_ui::widgets::button::width(ui, &refresh_label, Some(Icon::Refresh.id()), false);
    let refresh = Rect::new(install.x - 8. - rw, install.y, rw, 32.);
    ui.clip(
        Rect::new(bar.x, bar.y, refresh.x - bar.x - 12., bar.h),
        |ui| title_bar(ui, bar, &title),
    );
    if podbag_ui::widgets::button::button(
        ui,
        REFRESH,
        refresh,
        &refresh_label,
        Some(Icon::Refresh.id()),
        ButtonKind::Normal,
        !d.training.syncing,
    ) {
        sync(d, true);
    }
    if podbag_ui::widgets::button::button(
        ui,
        INSTALL,
        install,
        &label,
        Some(Icon::Import.id()),
        ButtonKind::Primary,
        !d.training.installing,
    ) {
        ask_file(d);
    }
    let mut scroll = std::mem::take(&mut d.training.scroll);
    scroll_view(ui, Id::of("training-scroll"), r, &mut scroll, |ui, area| {
        let x = area.x + 20.;
        let w = (area.w - 40.).min(680.);
        let mut y = area.y + 20.;
        let t = ui.theme;
        if !d.training.status.is_empty() {
            let status = d.training.status.clone();
            y += text(ui, x, y, w, &status, t.red) + 12.;
        }
        y += library(d, ui, x, y, w) + 24.;
        y - area.y
    });
    d.training.scroll = scroll;
}

/// The modules (or the one shown); returns the height.
fn library(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let lang = d.lang();
    let view = d.training.view.clone();
    let records: Vec<Record> = d
        .training
        .records
        .iter()
        .filter(|r| view.as_ref().is_none_or(|v| *v == r.id))
        .cloned()
        .collect();
    let mut y = y0;
    for rec in &records {
        let id = rec.id.clone();
        let practice = practice_for(rec);
        let (title, guide) = rec.words(lang);
        let h = card(
            ui,
            Id::of(("training-card", &id)),
            x,
            y,
            w,
            |ui, x, y0, w| {
                let mut y = y0;
                let detail = view.is_some() && practice.is_some();
                if view.is_none() {
                    let (clicked, h) =
                        blocks::link(ui, link_id(&id), x, y, w, &title_of(&title), 16.);
                    if clicked {
                        show(d, Some(id.clone()));
                    }
                    y += h + 8.;
                } else if detail {
                    y += heading(ui, x, y, w, &d.t("training.progress"), 16.) + 8.;
                }
                let stage = d.training.stages.get(&id).cloned();
                let current_stage = stage.as_ref().map(|s| s.stage.clone()).unwrap_or_else(|| {
                    if rec.completed {
                        "complete".into()
                    } else {
                        "start".into()
                    }
                });
                match (&practice, detail) {
                    (Some(p), true) => {
                        let now = Stage {
                            stage: current_stage.clone(),
                            command: stage.and_then(|s| s.command),
                        };
                        y += progress(d, ui, (x, y, w), p, &now);
                    }
                    (Some(p), false) => {
                        let current = if current_stage == "restart" {
                            "stop".to_string()
                        } else {
                            current_stage.clone()
                        };
                        let line = match current_stage.as_str() {
                            "complete" => d.t("tutorial.complete"),
                            "start" => d.t("tutorial.installed"),
                            _ => format!("{} · {}", d.t("practice.current"), p.label(d, &current)),
                        };
                        y += small(ui, x, y, w, &line, t.ink2) + 10.;
                    }
                    (None, _) => {
                        let key = if rec.completed {
                            "tutorial.complete"
                        } else {
                            "tutorial.installed"
                        };
                        y += small(ui, x, y, w, &d.t(key), t.ink2) + 10.;
                    }
                }
                if view.is_some() && practice.is_none() {
                    y += text(ui, x, y, w, &guide, t.ink2) + 10.;
                    y += code_box(
                        d,
                        ui,
                        Id::of(("training-start", &id)),
                        x,
                        y,
                        w,
                        &start_command(&id),
                    ) + 12.;
                }
                if let Some(e) = d.training.errors.get(&id).cloned() {
                    y += small(ui, x, y, w, &e, t.red) + 8.;
                }
                let mut row = Buttons::new(x, y + 4., w);
                if practice.is_some()
                    && row.add(
                        ui,
                        guide_id(&id),
                        &d.t("training.guide"),
                        Some(Icon::Globe),
                        ButtonKind::Primary,
                        true,
                    )
                {
                    open_guide(d, &id);
                }
                if row.add(
                    ui,
                    remove_id(&id),
                    &d.t("tutorial.uninstall"),
                    Some(Icon::Remove),
                    ButtonKind::Normal,
                    true,
                ) {
                    ask_remove(d, id.clone());
                }
                y + 4. + row.height(y + 4.) - y0
            },
        );
        y += h + 16.;
    }
    let lesson = d.training.lesson.clone();
    if let Some(l) = &lesson {
        let title = title_of(&l.word(lang, "title"));
        if view.is_none() {
            let h = card(
                ui,
                Id::of(("training-card", JSON_LESSON)),
                x,
                y,
                w,
                |ui, x, y0, w| {
                    let mut y = y0;
                    let (clicked, h) = blocks::link(ui, link_id(JSON_LESSON), x, y, w, &title, 16.);
                    if clicked {
                        show(d, Some(JSON_LESSON.into()));
                    }
                    y += h + 8.;
                    let key = if l.complete {
                        "tutorial.complete"
                    } else {
                        "tutorial.installed"
                    };
                    y += small(ui, x, y, w, &d.t(key), t.ink2);
                    y - y0
                },
            );
            y += h + 16.;
        } else if view.as_deref() == Some(JSON_LESSON) {
            let h = card(ui, Id::of("training-lab"), x, y, w, |ui, x, y, w| {
                lab_card(d, ui, x, y, w, l)
            });
            y += h + 16.;
        }
    }
    let shown_lesson = lesson.is_some() && view.as_deref().is_none_or(|v| v == JSON_LESSON);
    if records.is_empty() && !shown_lesson {
        y += text(ui, x, y, w, &d.t("tutorial.empty"), t.muted) + 8.;
    }
    y - y0
}

/// Each step, done, current or pending, with the
/// command of the current one. Returns the height.
fn progress(
    d: &mut Desktop,
    ui: &mut Ui,
    (x, y0, w): (f32, f32, f32),
    p: &Practice,
    now: &Stage,
) -> f32 {
    let (stage, live) = (now.stage.as_str(), now.command.as_deref());
    let t = ui.theme;
    let complete = stage == "complete";
    let wanted = if stage == "restart" { "stop" } else { stage };
    let current = p.steps.iter().position(|s| s.key == wanted);
    let mut y = y0;
    if complete {
        y += text(ui, x, y, w, &d.t("tutorial.complete"), t.ink2) + 10.;
    }
    for (i, Step { key, command, .. }) in p.steps.iter().enumerate() {
        let done = complete || current.is_some_and(|c| i < c);
        let active = current == Some(i);
        ui.p.fill(Rect::new(x, y, w, 1.), t.line);
        let row_y = y + 10.;
        let marker = Rect::new(x, row_y + 1., 17., 17.);
        if done {
            ui.p.rounded(marker, 8.5, t.green_soft);
            ui.p.icon(icons::CHECK, marker.center(11., 11.), t.green);
        } else if active {
            ui.p.rounded(marker, 8.5, t.accent);
            ui.p.rounded(marker.center(7., 7.), 3.5, t.surface);
        } else {
            ui.p.bordered(marker, 8.5, t.surface, 1.5, t.line);
        }
        let status = d.t(if done {
            "training.done"
        } else if active {
            "practice.current"
        } else {
            "training.pending"
        });
        let status_style = ui
            .style()
            .with_size(12.)
            .weight(if active { 600 } else { 400 });
        let sl = ui.p.layout(&status, status_style, None);
        let status_color = if done {
            t.green
        } else if active {
            t.accent_text
        } else {
            t.muted
        };
        ui.p.draw_text(
            &sl,
            x + w - sl.width,
            row_y + (20. - sl.height) / 2.,
            status_color,
        );
        let name = p.label(d, key);
        let name_style = ui.style().weight(if active { 600 } else { 400 });
        let name_color = if active { t.accent_text } else { t.ink };
        let nh = ui.paragraph(
            x + 27.,
            row_y,
            w - 27. - sl.width - 12.,
            &name,
            name_style,
            name_color,
        );
        y = row_y + nh.max(20.) + 10.;
        if active {
            let line = if stage == "restart" {
                Some("exit".to_string())
            } else if key == "start" {
                Some(start_command(&p.id))
            } else {
                live.map(str::to_string).or_else(|| command.clone())
            };
            if let Some(line) = line {
                let note = if stage == "restart" {
                    Some(d.t("training.restart"))
                } else if key == "start" {
                    Some(d.t("training.terminalStart"))
                } else {
                    None
                };
                let (cx, cw) = (x + 27., w - 27.);
                y -= 4.;
                if let Some(note) = note {
                    y += text(ui, cx, y, cw, &note, t.ink2) + 6.;
                }
                y += code_box(d, ui, Id::of(("training-step", &p.id)), cx, y, cw, &line) + 10.;
            }
        }
    }
    if complete {
        y += 8.;
        y += text(ui, x, y, w, &d.t("training.terminalStart"), t.ink2) + 6.;
        y += code_box(
            d,
            ui,
            Id::of(("training-again", &p.id)),
            x,
            y,
            w,
            &start_command(&p.id),
        ) + 10.;
    }
    y + 6. - y0
}

/// The heading of the lab card: an icon and the title.
fn lab_title(ui: &mut Ui, x: f32, y: f32, w: f32, icon: Icon, title: &str) -> f32 {
    let t = ui.theme;
    let style = ui.style().with_size(20.).weight(700).line_height(27.);
    let layout = ui.p.layout_wrapped(title, style, w - 40.);
    ui.p.icon(
        icon.id(),
        Rect::new(x, y + (layout.height - 28.).max(0.) / 2., 28., 28.),
        t.accent_text,
    );
    ui.p.draw_text(&layout, x + 40., y, t.ink);
    layout.height.max(28.) + 16.
}

/// A JSON lesson's lab: its steps and their results. Returns the height.
fn lab_card(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32, l: &Lesson) -> f32 {
    let t = ui.theme;
    let lang = d.lang();
    let busy = d.training.lab_busy;
    let mut y = y0;
    let shown = d.training.lab.clone().filter(|l| l.active);
    let Some(state) = shown else {
        // Not started: the goal, Start and Remove.
        y += lab_title(
            ui,
            x,
            y,
            w,
            Icon::Tutorials,
            &title_of(&l.word(lang, "title")),
        );
        let key = if l.complete {
            "tutorial.complete"
        } else {
            "tutorial.installed"
        };
        y += small(ui, x, y, w, &d.t(key), t.ink2) + 10.;
        y += text(ui, x, y, w, &l.word(lang, "goal"), t.ink) + 14.;
        let mut row = Buttons::new(x, y, w);
        if row.add(
            ui,
            LAB_START,
            &d.t("lab.start"),
            Some(Icon::Start),
            ButtonKind::Primary,
            !busy,
        ) {
            lab(d, "start", None);
        }
        if row.add(
            ui,
            remove_id(JSON_LESSON),
            &d.t("tutorial.uninstall"),
            Some(Icon::Remove),
            ButtonKind::Normal,
            !busy,
        ) {
            ask_remove(d, JSON_LESSON.into());
        }
        return y + row.height(y) - y0;
    };
    let file = format!("{}/response.json", state.volume.clone().unwrap_or_default());
    let mono = podbag_ui::TextStyle::new(13.).mono().line_height(18.);
    let end = |d: &mut Desktop, ui: &mut Ui, row: &mut Buttons| {
        if row.add(
            ui,
            LAB_END,
            &d.t("lab.end"),
            Some(Icon::Stop),
            ButtonKind::Normal,
            !busy,
        ) {
            ask_end_lab(d);
        }
    };
    if state.volume.is_none() {
        y += lab_title(ui, x, y, w, Icon::Volumes, &d.t("lab.volumeStep"));
        y += text(ui, x, y, w, &l.word(lang, "create"), t.ink) + 14.;
        let label_style = ui.style().with_size(13.).weight(600);
        ui.p.label(
            Rect::new(x, y, w, 18.),
            &d.t("lab.name"),
            label_style,
            t.ink,
        );
        y += 24.;
        text_field(
            ui,
            &mut d.training.lab_name,
            Rect::new(x, y, w.min(360.), 34.),
            None,
            !busy,
        );
        let enter = ui.has_focus(LAB_NAME) && ui.key_pressed(NamedKey::Enter).is_some();
        y += 46.;
        let mut row = Buttons::new(x, y, w);
        let create = row.add(
            ui,
            LAB_CREATE,
            &d.t("lab.create"),
            Some(Icon::Volumes),
            ButtonKind::Primary,
            !busy,
        );
        end(d, ui, &mut row);
        let name = d.training.lab_name.content().trim().to_string();
        if (create || enter) && !name.is_empty() {
            lab(d, "create", Some(name));
        }
        return y + row.height(y) - y0;
    }
    if state.source.is_none() || d.training.editing {
        if !d.training.editing {
            edit_source(d);
            d.focus_next = None;
        }
        y += lab_title(ui, x, y, w, Icon::Logs, &d.t("lab.fileStep"));
        y += text(ui, x, y, w, &l.word(lang, "write"), t.ink) + 14.;
        ui.p.label(Rect::new(x, y, w, 18.), &file, mono, t.ink);
        y += 24.;
        text_field(
            ui,
            &mut d.training.lab_source,
            Rect::new(x, y, w, 34.),
            None,
            !busy,
        );
        y += 46.;
        let mut row = Buttons::new(x, y, w);
        if row.add(
            ui,
            LAB_SAVE,
            &d.t("lab.save"),
            Some(Icon::Check),
            ButtonKind::Primary,
            !busy,
        ) {
            let source = d.training.lab_source.content().to_string();
            lab(d, "save", Some(source));
        }
        end(d, ui, &mut row);
        return y + row.height(y) - y0;
    }
    if state.url.is_none() {
        y += lab_title(ui, x, y, w, Icon::Engine, &d.t("lab.serveStep"));
        y += text(ui, x, y, w, &l.word(lang, "serve"), t.ink) + 14.;
        ui.p.label(Rect::new(x, y, w, 18.), &file, mono, t.ink);
        y += 24.;
        y += pre(
            ui,
            Id::of("lab-source-view"),
            x,
            y,
            w,
            state.source.as_deref().unwrap_or_default(),
        ) + 14.;
        let mut row = Buttons::new(x, y, w);
        if row.add(
            ui,
            LAB_SERVE,
            &d.t("lab.serve"),
            Some(Icon::Start),
            ButtonKind::Primary,
            !busy,
        ) {
            lab(d, "serve", None);
        }
        if row.add(
            ui,
            LAB_EDIT,
            &d.t("lab.edit"),
            Some(Icon::Rename),
            ButtonKind::Normal,
            !busy,
        ) {
            edit_source(d);
        }
        end(d, ui, &mut row);
        return y + row.height(y) - y0;
    }
    let answered = state.response.is_some();
    let (icon, key) = if answered {
        (Icon::Check, "lab.responseStep")
    } else {
        (Icon::Networks, "lab.requestStep")
    };
    y += lab_title(ui, x, y, w, icon, &d.t(key));
    let note = if answered {
        d.t("lab.resultNote")
    } else {
        l.word(lang, "request")
    };
    y += text(ui, x, y, w, &note, t.ink) + 14.;
    y += code_box(
        d,
        ui,
        Id::of("lab-url"),
        x,
        y,
        w,
        state.url.as_deref().unwrap_or_default(),
    ) + 12.;
    if let Some((status, body)) = &state.response {
        y += pre(
            ui,
            Id::of("lab-response"),
            x,
            y,
            w,
            &format!("HTTP {status}\n{body}"),
        ) + 14.;
    }
    let mut row = Buttons::new(x, y, w);
    if row.add(
        ui,
        LAB_SEND,
        &d.t("lab.send"),
        Some(Icon::Forward),
        ButtonKind::Primary,
        !busy,
    ) {
        lab(d, "request", None);
    }
    if row.add(
        ui,
        LAB_EDIT,
        &d.t("lab.edit"),
        Some(Icon::Rename),
        ButtonKind::Normal,
        !busy,
    ) {
        edit_source(d);
    }
    end(d, ui, &mut row);
    y + row.height(y) - y0
}

// ---- the sidebar ----

/// The Training item with its module list, then the practice items (updateTrainingNav
/// and renderNav's `nav-practice`). `y` moves down past them.
pub fn nav(d: &mut Desktop, ui: &mut Ui, inner: Rect, y: &mut f32, collapsed: bool) {
    let t = ui.theme;
    let entries = entries(d);
    let page = d.page;
    let view = d.training.view.clone();
    let expanded = !entries.is_empty() && d.training.expanded;
    let r = Rect::new(inner.x, *y, inner.w, 34.);
    *y += 36.;
    let open_r = if collapsed || entries.is_empty() {
        r
    } else {
        r.inset(0., 0., 32., 0.)
    };
    let in_module = page == Page::Training && view.is_some();
    if in_module && !expanded {
        ui.p.rounded(open_r, 7., t.accent_soft);
    }
    let label = d.t("nav.tutorials");
    if crate::shell::nav_item(
        ui,
        open_r,
        crate::shell::nav_id(Page::Training),
        Icon::Tutorials,
        &label,
        None,
        page == Page::Training && view.is_none(),
        collapsed,
    ) {
        show(d, None);
    }
    if !collapsed && !entries.is_empty() {
        let tr = Rect::new(r.right() - 30., r.y + 2., 30., 30.);
        if toggle(ui, TOGGLE, tr, expanded, &d.t("training.modules")) {
            d.training.expanded = !d.training.expanded;
        }
    }
    if expanded {
        let sub_x = if collapsed { 0. } else { 28. };
        let top = *y;
        for (id, title, label) in &entries {
            let r = Rect::new(inner.x, *y, inner.w, 30.).inset(sub_x, 0., 0., 0.);
            *y += 32.;
            let current = page == Page::Training && view.as_deref() == Some(id.as_str());
            if crate::shell::nav_item(
                ui,
                r,
                module_nav_id(id),
                Icon::Tutorials,
                label,
                None,
                current,
                collapsed,
            ) {
                show(d, Some(id.clone()));
            }
            if !collapsed {
                ui.tooltip(module_nav_id(id).with("tip"), r, title);
            }
        }
        if !collapsed {
            ui.p.fill(Rect::new(inner.x + 18., top, 1., *y - top - 2.), t.line);
        }
    }
    let practice = [
        (
            d.training.practice_containers(),
            Page::LabContainers,
            "training.containers",
            Icon::Containers,
        ),
        (
            d.training.practice_volumes(),
            Page::LabVolumes,
            "practice.volumes",
            Icon::Volumes,
        ),
    ];
    let shown = practice.iter().any(|p| p.0);
    if shown {
        *y += 4.;
    }
    for (on, page_of, key, icon) in practice {
        if !on {
            continue;
        }
        let r = Rect::new(inner.x, *y, inner.w, 34.);
        *y += 36.;
        let label = d.t(key);
        if crate::shell::nav_item(
            ui,
            r,
            crate::shell::nav_id(page_of),
            icon,
            &label,
            None,
            page == page_of,
            collapsed,
        ) {
            d.go(page_of);
        }
    }
}

/// The disclosure button right of a sidebar item; true when clicked.
pub fn toggle(ui: &mut Ui, id: Id, r: Rect, expanded: bool, tip: &str) -> bool {
    let t = ui.theme;
    if ui.hovered(id) {
        ui.p.bordered(r, 7., t.hover, 1., t.line);
    }
    let icon = if expanded {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    ui.p.icon(icon, r.center(14., 14.), t.ink2);
    ui.p.hitbox(r, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, r, 7.);
    ui.tooltip(id, r, tip);
    ui.clicked(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lose_the_lab_number_and_labels_the_prefix() {
        assert_eq!(title_of("Lab 1 · My HTTP lab"), "Lab · My HTTP lab");
        assert_eq!(title_of("실습 1 · 볼륨"), "실습 · 볼륨");
        assert_eq!(
            label_of("Podbag Training · Run a container"),
            "Run a container"
        );
        assert_eq!(label_of("Lab · My HTTP lab"), "My HTTP lab");
        assert_eq!(label_of("Plain"), "Plain");
    }

    #[test]
    fn a_json_lesson_needs_a_starter_object_and_all_its_words() {
        let mut pack = serde_json::json!({
            "format": JSON_FORMAT, "starter": "{\"message\":\"hello\"}",
            "translations": { "en": { "title": "Lab 1 · x", "goal": "g", "create": "c",
                "write": "w", "serve": "s", "request": "r" } },
        });
        assert!(valid_lesson(&pack));
        pack["starter"] = "[1]".into();
        assert!(!valid_lesson(&pack));
        pack["starter"] = "{}".into();
        pack["translations"]["ko"] = serde_json::json!({ "title": "t" });
        assert!(!valid_lesson(&pack));
    }
}
