//! Training on sample data: the installed modules, a practice session, the tutorial lab
//! and the JSON lesson, all in memory.
//!
//! A real practice session starts in a terminal (`podbag lab start <id>`) and moves on
//! there; the window only watches it. Here two extra actions stand in for the terminal:
//! `practice("start", None, Some(id), ..)` starts a session of the module `id`, and
//! `practice("advance", ..)` does the current step's terminal part.

use crate::training::LessonRecord;
use podbag::client::{ContainerSummary, Port, VolumeSummary};
use podbag::dashboard::lab::{Reply, Snapshot};
use podbag::volume_lesson::{Installed, Module, course};
use serde_json::{Value, json};

/// The module file of `id`, as `podbag lab` installs it (English and Korean words).
pub fn sample_module(id: &str) -> Option<String> {
    let module = Module::parse(id).ok()?;
    if module == Module::Containers {
        return Some(include_str!("../../labs/container-basics/container-basics.podbag").into());
    }
    let (en, ko) = match module {
        Module::Web => ("Publish a web page", "웹 페이지 게시하기"),
        Module::Persistence => ("Keep data in a volume", "볼륨에 데이터 보관하기"),
        Module::Service => ("Replace a web service", "웹 서비스 교체하기"),
        Module::Volumes => ("Volumes in the CLI and the GUI", "CLI와 GUI의 볼륨"),
        _ => ("Volumes in the TUI", "TUI의 볼륨"),
    };
    let words = |title: &str, guide: &str| json!({ "title": format!("Podbag Training · {title}"), "guide": guide });
    Some(
        json!({
            "format": module.format(),
            "translations": {
                "en": words(en, "Follow the steps in your terminal and in Podbag Desktop."),
                "ko": words(ko, "터미널과 Podbag Desktop에서 단계를 따라 하세요."),
            },
        })
        .to_string(),
    )
}

