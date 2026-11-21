//! Steps and verification for modules that use the practice container engine.
use super::{Module, containers, gui};
use crate::client::{ContainerSummary, Kind, VolumeSummary};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::Path,
    time::Duration,
};

pub const ACTIVE: &str = "active-course-session";
const STATE: &str = "run/course.json";
const IMAGE: &str = containers::IMAGE;
const NOTE: &str = "hello from app-a";
const RECOVERY_NAME: &str = "lab-web";
const MISSING_TITLE: &str = "SITE_TITLE is required";
const RECOVERED_TITLE: &str = "Recovered Podbag site";

fn recovery_command(options: &str) -> String {
    let startup = r#"if [ -z "$SITE_TITLE" ]; then echo "SITE_TITLE is required" >&2; exit 1; fi; mkdir -p /www && printf "%s\n" "$SITE_TITLE" > /www/index.html && exec httpd -f -p 8080 -h /www"#;
    format!("podbag run -d --name {RECOVERY_NAME} {options} {IMAGE} sh -c '{startup}'")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    RunWeb,
    RunFailed,
    InspectFailed,
    ReadRecoveryLogs,
    RemoveFailed,
    RunFixed,
    Ports(&'static str),
    Browser(&'static str),
    Stop(&'static str),
    Remove(&'static str),
    Create(&'static str),
    Write,
    Read,
    Inspect(&'static str),
    Delete(&'static str),
    RunFirst,
    RunReplacement,
    CheckContainers,
    CheckVolumes,
    Finish,
}

const WEB: &[Step] = &[
    Step::RunWeb,
    Step::Ports("lab-web"),
    Step::Browser("lab-web"),
    Step::Stop("lab-web"),
    Step::Remove("lab-web"),
    Step::Finish,
];
const PERSISTENCE: &[Step] = &[
    Step::Create("training-data"),
    Step::Write,
    Step::Remove("app-a"),
    Step::Read,
    Step::Remove("app-b"),
    Step::Inspect("training-data"),
    Step::Delete("training-data"),
    Step::CheckVolumes,
    Step::Finish,
];
const SERVICE: &[Step] = &[
    Step::Create("site-data"),
    Step::RunFirst,
    Step::Ports("site-v1"),
    Step::Browser("site-v1"),
    Step::Stop("site-v1"),
    Step::Remove("site-v1"),
    Step::RunReplacement,
    Step::Ports("site-v2"),
    Step::Browser("site-v2"),
    Step::Stop("site-v2"),
    Step::Remove("site-v2"),
    Step::Inspect("site-data"),
    Step::Delete("site-data"),
    Step::CheckContainers,
    Step::CheckVolumes,
    Step::Finish,
];

const RECOVERY: &[Step] = &[
    Step::RunFailed,
    Step::InspectFailed,
    Step::ReadRecoveryLogs,
    Step::RemoveFailed,
    Step::RunFixed,
    Step::Ports(RECOVERY_NAME),
    Step::Browser(RECOVERY_NAME),
    Step::Stop(RECOVERY_NAME),
    Step::Remove(RECOVERY_NAME),
    Step::Finish,
];

pub fn steps(module: Module) -> &'static [Step] {
    match module {
        Module::Web => WEB,
        Module::Persistence => PERSISTENCE,
        Module::Service => SERVICE,
        Module::Recovery => RECOVERY,
        _ => &[],
    }
}

fn default_port(module: Module) -> u16 {
    match module {
        Module::Web => 18080,
        Module::Service => 18081,
        Module::Recovery => 18082,
        _ => 0,
    }
}

impl Step {
    fn key(self) -> &'static str {
        match self {
            Self::RunWeb => "runWeb",
            Self::RunFailed => "runFailed",
            Self::InspectFailed => "inspectFailed",
            Self::ReadRecoveryLogs => "readRecoveryLogs",
            Self::RemoveFailed => "removeFailed",
            Self::RunFixed => "runFixed",
            Self::Ports("site-v2") => "portsReplacement",
            Self::Ports(_) => "ports",
            Self::Browser("site-v2") => "browserReplacement",
            Self::Browser(_) => "browser",
            Self::Stop("site-v2") => "stopReplacement",
            Self::Stop(_) => "stop",
            Self::Remove("app-a") => "removeWriter",
            Self::Remove("app-b") => "removeReader",
            Self::Remove("site-v2") => "removeReplacement",
            Self::Remove(_) => "remove",
            Self::Create(_) => "create",
            Self::Write => "write",
            Self::Read => "read",
            Self::Inspect(_) => "inspect",
            Self::Delete(_) => "delete",
            Self::RunFirst => "runFirst",
            Self::RunReplacement => "runReplacement",
            Self::CheckContainers => "checkContainers",
            Self::CheckVolumes => "checkVolumes",
            Self::Finish => "finish",
        }
    }

    pub fn command(self, port: u16) -> Option<String> {
        let publish = format!("-p 127.0.0.1:{port}:8080");
        Some(match self {
            Self::RunWeb => format!(
                "podbag run -d --name lab-web {publish} {IMAGE} sh -c 'mkdir -p /www && printf \"%s\\n\" \"Podbag Training\" > /www/index.html && exec httpd -f -p 8080 -h /www'"
            ),
            Self::RunFailed => recovery_command("--network none"),
            Self::ReadRecoveryLogs => format!("podbag logs {RECOVERY_NAME}"),
            Self::RemoveFailed => format!("podbag rm {RECOVERY_NAME}"),
            Self::RunFixed => {
                recovery_command(&format!("{publish} -e 'SITE_TITLE={RECOVERED_TITLE}'"))
            }
            Self::RunFirst => format!(
                "podbag run -d --name site-v1 {publish} -v site-data:/site {IMAGE} sh -c 'printf \"%s\\n\" \"My Podbag site\" > /site/index.html && exec httpd -f -p 8080 -h /site'"
            ),
            Self::RunReplacement => format!(
                "podbag run -d --name site-v2 {publish} -v site-data:/site:ro {IMAGE} httpd -f -p 8080 -h /site"
            ),
            Self::Create(volume) => format!("podbag volume create {volume}"),
            Self::Write => format!(
                "podbag run --name app-a --network none -v training-data:/data {IMAGE} sh -c 'printf \"%s\\n\" \"{NOTE}\" > /data/note.txt'"
            ),
            Self::Read => format!(
                "podbag run --name app-b --network none -v training-data:/data:ro {IMAGE} cat /data/note.txt"
            ),
            Self::Remove(name) => format!("podbag rm {name}"),
            Self::CheckContainers => "podbag ps -a".into(),
            Self::CheckVolumes => "podbag volume ls".into(),
            Self::Finish => "exit".into(),
            _ => return None,
        })
    }

    pub fn verify_cli(self, root: &Path, output: &[u8]) -> io::Result<()> {
        let client = containers::client(root);
        let verified = match self {
            Self::RunFailed => {
                // Detach returns after launch. The normal wait endpoint supplies the actual exit code.
                let reply = client
                    .call("POST", &format!("/containers/{RECOVERY_NAME}/wait"), None)
                    .map_err(io::Error::other)?;
                reply["StatusCode"].as_i64() == Some(1)
            }
            Self::ReadRecoveryLogs => std::str::from_utf8(output)
                .is_ok_and(|text| text.lines().any(|line| line.trim() == MISSING_TITLE)),
            Self::RunWeb | Self::RunFirst | Self::RunReplacement | Self::RunFixed => {
                let name = match self {
                    Self::RunWeb | Self::RunFixed => "lab-web",
                    Self::RunFirst => "site-v1",
                    _ => "site-v2",
                };
                let rows = client.containers().map_err(io::Error::other)?;
                let port = Attempt::read(root)?.port;
                rows.iter()
                    .any(|row| row.name() == name && published(row, port))
            }
            Self::Create(name) => client
                .volumes()
                .map_err(io::Error::other)?
                .iter()
                .any(|vol| vol.name == name),
            Self::Write => client
                .containers()
                .map_err(io::Error::other)?
                .iter()
                .any(|row| {
                    row.name() == "app-a"
                        && row.state == "exited"
                        && row.status.starts_with("Exited (0)")
                }),
            Self::Read => std::str::from_utf8(output)
                .is_ok_and(|text| text.trim_end_matches(['\r', '\n']) == NOTE),
            Self::RemoveFailed => client
                .containers()
                .map_err(io::Error::other)?
                .iter()
                .all(|row| row.name() != RECOVERY_NAME),
            Self::Remove(name) => client
                .containers()
                .map_err(io::Error::other)?
                .iter()
                .all(|row| row.name() != name),
            Self::CheckContainers => client.containers().map_err(io::Error::other)?.is_empty(),
            Self::CheckVolumes => client.volumes().map_err(io::Error::other)?.is_empty(),
            _ => false,
        };
        if verified {
            Ok(())
        } else {
            Err(io::Error::other(
                "The practice result did not match this step.",
            ))
        }
    }
}

#[derive(Serialize)]
pub struct Instruction {
    key: &'static str,
    command: Option<String>,
}

pub fn instructions(module: Module) -> Vec<Instruction> {
    steps(module)
        .iter()
        .map(|step| Instruction {
            key: step.key(),
            command: step.command(default_port(module)),
        })
        .collect()
}

#[derive(Deserialize, Serialize)]
pub struct Attempt {
    module: String,
    index: usize,
    pub port: u16,
}

impl Attempt {
    pub fn start(root: &Path, module: Module) -> io::Result<()> {
        let preferred = default_port(module);
        let port = if preferred == 0 {
            0
        } else {
            available_port(preferred)?
        };
        Self {
            module: module.id().into(),
            index: 0,
            port,
        }
        .save(root)
    }

    pub fn active(root: &Path) -> bool {
        root.join(STATE).is_file()
    }

    fn save(&self, root: &Path) -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(root.join("run"))?;
        serde_json::to_writer(file.as_file_mut(), self)?;
        file.persist(root.join(STATE))?;
        Ok(())
    }

    pub fn read(root: &Path) -> io::Result<Self> {
        Ok(serde_json::from_slice(&fs::read(root.join(STATE))?)?)
    }

    pub fn lock(root: &Path) -> io::Result<fs::File> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("run/course.lock"))?;
        file.lock()?;
        Ok(file)
    }

    pub fn step(&self) -> io::Result<Step> {
        steps(Module::parse(&self.module)?)
            .get(self.index)
            .copied()
            .ok_or_else(|| io::Error::other("Invalid Training step."))
    }

    pub fn advance(&mut self, root: &Path) -> io::Result<()> {
        self.index += 1;
        self.save(root)
    }

    pub fn refresh_port(&mut self, root: &Path) -> io::Result<bool> {
        if TcpListener::bind((Ipv4Addr::LOCALHOST, self.port)).is_ok() {
            return Ok(false);
        }
        self.port = available_port(self.port)?;
        self.save(root)?;
        Ok(true)
    }
}

fn available_port(preferred: u16) -> io::Result<u16> {
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, preferred))
        .or_else(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))?;
    Ok(socket.local_addr()?.port())
}

