//! Training on sample data: the installed modules, the Training catalog, a practice
//! session, the tutorial lab and the JSON lesson, all in memory.
//!
//! Module files go through podbag's own parser, and a course session moves on by the
//! checks in its module file. A real practice session starts in a terminal
//! (`podbag lab start <id>`) and moves on there; the window only watches it. Here two
//! extra actions stand in for the terminal: `practice("start", None, Some(id), ..)`
//! starts a session of the module `id`, and `practice("advance", ..)` does the current
//! step's terminal part.

use crate::training::LessonRecord;
use podbag::client::{ContainerSummary, Port, VolumeSummary};
use podbag::dashboard::lab::{Reply, Snapshot};
use podbag::volume_lesson::course::{self, Check};
use podbag::volume_lesson::{Installed, Module, Package, catalog, parse_package};
use serde_json::{Value, json};

const VOLUME_BASICS: &str = r#"{
  "format": "podbag-cli-gui-volumes-v1",
  "translations": {
    "en": { "title": "Podbag Training · Volumes in the CLI and the GUI",
      "guide": "Follow the steps in your terminal and in Podbag Desktop." },
    "ko": { "title": "Podbag Training · CLI와 GUI의 볼륨",
      "guide": "터미널과 Podbag Desktop에서 단계를 따라 하세요." }
  }
}"#;

const TUI_VOLUMES: &str = r#"{
  "format": "podbag-cli-tui-volumes-v1",
  "translations": {
    "en": { "title": "Podbag Training · Volumes in the TUI",
      "guide": "Follow the steps in your terminal." },
    "ko": { "title": "Podbag Training · TUI의 볼륨",
      "guide": "터미널에서 단계를 따라 하세요." }
  }
}"#;

/// The Training catalog of the mock, in its order: the module files a sync installs.
const CATALOG: [(&str, &str); 5] = [
    (
        "container-basics",
        include_str!("../../labs/container-basics/container-basics.podbag"),
    ),
    (
        "web-basics",
        include_str!("../../tests/fixtures/training/web-basics.podbag"),
    ),
    (
        "volume-persistence",
        include_str!("../../tests/fixtures/training/volume-persistence.podbag"),
    ),
    (
        "web-service-recovery",
        include_str!("../../tests/fixtures/training/web-service-recovery.podbag"),
    ),
    ("volume-basics", VOLUME_BASICS),
];

/// The module file of `id`, as the Training catalog has it.
pub fn sample_module(id: &str) -> Option<String> {
    if id == "tui-volumes" {
        return Some(TUI_VOLUMES.into());
    }
    CATALOG
        .iter()
        .find(|(m, _)| *m == id)
        .map(|(_, text)| text.to_string())
}

/// Which practice view a session uses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// `volume-basics`: the GUI volumes lesson.
    Volumes,
    /// `container-basics`.
    Containers,
    /// A course module, with the steps of its module file.
    Course,
}

/// A step of a session; only a course step has a check.
struct Step {
    key: String,
    command: Option<String>,
    check: Option<Check>,
}

impl Step {
    fn lab(key: &str) -> Step {
        Step {
            key: key.into(),
            command: None,
            check: None,
        }
    }
}

struct Session {
    module: String,
    flavor: Flavor,
    id: String,
    /// The steps after "start".
    steps: Vec<Step>,
    at: usize,
    port: u16,
    containers: Vec<ContainerSummary>,
    volumes: Vec<VolumeSummary>,
}

/// The port of a course without its own.
const PORT: u16 = 18080;

fn container(name: &str, state: &str, port: Option<u16>) -> ContainerSummary {
    ContainerSummary {
        id: format!(
            "{:0>16x}",
            name.bytes().fold(7u64, |h, b| h * 31 + b as u64)
        ),
        names: vec![format!("/{name}")],
        image: podbag::volume_lesson::containers::IMAGE.into(),
        command: "httpd -f -p 8080".into(),
        created: 0,
        status: match state {
            "running" => "Up 5 seconds",
            "created" => "Created",
            _ => "Exited (0) 1 second ago",
        }
        .into(),
        state: state.into(),
        ports: port
            .map(|public| Port {
                ip: "127.0.0.1".into(),
                private_port: 8080,
                public_port: public,
                kind: "tcp".into(),
            })
            .into_iter()
            .collect(),
    }
}