/// Where a step is done: in the terminal, or in the window with this action.
fn desktop_action(key: &str) -> Option<&'static str> {
    Some(match key {
        "ports" | "portsReplacement" => "ports",
        "browser" | "browserReplacement" => "open-web",
        "stop" | "stopReplacement" => "stop",
        "inspect" => "inspect",
        "delete" => "remove",
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// `volume-basics`: the GUI volumes lesson.
    Volumes,
    /// `container-basics`.
    Containers,
    /// A course module (`web-basics`, …).
    Course,
}

struct Session {
    module: Module,
    flavor: Flavor,
    id: String,
    /// The steps after "start": (key, command).
    steps: Vec<(String, Option<String>)>,
    at: usize,
    containers: Vec<ContainerSummary>,
    volumes: Vec<VolumeSummary>,
}

const PORT: u16 = 18080;

fn container(name: &str, running: bool, port: Option<u16>) -> ContainerSummary {
    ContainerSummary {
        id: format!(
            "{:0>16x}",
            name.bytes().fold(7u64, |h, b| h * 31 + b as u64)
        ),
        names: vec![format!("/{name}")],
        image: podbag::volume_lesson::containers::IMAGE.into(),
        command: "httpd -f -p 8080".into(),
        created: 0,
        state: if running { "running" } else { "exited" }.into(),
        status: if running {
            "Up 5 seconds"
        } else {
            "Exited (0) 1 second ago"
        }
        .into(),
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
    fn key(&self) -> &str {
        self.steps.get(self.at).map_or("finish", |s| s.0.as_str())
    }

    /// The container the current step is about: the last web server.
    fn web_container(&self) -> Option<&ContainerSummary> {
        self.containers
            .iter()
            .rev()
            .find(|c| !matches!(c.name(), "app-a" | "app-b"))
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
            Flavor::Course => {
                let web = self.web_container().map(|c| c.name().to_string());
                let web = match key {
                    "ports" | "portsReplacement" | "browser" | "browserReplacement" => {
                        web.map(|name| {
                            json!({
                                "container": name, "port": PORT,
                                "ready": key.starts_with("browser"),
                                "url": format!("http://127.0.0.1:{PORT}/"),
                            })
                        })
                    }
                    _ => None,
                };
                let stop = key
                    .starts_with("stop")
                    .then(|| self.web_container().map(|c| c.name().to_string()))
                    .flatten();
                let remove = (key == "delete")
                    .then(|| self.volumes.first().map(|v| v.name.clone()))
                    .flatten();
                json!({
                    "module": self.module.id(), "session": self.id, "stage": key,
                    "command": self.steps.get(self.at).and_then(|s| s.1.clone()),
                    "containers": self.containers, "volumes": self.volumes, "detail": detail,
                    "web": web, "stop": stop, "remove_volume": remove,
                })
            }
        }
    }

    /// What the step `key` changes when it is done.
    fn effect(&mut self, key: &str) {
        let web_name = match self.module {
            Module::Service if self.containers.iter().any(|c| c.name() == "site-v1") => "site-v2",
            Module::Service => "site-v1",
            _ => "lab-web",
        };
        match key {
            "run" => self.containers.push(container("hello-web", true, None)),
            "runWeb" | "runFirst" | "runReplacement" => {
                self.containers.push(container(web_name, true, Some(PORT)));
            }
            "write" => self.containers.push(container("app-a", false, None)),
            "read" => self.containers.push(container("app-b", false, None)),
            "create" if self.flavor == Flavor::Volumes => self.volumes.push(volume("my-api")),
            "create" => {
                let name = if self.module == Module::Service {
                    "site-data"
                } else {
                    "training-data"
                };
                self.volumes.push(volume(name));
            }
            "stop" | "stopReplacement" => {
                if let Some(c) = self
                    .containers
                    .iter_mut()
                    .rev()
                    .find(|c| c.state == "running")
                {
                    c.state = "exited".into();
                    c.status = "Exited (137) 1 second ago".into();
                }
            }
            "removeWriter" => self.containers.retain(|c| c.name() != "app-a"),
            "removeReader" => self.containers.retain(|c| c.name() != "app-b"),
            "remove" | "removeReplacement" if self.flavor == Flavor::Course => {
                if let Some(i) = self.containers.iter().rposition(|c| c.state == "exited") {
                    self.containers.remove(i);
                }
            }
            "remove" | "delete" => self.volumes.clear(),
            _ => {}
        }
    }
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
    modules: Vec<(Module, Value, bool)>,
    session: Option<Session>,
    sessions: u32,
    lab: TutorialLab,
    lesson: Option<Value>,
}

fn record(module: Module, package: &Value, completed: bool) -> Result<LessonRecord, String> {
    let installed: Installed = serde_json::from_value(json!({
        "package": package, "completed": completed, "generation": "mock",
    }))
    .map_err(|e| e.to_string())?;
    Ok(LessonRecord {
        id: module.id(),
        steps: course::instructions(module),
        record: installed,
    })
}

fn steps_of(module: Module) -> Vec<(String, Option<String>)> {
    let keys = |k: &[&str]| k.iter().map(|k| (k.to_string(), None)).collect();
    match module {
        Module::Containers => keys(&["run", "stop", "check", "finish"]),
        Module::Volumes => keys(&["create", "inspect", "remove", "verify", "finish"]),
        _ => serde_json::to_value(course::instructions(module))
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|s| {
                (
                    s["key"].as_str().unwrap_or_default().to_string(),
                    s["command"].as_str().map(str::to_string),
                )
            })
            .collect(),
    }
}

