//! One disposable Podbag volume and a loopback HTTP service for the desktop lab.
use crate::{Volume, paths::Dirs};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tempfile::{NamedTempFile, TempDir};
use tiny_http::{Header, Method, Response, Server};

type LabResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Status,
    Start,
    Create,
    Save,
    Serve,
    Request,
    End,
}

#[derive(Default, Serialize)]
pub struct Snapshot {
    pub active: bool,
    pub volume: Option<String>,
    pub source: Option<String>,
    pub url: Option<String>,
    pub response: Option<Reply>,
}

#[derive(Clone, Serialize)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

struct Service {
    url: String,
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
}

impl Service {
    fn start(file: PathBuf) -> LabResult<Self> {
        let server = Arc::new(Server::http(("127.0.0.1", 0))?);
        let url = format!("http://{}/", server.server_addr());
        let listener = Arc::clone(&server);
        let thread = thread::Builder::new()
            .name("podbag-tutorial-http".into())
            .spawn(move || {
                for request in listener.incoming_requests() {
                    let (status, body) = match (request.method(), request.url()) {
                        (&Method::Get, "/") => match fs::read(&file) {
                            Ok(bytes) => (200, bytes),
                            Err(_) => (500, vec![]),
                        },
                        (_, "/") => (405, vec![]),
                        _ => (404, vec![]),
                    };
                    let reply = Response::from_data(body)
                        .with_status_code(status)
                        .with_header(
                            Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                                .unwrap(),
                        )
                        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap())
                        .with_header(
                            Header::from_bytes("X-Content-Type-Options", "nosniff").unwrap(),
                        );
                    let _ = request.respond(reply);
                }
            })?;
        Ok(Self {
            url,
            server,
            thread: Some(thread),
        })
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Session {
    // Stop serving before TempDir removes the files.
    service: Option<Service>,
    root: TempDir,
    volume: Option<Volume>,
    reply: Option<Reply>,
}
impl Session {
    fn file(&self) -> LabResult<PathBuf> {
        let volume = self.volume.as_ref().ok_or("Create a volume first.")?;
        Ok(Path::new(&volume.mountpoint).join("response.json"))
    }

    fn source(&self) -> LabResult<Option<String>> {
        if self.volume.is_none() {
            return Ok(None);
        }
        match fs::read_to_string(self.file()?) {
            Ok(source) => Ok(Some(source)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Default)]
pub struct Lab {
    session: Option<Session>,
}

impl Lab {
    pub fn run(&mut self, action: Action, value: Option<&str>) -> LabResult<Snapshot> {
        match action {
            Action::Status => {}
            Action::Start => {
                if self.session.is_none() {
                    self.session = Some(Session {
                        service: None,
                        root: tempfile::Builder::new().prefix("podbag-lab-").tempdir()?,
                        volume: None,
                        reply: None,
                    });
                }
            }
            Action::End => {
                self.session.take();
            }
            other => {
                let lab = self.session.as_mut().ok_or("Start the lab first.")?;
                match other {
                    Action::Create => {
                        let name = value
                            .filter(|v| !v.trim().is_empty())
                            .ok_or("Enter a volume name.")?;
                        if let Some(v) = &lab.volume {
                            if v.name != name {
                                return Err("This lab already has a volume.".into());
                            }
                        } else {
                            let root = lab.root.path();
                            let dirs = Dirs {
                                data: root.join("data"),
                                runtime: root.join("run"),
                                config: root.join("config"),
                            };
                            lab.volume = Some(Volume::create_in_store(
                                &dirs,
                                Some(name),
                                Default::default(),
                            )?);
                        }
                    }
                    Action::Save => {
                        let source = value.ok_or("Enter a JSON object.")?;
                        if source.len() > 32 * 1024 {
                            return Err("Use 32 KiB or less.".into());
                        }
                        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(source)?;
                        let file = lab.file()?;
                        let mut tmp = NamedTempFile::new_in(file.parent().unwrap())?;
                        tmp.write_all(source.as_bytes())?;
                        tmp.persist(file)?;
                        lab.reply = None;
                    }
                    Action::Serve => {
                        let file = lab.file()?;
                        if !file.is_file() {
                            return Err("Save response.json first.".into());
                        }
                        if lab.service.is_none() {
                            lab.service = Some(Service::start(file)?);
                        }
                    }
                    Action::Request => {
                        let service = lab.service.as_ref().ok_or("Start the server first.")?;
                        let agent = ureq::Agent::config_builder()
                            .proxy(None)
                            .timeout_global(Some(Duration::from_secs(3)))
                            .build()
                            .new_agent();
                        let mut res = agent.get(&service.url).call()?;
                        let reply = Reply {
                            status: res.status().as_u16(),
                            body: res.body_mut().read_to_string()?,
                        };
                        if reply.status != 200 || Some(&reply.body) != lab.source()?.as_ref() {
                            return Err("The response does not match the saved file.".into());
                        }
                        lab.reply = Some(reply);
                    }
                    _ => unreachable!(),
                }
            }
        }
        let Some(lab) = &self.session else {
            return Ok(Snapshot::default());
        };
        Ok(Snapshot {
            active: true,
            volume: lab.volume.as_ref().map(|v| v.name.clone()),
            source: lab.source()?,
            url: lab.service.as_ref().map(|s| s.url.clone()),
            response: lab.reply.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learners_serve_their_own_file_from_separate_disposable_volumes() {
        let mut a = Lab::default();
        let mut b = Lab::default();
        let source = r#"{"message":"I wrote this"}"#;
        for lab in [&mut a, &mut b] {
            lab.run(Action::Start, None).unwrap();
            assert!(lab.run(Action::Create, Some("../escape")).is_err());
            lab.run(Action::Create, Some("my-api")).unwrap();
            assert!(lab.run(Action::Serve, None).is_err());
        }
        let file = a.session.as_ref().unwrap().file().unwrap();
        assert_ne!(file, b.session.as_ref().unwrap().file().unwrap());
        a.run(Action::Save, Some(source)).unwrap();
        assert!(a.run(Action::Save, Some("[")).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), source);
        let url = a.run(Action::Serve, None).unwrap().url.unwrap();
        let addr = url.trim_start_matches("http://").trim_end_matches('/');
        let first = a.run(Action::Request, None).unwrap().response.unwrap();
        assert_eq!((first.status, first.body.as_str()), (200, source));
        a.run(Action::Save, Some("{}")).unwrap();
        assert_eq!(
            a.run(Action::Request, None).unwrap().response.unwrap().body,
            "{}"
        );
        assert!(b.run(Action::Status, None).unwrap().source.is_none());
        a.run(Action::End, None).unwrap();
        assert!(!file.exists());
        // tiny_http closes its listener on its own accept thread, a moment after `End`.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(addr).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the lab still serves {addr}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(a.run(Action::Start, None).unwrap().volume.is_none());
    }
}