fn volume(name: &str) -> VolumeSummary {
    VolumeSummary {
        name: name.into(),
        driver: "local".into(),
        mountpoint: format!("/var/lib/podbag/volumes/{name}/_data"),
        created_at: "2026-11-26T09:00:00.000Z".into(),
    }
}

impl Session {
    fn step(&self) -> Option<&Step> {
        self.steps.get(self.at)
    }

    fn key(&self) -> &str {
        self.step().map_or("finish", |s| s.key.as_str())
    }

    fn check(&self) -> Option<&Check> {
        self.step().and_then(|s| s.check.as_ref())
    }

    fn row(&self, name: &str) -> Option<&ContainerSummary> {
        self.containers.iter().find(|c| c.name() == name)
    }

    /// The container `name` gets this state.
    fn set_state(&mut self, name: &str, state: &str, status: String) {
        if let Some(c) = self.containers.iter_mut().find(|c| c.name() == name) {
            c.state = state.into();
            c.status = status;
        }
    }

    fn run(&mut self, name: &str, state: &str, port: Option<u16>) {
        self.containers.retain(|c| c.name() != name);
        self.containers.push(container(name, state, port));
    }

    fn snapshot(&self, detail: Option<Value>) -> Value {
        let key = self.key();
        match self.flavor {
            Flavor::Volumes => json!({
                "session": self.id, "stage": key, "volumes": self.volumes, "detail": detail,
            }),
            Flavor::Containers => json!({
                "session": self.id, "stage": key, "containers": self.containers,
            }),
            // As `course::view` reports it.
            Flavor::Course => {
                let check = self.check();
                let web = match check {
                    Some(Check::DesktopPorts { name } | Check::DesktopBrowser { name, .. }) => {
                        Some(json!({
                            "container": name, "port": self.port,
                            "ready": matches!(check, Some(Check::DesktopBrowser { .. })),
                            "url": format!("http://127.0.0.1:{}/", self.port),
                        }))
                    }
                    _ => None,
                };
                let named = |wanted: fn(&Check) -> Option<&String>| check.and_then(wanted);
                json!({
                    "module": self.module, "session": self.id, "stage": key,
                    "command": self.step().and_then(|s| s.command.clone()),
                    "containers": self.containers, "volumes": self.volumes, "detail": detail,
                    "web": web,
                    "stop": named(|c| match c {
                        Check::DesktopStop { name } => Some(name),
                        _ => None,
                    }),
                    "inspect_container": named(|c| match c {
                        Check::DesktopInspectExited { name } => Some(name),
                        _ => None,
                    }),
                    "remove_volume": named(|c| match c {
                        Check::DesktopDeleteVolume { name } => Some(name),
                        _ => None,
                    }),
                })
            }
        }
    }

    /// What the current step changes when it is done.
    fn effect(&mut self) {
        let Some(check) = self.check().cloned() else {
            // The built-in labs.
            match (self.flavor, self.key()) {
                (Flavor::Containers, "run") => self.run("hello-web", "running", None),
                (Flavor::Containers, "stop") => {
                    self.set_state("hello-web", "exited", "Exited (137) 1 second ago".into())
                }
                (Flavor::Volumes, "create") => self.volumes.push(volume("my-api")),
                (Flavor::Volumes, "remove") => self.volumes.clear(),
                _ => {}
            }
            return;
        };
        match check {
            Check::ContainerPublished { name, .. } => self.run(&name, "running", Some(self.port)),
            Check::ContainerRunning { name } => self.run(&name, "running", None),
            Check::ContainerCreated { name } => self.run(&name, "created", None),
            Check::ContainerExited { name, code } => {
                self.run(&name, "exited", None);
                self.set_state(&name, "exited", format!("Exited ({code}) 1 second ago"));
            }
            Check::ContainerStopped { name } | Check::DesktopStop { name } => {
                self.set_state(&name, "exited", "Exited (137) 1 second ago".into());
            }
            Check::ContainerAbsent { name } => self.containers.retain(|c| c.name() != name),
            Check::ContainersEmpty => self.containers.clear(),
            Check::VolumeExists { name } => {
                self.volumes.retain(|v| v.name != name);
                self.volumes.push(volume(&name));
            }
            Check::VolumeAbsent { name } | Check::DesktopDeleteVolume { name } => {
                self.volumes.retain(|v| v.name != name);
            }
            Check::VolumesEmpty => self.volumes.clear(),
            _ => {}
        }
    }