impl Training {
    pub(crate) fn modules(
        &mut self,
        action: &str,
        package: Option<&str>,
        id: Option<&str>,
    ) -> Result<Vec<LessonRecord>, String> {
        match action {
            "status" | "summary" => {}
            "install" => {
                let text = package.ok_or("Choose a module file.")?;
                if text.len() > podbag::volume_lesson::LIMIT {
                    return Err("The module file is too large.".into());
                }
                let mut pack: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
                let module = Module::for_format(pack["format"].as_str().unwrap_or_default())
                    .map_err(|e| e.to_string())?;
                // Words without a guide are kept with an empty one.
                if let Some(words) = pack["translations"].as_object_mut() {
                    for w in words.values_mut() {
                        if w.get("guide").is_none() {
                            w["guide"] = json!("");
                        }
                    }
                }
                self.modules.retain(|(m, _, _)| *m != module);
                self.modules.push((module, pack, false));
            }
            "remove" => {
                let module = Module::parse(id.unwrap_or_default()).map_err(|e| e.to_string())?;
                self.modules.retain(|(m, _, _)| *m != module);
            }
            _ => return Err("Unknown lesson action.".into()),
        }
        self.modules
            .iter()
            .filter(|(m, _, _)| action != "summary" || id.is_none_or(|id| id == m.id()))
            .map(|(m, pack, done)| record(*m, pack, *done))
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
            let module = Module::parse(name.unwrap_or_default()).map_err(|e| e.to_string())?;
            let flavor = match module {
                Module::Containers => Flavor::Containers,
                Module::Volumes => Flavor::Volumes,
                Module::Tui => return Err("This module has no practice session.".into()),
                _ => Flavor::Course,
            };
            self.sessions += 1;
            self.session = Some(Session {
                module,
                flavor,
                id: format!("podbag-practice-{}", self.sessions),
                steps: steps_of(module),
                at: 0,
                containers: Vec::new(),
                volumes: Vec::new(),
            });
            return Ok(Value::Null);
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
            let key = s.key().to_string();
            if desktop_action(&key).is_some() && s.flavor == Flavor::Course
                || matches!((s.flavor, key.as_str()), (Flavor::Containers, "stop"))
                || matches!(
                    (s.flavor, key.as_str()),
                    (Flavor::Volumes, "inspect" | "remove")
                )
            {
                return Err("This step is done in Podbag Desktop.".into());
            }
            if key == "finish" {
                // The session ends and the module is complete.
                let module = s.module;
                self.session = None;
                if let Some(m) = self.modules.iter_mut().find(|(m, _, _)| *m == module) {
                    m.2 = true;
                }
                return Ok(Value::Null);
            }
            s.effect(&key);
            s.at += 1;
            return Ok(s.snapshot(None));
        }
        if session != Some(s.id.as_str()) {
            return Err("The practice session has changed.".into());
        }
        let target = name.unwrap_or_default();
        let key = s.key().to_string();
        let mut detail = None;
        match (s.flavor, action) {
            (_, "inspect") => {
                let v = s
                    .volumes
                    .iter()
                    .find(|v| v.name == target)
                    .ok_or("The practice volume was not found.")?;
                detail = Some(json!({
                    "Name": v.name, "Driver": v.driver, "Mountpoint": v.mountpoint,
                    "CreatedAt": v.created_at, "Scope": "local",
                }));
                if key == "inspect" {
                    s.at += 1;
                }
            }
            (Flavor::Volumes, "remove") if key == "remove" && target == "my-api" => {
                s.effect("remove");
                s.at += 1;
            }
            (Flavor::Course, "remove") if key == "delete" => {
                s.effect("delete");
                s.at += 1;
            }
            (Flavor::Containers, "stop") if key == "stop" => {
                let c = s
                    .containers
                    .iter()
                    .find(|c| c.id == target)
                    .ok_or("The practice container was not found.")?;
                if c.state != "running" {
                    return Err("The practice container cannot perform that action.".into());
                }
                s.effect("stop");
                s.at += 1;
            }
            (Flavor::Course, "ports" | "open-web" | "show-web" | "stop")
                if desktop_action(&key)
                    == Some(if action == "show-web" {
                        "open-web"
                    } else {
                        action
                    }) =>
            {
                if s.web_container().is_none_or(|c| c.id != target) {
                    return Err("The practice container was not found.".into());
                }
                if action != "show-web" {
                    s.effect(&key);
                    s.at += 1;
                }
            }
            _ => return Err("This action is unavailable for the current practice step.".into()),
        }
        Ok(s.snapshot(detail))
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
        t.practice("start", None, Some("web-basics"), false, false)
            .unwrap();
        let s = t.practice("advance", None, None, false, true).unwrap();
        assert_eq!(stage(&s), "ports");
        assert_eq!(s["module"], "web-basics");
        assert_eq!(s["web"]["ready"], false);
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