fn published(row: &ContainerSummary, port: u16) -> bool {
    row.state == "running"
        && row.ports.iter().any(|p| {
            p.public_port == port
                && p.private_port == 8080
                && p.kind == "tcp"
                && p.ip == "127.0.0.1"
        })
}

fn check_page(port: u16, expected: &str) -> io::Result<()> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let timeout = Duration::from_secs(2);
    let mut connection = TcpStream::connect_timeout(&address, timeout)?;
    connection.set_read_timeout(Some(timeout))?;
    connection.set_write_timeout(Some(timeout))?;
    connection.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut reply = String::new();
    connection.take(8192).read_to_string(&mut reply)?;
    let matched = reply.split_once("\r\n\r\n").is_some_and(|(head, body)| {
        head.lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            == Some("200")
            && body.trim() == expected
    });
    if matched {
        Ok(())
    } else {
        Err(io::Error::other(
            "The published page did not return the expected content.",
        ))
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    module: String,
    session: String,
    stage: &'static str,
    command: Option<String>,
    containers: Vec<ContainerSummary>,
    volumes: Vec<VolumeSummary>,
    detail: Option<serde_json::Value>,
    web: Option<Web>,
    stop: Option<&'static str>,
    inspect_container: Option<&'static str>,
    remove_volume: Option<&'static str>,
}