    /// Do the current step and move on.
    fn advance(&mut self) {
        self.effect();
        self.at += 1;
    }

    /// The window's action on a course step, as `course::view` allows it.
    fn course_action(&mut self, action: &str, target: &str) -> Result<Option<Value>, String> {
        let check = self.check().cloned();
        let id_of = |s: &Session, name: &str| {
            s.row(name)
                .map(|c| (c.id.clone(), c.state.clone()))
                .ok_or("The practice container was not found.")
        };
        match (action, &check) {
            ("inspect-failed", Some(Check::DesktopInspectExited { name }))
                if id_of(self, name)? == (target.to_string(), "exited".to_string()) =>
            {
                self.advance();
            }
            ("ports", Some(Check::DesktopPorts { name }))
            | ("open-web", Some(Check::DesktopBrowser { name, .. }))
            | ("stop", Some(Check::DesktopStop { name }))
                if id_of(self, name)?.0 == target =>
            {
                self.advance();
            }
            ("show-web", Some(Check::DesktopBrowser { name, .. }))
                if id_of(self, name)?.0 == target => {}
            ("inspect", _) if self.volumes.iter().any(|v| v.name == target) => {
                if matches!(&check, Some(Check::DesktopInspectVolume { name }) if name == target) {
                    self.advance();
                }
                return Ok(Some(inspect(self, target)));
            }
            ("remove", Some(Check::DesktopDeleteVolume { name })) if name == target => {
                self.advance();
            }
            _ => return Err("This action is unavailable for the current practice step.".into()),
        }
        Ok(None)
    }
}

/// The inspect JSON of the session's volume `name`.
fn inspect(s: &Session, name: &str) -> Value {
    let v = s.volumes.iter().find(|v| v.name == name);
    v.map_or(Value::Null, |v| {
        json!({
            "Name": v.name, "Driver": v.driver, "Mountpoint": v.mountpoint,
            "CreatedAt": v.created_at, "Scope": "local",
        })
    })
}

#[derive(Default)]
struct TutorialLab {
    active: bool,
    volume: Option<String>,
    source: Option<String>,
    url: Option<String>,
    response: Option<Reply>,
}

#[derive(Default)]
pub(crate) struct Training {
    /// The Training catalog cannot be read (the `offline` scenario).
    offline: bool,
    /// The installed modules: the module file and whether it is complete.
    modules: Vec<(Module, Package, bool)>,
    session: Option<Session>,
    sessions: u32,
    lab: TutorialLab,
    lesson: Option<Value>,
}

fn record(module: &Module, package: &Package, completed: bool) -> Result<LessonRecord, String> {
    let mut package = package.clone();
    package.animation = None;
    // An old course format has no steps until a sync replaces it.
    let steps = package
        .course()
        .map(|c| course::instructions(&c))
        .unwrap_or_default();
    let record: Installed = serde_json::from_value(json!({
        "package": package, "completed": completed, "generation": "mock",
    }))
    .map_err(|e| e.to_string())?;
    Ok(LessonRecord {
        id: module.id().to_owned(),
        steps,
        record,
    })
}

/// The place of a module in the catalog's order.
fn rank(module: &Module) -> (usize, String) {
    let id = module.id();
    let at = CATALOG.iter().position(|(m, _)| *m == id);
    (at.unwrap_or(usize::MAX), id.to_owned())
}

impl Training {
    pub(crate) fn new(offline: bool) -> Training {
        Training {
            offline,
            ..Training::default()
        }
    }

    /// `volume_lesson::install`: an update keeps the completion, and an old course file
    /// does not replace the course's steps.
    fn install(&mut self, text: &str) -> Result<(), String> {
        let package = parse_package(text).map_err(|e| e.to_string())?;
        let module = Module::of(&package).map_err(|e| e.to_string())?;
        let current = self.modules.iter().position(|(m, _, _)| *m == module);
        let completed = current.is_some_and(|i| self.modules[i].2);
        if let Some(i) = current {
            if !course::is_course(&package.format) && course::is_course(&self.modules[i].1.format) {
                return Ok(());
            }
            self.modules.remove(i);
        }
        self.modules.push((module, package, completed));
        self.modules.sort_by_key(|(m, _, _)| rank(m));
        Ok(())
    }

    /// `catalog::sync`: the catalog's modules; offline, nothing changes.
    fn sync(&mut self) -> Result<(), String> {
        if self.offline {
            return Err(format!(
                "cannot reach {}: the network is unreachable",
                catalog::URL
            ));
        }
        CATALOG.iter().try_for_each(|(_, text)| self.install(text))
    }

    /// The course of an installed module; `podbag lab start` syncs first when it is
    /// missing or has an old format.
    fn course_of(&mut self, module: &Module) -> Result<course::Course, String> {
        let find = |t: &Training| {
            t.modules
                .iter()
                .find(|(m, _, _)| m == module)
                .and_then(|(_, p, _)| p.course())
        };
        if let Some(c) = find(self) {
            return Ok(c);
        }
        self.sync()?;
        find(self).ok_or_else(|| "Unknown practice module. Run podbag lab list.".into())
    }

    pub(crate) fn modules(
        &mut self,
        action: &str,
        package: Option<&str>,
        id: Option<&str>,
    ) -> Result<Vec<LessonRecord>, String> {
        match action {
            "status" | "summary" => {}
            "install" => self.install(package.ok_or("Choose a module file.")?)?,
            "remove" => {
                let module = Module::parse(id.unwrap_or_default()).map_err(|e| e.to_string())?;
                self.modules.retain(|(m, _, _)| *m != module);
            }
            "sync" => self.sync()?,
            _ => return Err("Unknown lesson action.".into()),
        }
        self.modules
            .iter()
            .filter(|(m, _, _)| action != "summary" || id.is_none_or(|id| id == m.id()))
            .map(|(m, pack, done)| record(m, pack, *done))
            .collect()
    }

    pub(crate) fn practice(
        &mut self,
        action: &str,
        session: Option<&str>,
        name: Option<&str>,
        containers: bool,
        course: bool,
    ) -> Result<Value, String> {
        if action == "start" {
            return self.start(name.unwrap_or_default()).map(|()| Value::Null);
        }
        let wanted = if course {
            Flavor::Course
        } else if containers {
            Flavor::Containers
        } else {
            Flavor::Volumes
        };
        let Some(s) = self.session.as_mut().filter(|s| s.flavor == wanted) else {
            return if action == "status" || action == "advance" {
                Ok(Value::Null)
            } else {
                Err("The practice session has ended.".into())
            };
        };
        if action == "status" {
            return Ok(s.snapshot(None));
        }
        if action == "advance" {
            let in_desktop = match s.check() {
                Some(check) => check.in_desktop(),
                None => matches!(
                    (s.flavor, s.key()),
                    (Flavor::Containers, "stop") | (Flavor::Volumes, "inspect" | "remove")
                ),
            };
            if in_desktop {
                return Err("This step is done in Podbag Desktop.".into());
            }
            if s.check()
                .map_or(s.key() == "finish", |c| *c == Check::Finish)
            {
                // The session ends and the module is complete.
                let module = s.module.clone();
                self.session = None;
                if let Some(m) = self.modules.iter_mut().find(|(m, _, _)| m.id() == module) {
                    m.2 = true;
                }
                return Ok(Value::Null);
            }
            s.advance();
            return Ok(s.snapshot(None));
        }
        if session != Some(s.id.as_str()) {
            return Err("The practice session has changed.".into());
        }
        let target = name.unwrap_or_default();
        if s.flavor == Flavor::Course {
            let detail = s.course_action(action, target)?;
            return Ok(s.snapshot(detail));
        }
        let key = s.key().to_string();
        let mut detail = None;
        match (s.flavor, action) {
            (_, "inspect") => {
                if !s.volumes.iter().any(|v| v.name == target) {
                    return Err("The practice volume was not found.".into());
                }
                detail = Some(inspect(s, target));
                if key == "inspect" {
                    s.advance();
                }
            }
            (Flavor::Volumes, "remove") if key == "remove" && target == "my-api" => s.advance(),
            (Flavor::Containers, "stop") if key == "stop" => {
                let c = s
                    .containers
                    .iter()
                    .find(|c| c.id == target)
                    .ok_or("The practice container was not found.")?;
                if c.state != "running" {
                    return Err("The practice container cannot perform that action.".into());
                }
                s.advance();
            }
            _ => return Err("This action is unavailable for the current practice step.".into()),
        }
        Ok(s.snapshot(detail))
    }