#[derive(Serialize)]
struct Web {
    container: &'static str,
    port: u16,
    ready: bool,
    url: String,
}

pub fn view(
    library: &Path,
    action: &str,
    expected: Option<&str>,
    target: &str,
) -> io::Result<Option<Snapshot>> {
    let Some((session, root)) =
        gui::session_root(library, ACTIVE, containers::PREFIX, action, expected)?
    else {
        return Ok(None);
    };
    if !Attempt::active(&root) || root.join("run/closing").exists() {
        return if action == "status" {
            Ok(None)
        } else {
            Err(io::Error::other("The practice session has ended."))
        };
    }
    let _lock = if action == "status" {
        None
    } else {
        Some(Attempt::lock(&root)?)
    };
    let mut attempt = Attempt::read(&root)?;
    let client = containers::client(&root);
    let mut rows = client.containers().map_err(io::Error::other)?;
    let mut volumes = client.volumes().map_err(io::Error::other)?;
    let mut detail = None;
    let row = |name: &str| {
        rows.iter()
            .find(|row| row.name() == name)
            .ok_or_else(|| io::Error::other("The practice container was not found."))
    };
    let step = attempt.step()?;
    match (action, step) {
        ("status", _) => {}
        ("inspect-failed", Step::InspectFailed)
            if row(RECOVERY_NAME)?.id == target && row(RECOVERY_NAME)?.state == "exited" =>
        {
            attempt.advance(&root)?;
        }
        ("ports", Step::Ports(name))
            if published(row(name)?, attempt.port) && row(name)?.id == target =>
        {
            attempt.advance(&root)?;
        }
        ("open-web", Step::Browser(name)) | ("show-web", Step::Browser(name))
            if published(row(name)?, attempt.port) && row(name)?.id == target =>
        {
            check_page(
                attempt.port,
                match (attempt.module.as_str(), name) {
                    ("web-service-recovery", _) => RECOVERED_TITLE,
                    (_, "lab-web") => "Podbag Training",
                    _ => "My Podbag site",
                },
            )
            .map_err(|e| {
                io::Error::other(format!("Check the published page and try again: {e}"))
            })?;
            if action == "open-web" {
                attempt.advance(&root)?;
            }
        }
        ("stop", Step::Stop(name)) if row(name)?.id == target => {
            client
                .container_action(target, "stop")
                .map_err(io::Error::other)?;
            rows = client.containers().map_err(io::Error::other)?;
            if !rows.iter().any(|r| r.id == target && r.state == "exited") {
                return Err(io::Error::other(
                    "The practice container has not stopped yet.",
                ));
            }
            attempt.advance(&root)?;
        }
        ("inspect", _) if volumes.iter().any(|v| v.name == target) => {
            detail = Some(
                client
                    .inspect(Kind::Volumes, target)
                    .map_err(io::Error::other)?,
            );
            if let Step::Inspect(name) = step
                && name == target
            {
                attempt.advance(&root)?;
            }
        }
        ("remove", Step::Delete(name)) if name == target => {
            client
                .remove(Kind::Volumes, target, false)
                .map_err(io::Error::other)?;
            volumes = client.volumes().map_err(io::Error::other)?;
            if volumes.iter().any(|volume| volume.name == target) {
                return Err(io::Error::other("The practice volume still exists."));
            }
            attempt.advance(&root)?;
        }
        _ => {
            return Err(io::Error::other(
                "This action is unavailable for the current practice step.",
            ));
        }
    }
    let step = attempt.step()?;
    let web = match step {
        Step::Ports(container) | Step::Browser(container) => Some(Web {
            container,
            port: attempt.port,
            ready: matches!(step, Step::Browser(_)),
            url: format!("http://127.0.0.1:{}/", attempt.port),
        }),
        _ => None,
    };
    Ok(Some(Snapshot {
        module: attempt.module,
        session,
        stage: step.key(),
        command: step.command(attempt.port),
        containers: rows,
        volumes,
        detail,
        web,
        stop: if let Step::Stop(name) = step {
            Some(name)
        } else {
            None
        },
        inspect_container: (step == Step::InspectFailed).then_some(RECOVERY_NAME),
        remove_volume: if let Step::Delete(name) = step {
            Some(name)
        } else {
            None
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_steps_require_the_missing_title_message() {
        let dir = tempfile::tempdir().unwrap();
        let logs = Step::ReadRecoveryLogs;
        assert!(
            logs.verify_cli(dir.path(), b"SITE_TITLE is required\n")
                .is_ok()
        );
        assert!(logs.verify_cli(dir.path(), b"Listening on 8080\n").is_err());
        let steps = instructions(Module::Recovery);
        let keys: std::collections::HashSet<_> = steps.iter().map(|step| step.key).collect();
        assert_eq!(keys.len(), steps.len());
        assert!(keys.contains("removeFailed") && keys.contains("remove"));
    }

    #[test]
    fn a_used_port_updates_the_command_in_this_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("run")).unwrap();
        Attempt::start(root, Module::Web).unwrap();
        let mut attempt = Attempt::read(root).unwrap();
        let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        attempt.port = reserved.local_addr().unwrap().port();
        attempt.save(root).unwrap();
        assert!(attempt.refresh_port(root).unwrap());
        assert_ne!(attempt.port, reserved.local_addr().unwrap().port());
        let stored = Attempt::read(root).unwrap();
        assert_eq!(stored.port, attempt.port);
        assert!(
            stored
                .step()
                .unwrap()
                .command(stored.port)
                .unwrap()
                .contains(&format!("127.0.0.1:{}:8080", attempt.port))
        );
    }

    #[test]
    fn opening_the_page_checks_the_status_and_body() {
        for (status, body, expected) in [
            ("200 OK", "My Podbag site\n", true),
            ("404 Not Found", "My Podbag site\n", false),
            ("200 OK", "An unrelated service\n", false),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 128];
                stream.read(&mut request).unwrap();
                write!(stream, "HTTP/1.0 {status}\r\n\r\n{body}").unwrap();
            });
            assert_eq!(check_page(port, "My Podbag site").is_ok(), expected);
            server.join().unwrap();
        }
    }
}