    /// The terminal's `podbag lab start <id>`.
    fn start(&mut self, id: &str) -> Result<(), String> {
        let module = Module::parse(id).map_err(|e| e.to_string())?;
        let (flavor, steps, port) = match &module {
            Module::Containers => (
                Flavor::Containers,
                Vec::from(["run", "stop", "check", "finish"].map(Step::lab)),
                PORT,
            ),
            Module::Volumes => (
                Flavor::Volumes,
                Vec::from(["create", "inspect", "remove", "verify", "finish"].map(Step::lab)),
                PORT,
            ),
            Module::Tui => return Err("This module has no practice session.".into()),
            Module::Course(_) => {
                let c = self.course_of(&module)?;
                let port = c.port.unwrap_or(PORT);
                let steps = c
                    .steps
                    .iter()
                    .map(|s| Step {
                        key: s.key.clone(),
                        command: s.command(port),
                        check: Some(s.check.with_port(port)),
                    })
                    .collect();
                (Flavor::Course, steps, port)
            }
        };
        self.sessions += 1;
        self.session = Some(Session {
            module: module.id().to_owned(),
            flavor,
            id: format!("podbag-practice-{}", self.sessions),
            steps,
            at: 0,
            port,
            containers: Vec::new(),
            volumes: Vec::new(),
        });
        Ok(())
    }

    pub(crate) fn tutorial_lab(
        &mut self,
        action: &str,
        value: Option<&str>,
    ) -> Result<Snapshot, String> {
        crate::training::lab_action(action)?;
        let lab = &mut self.lab;
        match action {
            "status" => {}
            "start" => lab.active = true,
            "end" => *lab = TutorialLab::default(),
            _ if !lab.active => return Err("Start the lab first.".into()),
            "create" => {
                let name = value
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or("Enter a volume name.")?;
                if name.contains('/') || name.starts_with('.') {
                    return Err(format!("invalid volume name {name:?}"));
                }
                match &lab.volume {
                    Some(v) if v != name => return Err("This lab already has a volume.".into()),
                    _ => lab.volume = Some(name.into()),
                }
            }
            "save" => {
                lab.volume.as_ref().ok_or("Create a volume first.")?;
                let source = value.ok_or("Enter a JSON object.")?;
                serde_json::from_str::<serde_json::Map<String, Value>>(source)
                    .map_err(|e| e.to_string())?;
                lab.source = Some(source.into());
                lab.response = None;
            }
            "serve" => {
                lab.source.as_ref().ok_or("Save response.json first.")?;
                lab.url = Some("http://127.0.0.1:49152/".into());
            }
            _ => {
                lab.url.as_ref().ok_or("Start the server first.")?;
                lab.response = Some(Reply {
                    status: 200,
                    body: lab.source.clone().unwrap_or_default(),
                });
            }
        }
        if !lab.active {
            return Ok(Snapshot::default());
        }
        Ok(Snapshot {
            active: true,
            volume: lab.volume.clone(),
            source: lab.source.clone(),
            url: lab.url.clone(),
            response: lab.response.clone(),
        })
    }

    pub(crate) fn saved_lesson(&self) -> Option<Value> {
        self.lesson.clone()
    }

    pub(crate) fn save_lesson(&mut self, lesson: Option<&Value>) {
        self.lesson = lesson.cloned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(v: &Value) -> &str {
        v["stage"].as_str().unwrap_or_default()
    }

    fn ids(records: &[LessonRecord]) -> Vec<&str> {
        records.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn a_container_session_moves_on_in_the_terminal_and_in_the_window() {
        let mut t = Training::default();
        let pack = sample_module("container-basics").unwrap();
        t.modules("install", Some(&pack), None).unwrap();
        assert!(
            t.practice("status", None, None, true, false)
                .unwrap()
                .is_null()
        );
        t.practice("start", None, Some("container-basics"), false, false)
            .unwrap();
        let s = t.practice("status", None, None, true, false).unwrap();
        assert_eq!(stage(&s), "run");
        assert!(
            t.practice("status", None, None, false, true)
                .unwrap()
                .is_null()
        );
        let s = t.practice("advance", None, None, true, false).unwrap();
        assert_eq!(stage(&s), "stop");
        assert!(t.practice("advance", None, None, true, false).is_err());
        let (session, id) = (
            s["session"].as_str().unwrap().to_string(),
            s["containers"][0]["Id"].as_str().unwrap().to_string(),
        );
        let s = t
            .practice("stop", Some(&session), Some(&id), true, false)
            .unwrap();
        assert_eq!(stage(&s), "check");
        assert_eq!(s["containers"][0]["State"], "exited");
        t.practice("advance", None, None, true, false).unwrap();
        assert!(
            t.practice("advance", None, None, true, false)
                .unwrap()
                .is_null()
        );
        let done = t
            .modules("summary", None, Some("container-basics"))
            .unwrap();
        assert!(done[0].record.completed);
    }

    #[test]
    fn a_course_session_names_what_the_window_does() {
        let mut t = Training::default();
        // The module is missing: the start syncs the catalog first.
        t.practice("start", None, Some("web-basics"), false, false)
            .unwrap();
        let s = t.practice("advance", None, None, false, true).unwrap();
        assert_eq!(stage(&s), "ports");
        assert_eq!(s["module"], "web-basics");
        assert_eq!(s["web"]["ready"], false);
        assert_eq!(s["web"]["port"], 18080);
        let (session, id) = (
            s["session"].as_str().unwrap().to_string(),
            s["containers"][0]["Id"].as_str().unwrap().to_string(),
        );
        let s = t
            .practice("ports", Some(&session), Some(&id), false, true)
            .unwrap();
        assert_eq!(
            (stage(&s), s["web"]["ready"].clone()),
            ("browser", json!(true))
        );
        let s = t
            .practice("open-web", Some(&session), Some(&id), false, true)
            .unwrap();
        assert_eq!((stage(&s), s["stop"].clone()), ("stop", json!("lab-web")));
        assert!(t.practice("advance", None, None, false, true).is_err());
        let s = t
            .practice("stop", Some(&session), Some(&id), false, true)
            .unwrap();
        assert_eq!(stage(&s), "remove");
        assert_eq!(s["command"], "podbag rm lab-web");
        let s = t.practice("advance", None, None, false, true).unwrap();
        assert_eq!((stage(&s), s["containers"].clone()), ("finish", json!([])));
        t.practice("advance", None, None, false, true).unwrap();
        let done = t.modules("summary", None, Some("web-basics")).unwrap();
        assert!(done[0].record.completed);
    }

    #[test]
    fn a_failed_container_is_inspected_in_the_window() {
        let mut t = Training::default();
        t.practice("start", None, Some("web-service-recovery"), false, false)
            .unwrap();
        let s = t.practice("advance", None, None, false, true).unwrap();
        assert_eq!(stage(&s), "inspectFailed");
        assert_eq!(s["inspect_container"], "lab-web");
        assert_eq!(s["containers"][0]["State"], "exited");
        assert_eq!(s["containers"][0]["Status"], "Exited (1) 1 second ago");
        let session = s["session"].as_str().unwrap().to_string();
        let id = s["containers"][0]["Id"].as_str().unwrap().to_string();
        assert!(
            t.practice("inspect-failed", Some(&session), Some("other"), false, true)
                .is_err()
        );
        let s = t
            .practice("inspect-failed", Some(&session), Some(&id), false, true)
            .unwrap();
        assert_eq!(stage(&s), "readRecoveryLogs");
        assert!(s["inspect_container"].is_null());
    }

    #[test]
    fn a_course_volume_is_inspected_and_deleted_in_the_window() {
        let mut t = Training::default();
        t.practice("start", None, Some("volume-persistence"), false, false)
            .unwrap();
        for _ in 0..5 {
            t.practice("advance", None, None, false, true).unwrap();
        }
        let s = t.practice("status", None, None, false, true).unwrap();
        assert_eq!(stage(&s), "inspect");
        let session = s["session"].as_str().unwrap().to_string();
        assert!(
            t.practice("remove", Some(&session), Some("training-data"), false, true)
                .is_err()
        );
        let s = t
            .practice(
                "inspect",
                Some(&session),
                Some("training-data"),
                false,
                true,
            )
            .unwrap();
        assert_eq!(s["detail"]["Name"], "training-data");
        assert_eq!(
            (stage(&s), s["remove_volume"].clone()),
            ("delete", json!("training-data"))
        );
        let s = t
            .practice("remove", Some(&session), Some("training-data"), false, true)
            .unwrap();
        assert_eq!(
            (stage(&s), s["volumes"].clone()),
            ("checkVolumes", json!([]))
        );
    }

    #[test]
    fn a_sync_installs_the_catalog_with_the_names_of_its_steps() {
        let mut t = Training::default();
        let tui = sample_module("tui-volumes").unwrap();
        t.modules("install", Some(&tui), None).unwrap();
        let records = t.modules("sync", None, None).unwrap();
        assert_eq!(
            ids(&records),
            [
                "container-basics",
                "web-basics",
                "volume-persistence",
                "web-service-recovery",
                "volume-basics",
                "tui-volumes",
            ]
        );
        let web = &records[1];
        assert_eq!(web.steps[0].key, "runWeb");
        assert_eq!(web.steps[0].names["ko"], "웹 서버 실행하기");
        assert!(
            web.steps[0]
                .command
                .as_deref()
                .unwrap()
                .contains("127.0.0.1:18080:8080")
        );
        assert!(records[0].steps.is_empty(), "a built-in lab has no course");
    }

    #[test]
    fn offline_the_installed_modules_stay() {
        let mut t = Training::new(true);
        let pack = sample_module("container-basics").unwrap();
        t.modules("install", Some(&pack), None).unwrap();
        let error = t.modules("sync", None, None).err().unwrap();
        assert!(error.contains(catalog::URL), "{error}");
        let records = t.modules("status", None, None).unwrap();
        assert_eq!(ids(&records), ["container-basics"]);
        assert!(
            t.practice("start", None, Some("web-basics"), false, false)
                .is_err()
        );
    }

    #[test]
    fn an_old_course_file_has_no_steps_until_a_sync() {
        let mut t = Training::default();
        let old = json!({
            "format": "podbag-web-basics-v1",
            "translations": { "en": { "title": "Podbag Training · Web", "guide": "Old guide." } },
        })
        .to_string();
        let records = t.modules("install", Some(&old), None).unwrap();
        assert_eq!(ids(&records), ["web-basics"]);
        assert!(records[0].steps.is_empty());
        t.modules("sync", None, None).unwrap();
        // The old file again does not replace the course.
        let records = t.modules("install", Some(&old), None).unwrap();
        let web = records.iter().find(|r| r.id == "web-basics").unwrap();
        assert_eq!(web.steps.len(), 6);
        assert!(t.modules("install", Some("{}"), None).is_err());
    }

    #[test]
    fn the_tutorial_lab_serves_the_saved_file() {
        let mut t = Training::default();
        assert!(!t.tutorial_lab("status", None).unwrap().active);
        assert!(t.tutorial_lab("create", Some("my-api")).is_err());
        t.tutorial_lab("start", None).unwrap();
        assert!(t.tutorial_lab("create", Some("../x")).is_err());
        t.tutorial_lab("create", Some("my-api")).unwrap();
        assert!(t.tutorial_lab("save", Some("[")).is_err());
        t.tutorial_lab("save", Some(r#"{"a":1}"#)).unwrap();
        assert!(t.tutorial_lab("request", None).is_err());
        assert!(t.tutorial_lab("serve", None).unwrap().url.is_some());
        let r = t.tutorial_lab("request", None).unwrap().response.unwrap();
        assert_eq!((r.status, r.body.as_str()), (200, r#"{"a":1}"#));
        assert!(t.tutorial_lab("end", None).unwrap().volume.is_none());
    }
}
