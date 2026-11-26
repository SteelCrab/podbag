//! Steps and verification for modules that use the practice container engine.
//!
//! A course module is data (`podbag-course-v1`): its steps, the command of each CLI step,
//! and a check from a fixed set of kinds. The learner runs the commands; podbag only reads
//! the practice engine's state to check them.
use super::{Package, containers, gui};
use crate::client::{ContainerSummary, Kind, VolumeSummary};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::Path,
    time::Duration,
};

pub const FORMAT: &str = "podbag-course-v1";
/// v1 with more check kinds, files for the practice directory, and `docker …` steps.
pub const FORMAT_V2: &str = "podbag-course-v2";
const MAX_FILES: usize = 8;
const FILES_LIMIT: usize = 16 * 1024;

/// Both course formats.
pub fn is_course(format: &str) -> bool {
    format == FORMAT || format == FORMAT_V2
}
pub const ACTIVE: &str = "active-course-session";
const MAX_STEPS: usize = 40;

/// How a step is checked. CLI kinds read the practice engine after the learner's command;
/// `desktop-*` kinds are done by an action in Podbag Desktop.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Check {
    NetworkExists {
        name: String,
    },
    NetworkAbsent {
        name: String,
    },
    VolumeExists {
        name: String,
    },
    VolumesEmpty,
    ContainersEmpty,
    ContainerRunning {
        name: String,
    },
    ContainerPublished {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<String>,
    },
    ContainerExited {
        name: String,
        code: i64,
    },
    ContainerAbsent {
        name: String,
    },
    StdoutEquals {
        text: String,
    },
    OutputHasLine {
        text: String,
    },
    Finish,
    // podbag-course-v2
    ImageExists {
        name: String,
    },
    ImageAbsent {
        name: String,
    },
    ContainerCreated {
        name: String,
    },
    ContainerStopped {
        name: String,
    },
    ContainerPaused {
        name: String,
    },
    ContainerHealthy {
        name: String,
    },
    ContainerRestarted {
        name: String,
        count: u32,
    },
    ContainerInNetwork {
        name: String,
        network: String,
    },
    ContainerNotInNetwork {
        name: String,
        network: String,
    },
    VolumeAbsent {
        name: String,
    },
    OutputContains {
        text: String,
    },
    FileExists {
        path: String,
    },
    DesktopInspectExited {
        name: String,
    },
    DesktopPorts {
        name: String,
    },
    DesktopBrowser {
        name: String,
        page: String,
    },
    DesktopStop {
        name: String,
    },
    DesktopInspectVolume {
        name: String,
    },
    DesktopDeleteVolume {
        name: String,
    },
}

impl Check {
    pub fn in_desktop(&self) -> bool {
        matches!(
            self,
            Self::DesktopInspectExited { .. }
                | Self::DesktopPorts { .. }
                | Self::DesktopBrowser { .. }
                | Self::DesktopStop { .. }
                | Self::DesktopInspectVolume { .. }
                | Self::DesktopDeleteVolume { .. }
        )
    }

    /// The check with `{port}` in its names and texts set to PORT.
    pub fn with_port(&self, port: u16) -> Check {
        let text = serde_json::to_string(self).expect("a check is JSON");
        serde_json::from_str(&text.replace("{port}", &port.to_string())).expect("still a check")
    }

    /// Kinds that need `FORMAT_V2`.
    fn is_v2(&self) -> bool {
        matches!(
            self,
            Self::ImageExists { .. }
                | Self::ImageAbsent { .. }
                | Self::ContainerCreated { .. }
                | Self::ContainerStopped { .. }
                | Self::ContainerPaused { .. }
                | Self::ContainerHealthy { .. }
                | Self::ContainerRestarted { .. }
                | Self::ContainerInNetwork { .. }
                | Self::ContainerNotInNetwork { .. }
                | Self::VolumeAbsent { .. }
                | Self::OutputContains { .. }
                | Self::FileExists { .. }
        )
    }

    /// The names and texts the check compares with.
    fn values(&self) -> Vec<&str> {
        match self {
            Self::ContainerInNetwork { name, network }
            | Self::ContainerNotInNetwork { name, network } => vec![name, network],
            Self::OutputContains { text } => vec![text],
            Self::FileExists { path } => vec![path],
            Self::ImageExists { name }
            | Self::ImageAbsent { name }
            | Self::ContainerCreated { name }
            | Self::ContainerStopped { name }
            | Self::ContainerPaused { name }
            | Self::ContainerHealthy { name }
            | Self::ContainerRestarted { name, .. }
            | Self::VolumeAbsent { name } => vec![name],
            Self::VolumesEmpty | Self::ContainersEmpty | Self::Finish => vec![],
            Self::ContainerPublished { name, page } => std::iter::once(name.as_str())
                .chain(page.as_deref())
                .collect(),
            Self::DesktopBrowser { name, page } => vec![name, page],
            Self::StdoutEquals { text } | Self::OutputHasLine { text } => vec![text],
            Self::NetworkExists { name }
            | Self::NetworkAbsent { name }
            | Self::VolumeExists { name }
            | Self::ContainerRunning { name }
            | Self::ContainerExited { name, .. }
            | Self::ContainerAbsent { name }
            | Self::DesktopInspectExited { name }
            | Self::DesktopPorts { name }
            | Self::DesktopStop { name }
            | Self::DesktopInspectVolume { name }
            | Self::DesktopDeleteVolume { name } => vec![name],
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StepDef {
    pub key: String,
    pub names: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub check: Check,
}

/// The steps of a course module, as a practice session keeps them.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Course {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub steps: Vec<StepDef>,
    /// Written into the practice directory when the session starts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
}

fn plain(text: &str) -> bool {
    !text.trim().is_empty() && !text.chars().any(char::is_control)
}

/// Whether COMMAND is one plain podbag command when a learner pastes it into a shell:
/// outside quotes no operators, substitutions, redirections or patterns; inside double
/// quotes no expansions. Single quotes keep everything as text, so `sh -c '…'` for the
/// container stays possible.
fn shell_safe(command: &str) -> bool {
    let command = command.replace("{port}", "8080");
    let mut quote = None;
    for c in command.chars() {
        match quote {
            Some('\'') if c == '\'' => quote = None,
            Some('\'') => {}
            Some(_) if c == '"' => quote = None,
            Some(_) if matches!(c, '$' | '`' | '\\' | '!') => return false,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if "|&;<>()$`\\*?[]{}~#!".contains(c) => return false,
            None => {}
        }
    }
    quote.is_none()
}

/// What the practice session keeps in its directory: a module file cannot use these names.
const SESSION_NAMES: [&str; 10] = [
    "bin",
    "run",
    "data",
    "config",
    "ui",
    "docker",
    "api.sock",
    "disk.img",
    "console.log",
    "verified",
];

/// A path for a module file: in the practice directory, not over the session's own files.
fn file_path(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("").to_ascii_lowercase();
    practice_path(path) && !SESSION_NAMES.contains(&first.as_str())
}

/// A docker command that talks to the practice engine: no global options before the
/// command (`-H`, `--context`, `--config` would reach another engine) and no `context`.
fn practice_docker(command: &str) -> bool {
    let mut words = command.split_whitespace().skip(1);
    words
        .next()
        .is_some_and(|word| !word.starts_with('-') && word != "context")
}

/// A relative path in the practice directory: plain names, no `..`.
fn practice_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 200
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        })
}

fn invalid(why: &str) -> io::Error {
    io::Error::other(format!("Invalid course module: {why}."))
}

/// Refuses a course module the app cannot check safely: see the module format in
/// docs/usage.md.
pub fn validate(package: &Package) -> io::Result<()> {
    let id = package.id.as_deref().unwrap_or("");
    if id.is_empty()
        || id.len() > 40
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return Err(invalid("the id must be 1-40 of a-z, 0-9 and -"));
    }
    if super::reserved(id) {
        return Err(invalid(&format!("the id {id} is taken")));
    }
    if package.port.is_some_and(|port| port < 1024) {
        return Err(invalid("the port must be 1024-65535"));
    }
    if package
        .translations
        .get("en")
        .is_none_or(|words| words.url.is_none())
    {
        return Err(invalid("the English translation needs the guide's url"));
    }
    if package.translations.values().any(|words| {
        words.url.as_deref().is_some_and(|url| {
            !url.starts_with("https://podbag.dev/") || url.chars().any(char::is_whitespace)
        })
    }) {
        return Err(invalid("guide links must be on https://podbag.dev/"));
    }
    let v2 = package.format == FORMAT_V2;
    if !v2 && !package.files.is_empty() {
        return Err(invalid("files need podbag-course-v2"));
    }
    if package.files.len() > MAX_FILES
        || package.files.values().map(String::len).sum::<usize>() > FILES_LIMIT
        || !package.files.keys().all(|path| file_path(path))
        || package.files.values().any(|text| {
            text.chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        })
    {
        return Err(invalid(
            "files are at most 8 plain text files, 16 KiB in all, under the practice directory",
        ));
    }
    if package.port.is_none() && package.files.values().any(|text| text.contains("{port}")) {
        return Err(invalid("files use {port} without a port"));
    }
    if package.steps.is_empty() || package.steps.len() > MAX_STEPS {
        return Err(invalid("a course has 1-40 steps"));
    }
    let mut keys = HashSet::new();
    for step in &package.steps {
        let key_ok = !step.key.is_empty()
            && step.key.len() <= 40
            && step.key.bytes().all(|c| c.is_ascii_alphanumeric());
        if !key_ok || !keys.insert(step.key.as_str()) {
            return Err(invalid(&format!("bad or repeated step key {:?}", step.key)));
        }
        if !step.names.contains_key("en") || !step.names.values().all(|name| plain(name)) {
            return Err(invalid(&format!("step {} needs an English name", step.key)));
        }
        if step.check.is_v2() && !v2 {
            return Err(invalid(&format!(
                "step {} needs podbag-course-v2 for its check",
                step.key
            )));
        }
        if matches!(&step.check, Check::FileExists { path } if !practice_path(path)) {
            return Err(invalid(&format!("step {} checks a path outside", step.key)));
        }
        if package.port.is_none() && step.check.values().iter().any(|v| v.contains("{port}")) {
            return Err(invalid(&format!(
                "step {} uses {{port}} without a port",
                step.key
            )));
        }
        if !step.check.values().into_iter().all(plain) {
            return Err(invalid(&format!(
                "step {} has an empty check value",
                step.key
            )));
        }
        let finish = step.check == Check::Finish;
        let command_ok = match step.command.as_deref() {
            None => step.check.in_desktop(),
            Some("exit") => finish,
            Some(command) => {
                !step.check.in_desktop()
                    && !finish
                    && (command.starts_with("podbag ")
                        || (v2 && command.starts_with("docker ") && practice_docker(command)))
                    && plain(command)
                    && shell_safe(command)
                    && command.len() <= 1000
                    && (package.port.is_some() || !command.contains("{port}"))
            }
        };
        if !command_ok {
            return Err(invalid(&format!(
                "step {} has a command it cannot have",
                step.key
            )));
        }
    }
    let last = package.steps.last().unwrap();
    if last.key != "finish" || last.check != Check::Finish {
        return Err(invalid("the last step is finish, with the command exit"));
    }
    Ok(())
}
const STATE: &str = "run/course.json";

/// What a CLI step's check reads from the learner's command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    None,
    Stdout,
    /// stdout, then stderr.
    All,
}

impl StepDef {
    /// The command shown and accepted for this step, with the attempt's port.
    pub fn command(&self, port: u16) -> Option<String> {
        self.command
            .as_ref()
            .map(|command| command.replace("{port}", &port.to_string()))
    }

    /// Steps that publish the attempt's port (`127.0.0.1:{port}:…`): a used port changes
    /// the command first. Other uses of `{port}` reach what such a step published.
    pub fn publishes_port(&self) -> bool {
        self.command
            .as_deref()
            .is_some_and(|command| command.contains("127.0.0.1:{port}:"))
    }

    pub fn output(&self) -> Output {
        match self.check {
            Check::StdoutEquals { .. } => Output::Stdout,
            Check::OutputHasLine { .. } | Check::OutputContains { .. } => Output::All,
            _ => Output::None,
        }
    }

    /// A step the learner runs with the Docker CLI (`bin/docker` in the practice directory).
    pub fn is_docker(&self) -> bool {
        self.command
            .as_deref()
            .is_some_and(|command| command.starts_with("docker "))
    }

    pub fn is_finish(&self) -> bool {
        self.check == Check::Finish
    }

    /// Checks the practice engine after the learner ran this step's command.
    pub fn verify_cli(&self, root: &Path, port: u16, output: &[u8]) -> io::Result<()> {
        let client = containers::client(root);
        let containers = || client.containers().map_err(io::Error::other);
        let text = || std::str::from_utf8(output).unwrap_or("");
        let check = self.check.with_port(port);
        let verified = match &check {
            Check::NetworkExists { name } | Check::NetworkAbsent { name } => {
                let listed = client
                    .networks()
                    .map_err(io::Error::other)?
                    .iter()
                    .any(|net| &net.name == name);
                listed == matches!(check, Check::NetworkExists { .. })
            }
            Check::VolumeExists { name } => client
                .volumes()
                .map_err(io::Error::other)?
                .iter()
                .any(|vol| &vol.name == name),
            Check::VolumesEmpty => client.volumes().map_err(io::Error::other)?.is_empty(),
            Check::ContainersEmpty => containers()?.is_empty(),
            Check::ContainerRunning { name } => containers()?
                .iter()
                .any(|row| row.name() == name && row.state == "running"),
            Check::ContainerPublished { name, page } => {
                containers()?
                    .iter()
                    .any(|row| row.name() == name && published(row, port))
                    // A copied page shows the container reached what it needs; it exits
                    // otherwise.
                    && page.as_deref().is_none_or(|page| {
                        wait_for_page(port, page, Duration::from_secs(5)).is_ok()
                    })
            }
            Check::ContainerExited { name, code } => {
                // `run -d` returns after the launch; the wait endpoint gives the real exit code.
                let reply = client
                    .call("POST", &format!("/containers/{name}/wait"), None)
                    .map_err(io::Error::other)?;
                reply["StatusCode"].as_i64() == Some(*code)
            }
            Check::ContainerAbsent { name } => containers()?.iter().all(|row| row.name() != name),
            Check::StdoutEquals { text: expected } => {
                text().trim_end_matches(['\r', '\n']) == expected
            }
            Check::OutputHasLine { text: expected } => {
                text().lines().any(|line| line.trim() == expected)
            }
            Check::ImageExists { name } | Check::ImageAbsent { name } => {
                let found = client
                    .images()
                    .map_err(io::Error::other)?
                    .iter()
                    .any(|image| image.repo_tags.iter().any(|tag| same_image(tag, name)));
                found == matches!(check, Check::ImageExists { .. })
            }
            Check::ContainerCreated { name }
            | Check::ContainerStopped { name }
            | Check::ContainerPaused { name } => {
                let state = match check {
                    Check::ContainerCreated { .. } => "created",
                    Check::ContainerStopped { .. } => "exited",
                    _ => "paused",
                };
                // `kill` and `pause` return before the state changes.
                poll(Duration::from_secs(10), || {
                    containers()
                        .ok()?
                        .iter()
                        .any(|row| row.name() == name && row.state == state)
                        .then_some(())
                })
            }
            Check::ContainerHealthy { name } => poll(Duration::from_secs(30), || {
                let detail = client.inspect(Kind::Containers, name).ok()?;
                (detail["State"]["Health"]["Status"] == "healthy").then_some(())
            }),
            Check::ContainerRestarted { name, count } => poll(Duration::from_secs(20), || {
                let detail = client.inspect(Kind::Containers, name).ok()?;
                (detail["RestartCount"].as_u64()? >= u64::from(*count)).then_some(())
            }),
            Check::ContainerInNetwork { name, network }
            | Check::ContainerNotInNetwork { name, network } => {
                let detail = client
                    .inspect(Kind::Containers, name)
                    .map_err(io::Error::other)?;
                let attached = detail["NetworkSettings"]["Networks"]
                    .as_object()
                    .is_some_and(|networks| networks.contains_key(network));
                attached == matches!(check, Check::ContainerInNetwork { .. })
            }
            Check::VolumeAbsent { name } => client
                .volumes()
                .map_err(io::Error::other)?
                .iter()
                .all(|vol| &vol.name != name),
            Check::OutputContains { text: expected } => text().contains(expected.as_str()),
            // validate() keeps the path inside the practice directory.
            Check::FileExists { path } => root.join(path).is_file(),
            Check::Finish
            | Check::DesktopInspectExited { .. }
            | Check::DesktopPorts { .. }
            | Check::DesktopBrowser { .. }
            | Check::DesktopStop { .. }
            | Check::DesktopInspectVolume { .. }
            | Check::DesktopDeleteVolume { .. } => false,
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
    pub key: String,
    pub names: BTreeMap<String, String>,
    pub command: Option<String>,
}

/// The steps as Training lists them, with the course's own port.
pub fn instructions(course: &Course) -> Vec<Instruction> {
    let port = course.port.unwrap_or(0);
    course
        .steps
        .iter()
        .map(|step| Instruction {
            key: step.key.clone(),
            names: step.names.clone(),
            command: step.command(port),
        })
        .collect()
}

/// A course attempt in a practice session. It keeps its own copy of the course, so a
/// module update during the session does not change its steps.
#[derive(Deserialize, Serialize)]
pub struct Attempt {
    module: String,
    index: usize,
    pub port: u16,
    course: Course,
}

impl Attempt {
    pub fn start(root: &Path, course: &Course) -> io::Result<()> {
        let port = match course.port {
            Some(preferred) => available_port(preferred)?,
            None => 0,
        };
        prepare(root, course, port)?;
        Self {
            module: course.id.clone(),
            index: 0,
            port,
            course: course.clone(),
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

    pub fn step(&self) -> io::Result<StepDef> {
        self.course
            .steps
            .get(self.index)
            .cloned()
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

/// Asks READY every half second until it answers or WITHIN has gone.
fn poll(within: Duration, mut ready: impl FnMut() -> Option<()>) -> bool {
    let deadline = std::time::Instant::now() + within;
    loop {
        if ready().is_some() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// An image name as the engine lists it: `busybox` is `docker.io/library/busybox:latest`.
fn normal_image(name: &str) -> String {
    let name = name
        .strip_prefix("docker.io/")
        .or_else(|| name.strip_prefix("index.docker.io/"))
        .map(|rest| rest.strip_prefix("library/").unwrap_or(rest))
        .unwrap_or(name);
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.contains(':') || last.contains('@') {
        name.to_owned()
    } else {
        format!("{name}:latest")
    }
}

fn same_image(a: &str, b: &str) -> bool {
    normal_image(a) == normal_image(b)
}

/// The script that sends `docker …` in the practice shell to `podbag lab docker-step`.
const DOCKER_WRAPPER: &str = "#!/bin/sh\nexec \"$PODBAG_BIN\" lab docker-step \"$@\"\n";

/// Writes the course's files, and the docker wrapper when a step uses the Docker CLI.
fn prepare(root: &Path, course: &Course, port: u16) -> io::Result<()> {
    for (path, text) in &course.files {
        // Folder by folder, never through a link, and never over a file that is there.
        let mut folder = root.to_path_buf();
        let parts: Vec<_> = path.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            folder.push(part);
            match fs::symlink_metadata(&folder) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(io::Error::other(format!("{path}: not a folder"))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&folder)?,
                Err(e) => return Err(e),
            }
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(folder.join(parts[parts.len() - 1]))?;
        file.write_all(text.replace("{port}", &port.to_string()).as_bytes())?;
    }
    if course.steps.iter().any(StepDef::is_docker) {
        fs::create_dir_all(root.join("bin"))?;
        let wrapper = root.join("bin/docker");
        fs::write(&wrapper, DOCKER_WRAPPER)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
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

/// check_page until it passes or WITHIN has gone: a new container's server starts a
/// moment after its published port does.
fn wait_for_page(port: u16, expected: &str, within: Duration) -> io::Result<()> {
    let deadline = std::time::Instant::now() + within;
    loop {
        match check_page(port, expected) {
            Err(e) if std::time::Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
            ok => return ok,
        }
    }
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
    stage: String,
    command: Option<String>,
    containers: Vec<ContainerSummary>,
    volumes: Vec<VolumeSummary>,
    detail: Option<serde_json::Value>,
    web: Option<Web>,
    stop: Option<String>,
    inspect_container: Option<String>,
    remove_volume: Option<String>,
}

#[derive(Serialize)]
struct Web {
    container: String,
    port: u16,
    ready: bool,
    url: String,
}

/// Desktop's view of the practice session, after doing ACTION when the current step allows it.
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
    let check = step.check.with_port(attempt.port);
    match (action, &check) {
        ("status", _) => {}
        ("inspect-failed", Check::DesktopInspectExited { name })
            if row(name)?.id == target && row(name)?.state == "exited" =>
        {
            attempt.advance(&root)?;
        }
        ("ports", Check::DesktopPorts { name })
            if published(row(name)?, attempt.port) && row(name)?.id == target =>
        {
            attempt.advance(&root)?;
        }
        ("open-web" | "show-web", Check::DesktopBrowser { name, page })
            if published(row(name)?, attempt.port) && row(name)?.id == target =>
        {
            check_page(attempt.port, page).map_err(|e| {
                io::Error::other(format!("Check the published page and try again: {e}"))
            })?;
            if action == "open-web" {
                attempt.advance(&root)?;
            }
        }
        ("stop", Check::DesktopStop { name }) if row(name)?.id == target => {
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
        ("inspect", check) if volumes.iter().any(|v| v.name == target) => {
            detail = Some(
                client
                    .inspect(Kind::Volumes, target)
                    .map_err(io::Error::other)?,
            );
            if matches!(check, Check::DesktopInspectVolume { name } if name == target) {
                attempt.advance(&root)?;
            }
        }
        ("remove", Check::DesktopDeleteVolume { name }) if name == target => {
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
    let check = step.check.with_port(attempt.port);
    let web = match &check {
        Check::DesktopPorts { name } | Check::DesktopBrowser { name, .. } => Some(Web {
            container: name.clone(),
            port: attempt.port,
            ready: matches!(check, Check::DesktopBrowser { .. }),
            url: format!("http://127.0.0.1:{}/", attempt.port),
        }),
        _ => None,
    };
    let named = |wanted: fn(&Check) -> Option<&String>| wanted(&check).cloned();
    Ok(Some(Snapshot {
        stop: named(|c| match c {
            Check::DesktopStop { name } => Some(name),
            _ => None,
        }),
        inspect_container: named(|c| match c {
            Check::DesktopInspectExited { name } => Some(name),
            _ => None,
        }),
        remove_volume: named(|c| match c {
            Check::DesktopDeleteVolume { name } => Some(name),
            _ => None,
        }),
        command: step.command(attempt.port),
        stage: step.key,
        module: attempt.module,
        session,
        containers: rows,
        volumes,
        detail,
        web,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(id: &str) -> Course {
        let text = match id {
            "web-basics" => include_str!("../../tests/fixtures/training/web-basics.podbag"),
            "web-service-recovery" => {
                include_str!("../../tests/fixtures/training/web-service-recovery.podbag")
            }
            "container-network" => {
                include_str!("../../tests/fixtures/training/container-network.podbag")
            }
            _ => unreachable!(),
        };
        crate::volume_lesson::parse_package(text)
            .unwrap()
            .course()
            .unwrap()
    }

    fn step(course: &Course, key: &str) -> StepDef {
        course.steps.iter().find(|s| s.key == key).unwrap().clone()
    }

    /// An attempt at INDEX of COURSE, in a new session directory.
    fn attempt_at(course: &Course, index: usize) -> (tempfile::TempDir, Attempt) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("run")).unwrap();
        Attempt::start(dir.path(), course).unwrap();
        let mut attempt = Attempt::read(dir.path()).unwrap();
        attempt.index = index;
        attempt.save(dir.path()).unwrap();
        (dir, attempt)
    }

    #[test]
    fn recovery_steps_require_the_missing_title_message() {
        let dir = tempfile::tempdir().unwrap();
        let logs = step(&fixture("web-service-recovery"), "readRecoveryLogs");
        assert_eq!(logs.output(), Output::All);
        assert!(
            logs.verify_cli(dir.path(), 0, b"SITE_TITLE is required\n")
                .is_ok()
        );
        assert!(
            logs.verify_cli(dir.path(), 0, b"Listening on 8080\n")
                .is_err()
        );
    }

    #[test]
    fn network_steps_have_unique_keys_and_the_shown_commands() {
        let steps = instructions(&fixture("container-network"));
        let keys: Vec<_> = steps.iter().map(|step| step.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "createNetwork",
                "runApi",
                "callByName",
                "runFront",
                "ports",
                "browser",
                "stop",
                "remove",
                "removeApi",
                "deleteNetwork",
                "finish",
            ]
        );
        let command = |key| {
            steps
                .iter()
                .find(|step| step.key == key)
                .and_then(|step| step.command.clone())
                .unwrap()
        };
        assert_eq!(command("createNetwork"), "podbag network create lab-net");
        assert!(command("runApi").starts_with("podbag run -d --name api --network lab-net "));
        assert!(!command("runApi").contains("127.0.0.1:"));
        assert_eq!(
            command("callByName"),
            "podbag run --rm --network lab-net docker.io/library/busybox:1.37.0 wget -qO- http://api:8080/"
        );
        assert!(
            command("runFront").contains("--name web --network lab-net -p 127.0.0.1:18083:8080 ")
        );
        assert!(command("runFront").contains("wget -qO /www/index.html http://api:8080/"));
        assert_eq!(command("remove"), "podbag rm web");
        assert_eq!(command("removeApi"), "podbag rm -f api");
        assert_eq!(command("deleteNetwork"), "podbag network rm lab-net");
        let names = &steps[0].names;
        assert_eq!(names["ko"], "CLI에서 네트워크 만들기");
    }

    #[test]
    fn calling_by_name_needs_the_api_page() {
        let dir = tempfile::tempdir().unwrap();
        let call = step(&fixture("container-network"), "callByName");
        assert_eq!(call.output(), Output::Stdout);
        assert!(call.verify_cli(dir.path(), 0, b"Hello from api\n").is_ok());
        assert!(
            call.verify_cli(dir.path(), 0, b"Hello from api\r\n")
                .is_ok()
        );
        assert!(
            call.verify_cli(dir.path(), 0, b"wget: bad address 'api:8080'\n")
                .is_err()
        );
        assert!(call.verify_cli(dir.path(), 0, b"").is_err());
    }

    #[test]
    fn a_used_port_updates_the_front_command() {
        let course = fixture("container-network");
        let (dir, mut attempt) = attempt_at(&course, 3);
        let root = dir.path();
        let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        attempt.port = reserved.local_addr().unwrap().port();
        attempt.save(root).unwrap();
        let front = attempt.step().unwrap();
        assert_eq!(front.key, "runFront");
        // course_command refreshes the port of these steps before the learner's command runs.
        assert!(front.publishes_port());
        assert!(!step(&course, "runApi").publishes_port());
        assert!(!step(&course, "callByName").publishes_port());
        assert_eq!(front.output(), Output::None);
        assert!(attempt.refresh_port(root).unwrap());
        let stored = Attempt::read(root).unwrap();
        assert_ne!(stored.port, reserved.local_addr().unwrap().port());
        assert!(
            stored
                .step()
                .unwrap()
                .command(stored.port)
                .unwrap()
                .contains(&format!("127.0.0.1:{}:8080", stored.port))
        );
    }

    #[test]
    fn a_used_port_updates_the_command_in_this_attempt() {
        let (dir, mut attempt) = attempt_at(&fixture("web-basics"), 0);
        let root = dir.path();
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
    fn a_session_keeps_its_course_copy() {
        let library = tempfile::tempdir().unwrap();
        let library = library.path();
        let text = include_str!("../../tests/fixtures/training/web-basics.podbag");
        let module = crate::volume_lesson::install(library, text).unwrap();
        let installed = crate::volume_lesson::read(library, &module)
            .unwrap()
            .unwrap();
        let session = tempfile::tempdir().unwrap();
        fs::create_dir(session.path().join("run")).unwrap();
        Attempt::start(session.path(), &installed.course().unwrap()).unwrap();
        // A sync replaces the installed module while the session runs.
        crate::volume_lesson::install(library, &text.replace("\"runWeb\"", "\"runServer\""))
            .unwrap();
        let stored = Attempt::read(session.path()).unwrap();
        assert_eq!(stored.step().unwrap().key, "runWeb");
        assert!(!stored.step().unwrap().is_finish());
    }

    fn cli_step(command: &str, check: Check) -> StepDef {
        StepDef {
            key: "step".into(),
            names: BTreeMap::from([("en".into(), "A step".into())]),
            command: Some(command.into()),
            check,
        }
    }

    #[test]
    fn output_contains_reads_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let step = cli_step(
            "docker ps -a",
            Check::OutputContains {
                text: "CONTAINER ID".into(),
            },
        );
        assert_eq!(step.output(), Output::All);
        assert!(
            step.verify_cli(dir.path(), 0, b"CONTAINER ID   IMAGE\n")
                .is_ok()
        );
        assert!(step.verify_cli(dir.path(), 0, b"Cannot connect\n").is_err());
        assert!(step.is_docker());
        assert!(!cli_step("podbag ps", Check::ContainersEmpty).is_docker());
    }

    #[test]
    fn file_exists_looks_in_the_practice_directory() {
        let dir = tempfile::tempdir().unwrap();
        let step = cli_step(
            "podbag export app -o out/app.tar",
            Check::FileExists {
                path: "out/app.tar".into(),
            },
        );
        assert!(step.verify_cli(dir.path(), 0, b"").is_err());
        fs::create_dir(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/app.tar"), b"tar").unwrap();
        assert!(step.verify_cli(dir.path(), 0, b"").is_ok());
    }

    #[test]
    fn a_session_writes_its_files_and_the_docker_wrapper() {
        let mut course = fixture("web-basics");
        course.files = BTreeMap::from([
            ("Dockerfile".into(), "FROM busybox:1.37.0\n".into()),
            ("site/index.html".into(), "Hello\n".into()),
        ]);
        let (dir, _) = attempt_at(&course, 0);
        let root = dir.path();
        assert_eq!(
            fs::read_to_string(root.join("Dockerfile")).unwrap(),
            "FROM busybox:1.37.0\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("site/index.html")).unwrap(),
            "Hello\n"
        );
        assert!(
            !root.join("bin/docker").exists(),
            "no docker steps, no wrapper"
        );

        course.steps[0].command = Some("docker ps".into());
        let (dir, _) = attempt_at(&course, 0);
        let wrapper = fs::read_to_string(dir.path().join("bin/docker")).unwrap();
        assert!(wrapper.contains("lab docker-step"), "{wrapper}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("bin/docker"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }

    #[test]
    fn image_names_match_with_or_without_the_registry() {
        assert!(same_image(
            "busybox:1.37.0",
            "docker.io/library/busybox:1.37.0"
        ));
        assert!(same_image(
            "docker.io/library/busybox:1.37.0",
            "busybox:1.37.0"
        ));
        assert!(same_image(
            "127.0.0.1:5000/my/app:1",
            "127.0.0.1:5000/my/app:1"
        ));
        assert!(!same_image("busybox:1.36.0", "busybox:1.37.0"));
        assert!(!same_image("my/busybox:1.37.0", "busybox:1.37.0"));
    }

    #[test]
    fn only_publishing_steps_refresh_the_port() {
        let check = || Check::ContainersEmpty;
        assert!(
            cli_step("podbag run -d -p 127.0.0.1:{port}:5000 registry", check()).publishes_port()
        );
        assert!(
            !cli_step("podbag push 127.0.0.1:{port}/training/busybox:1", check()).publishes_port()
        );
        assert!(
            !cli_step(
                "podbag tag busybox 127.0.0.1:{port}/training/busybox:1",
                check()
            )
            .publishes_port()
        );
    }

    #[test]
    fn the_port_reaches_check_values_and_files() {
        let image = Check::ImageExists {
            name: "127.0.0.1:{port}/training/busybox:1".into(),
        };
        assert_eq!(
            image.with_port(18105),
            Check::ImageExists {
                name: "127.0.0.1:18105/training/busybox:1".into()
            }
        );
        let mut course = fixture("web-basics");
        course.files = BTreeMap::from([(
            "compose.yaml".into(),
            "ports: [\"127.0.0.1:{port}:8080\"]\n".into(),
        )]);
        let (dir, attempt) = attempt_at(&course, 0);
        assert_eq!(
            fs::read_to_string(dir.path().join("compose.yaml")).unwrap(),
            format!("ports: [\"127.0.0.1:{}:8080\"]\n", attempt.port)
        );
    }

    #[cfg(unix)]
    #[test]
    fn files_never_replace_what_is_there() {
        let mut course = fixture("web-basics");
        course.files = BTreeMap::from([("notes.txt".into(), "from the module\n".into())]);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("run")).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "keep me").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("notes.txt")).unwrap();
        assert!(Attempt::start(root, &course).is_err());
        assert_eq!(fs::read_to_string(outside.path()).unwrap(), "keep me");

        // Nor through a linked folder.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("run")).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.join("site")).unwrap();
        course.files = BTreeMap::from([("site/index.html".into(), "x".into())]);
        assert!(Attempt::start(root, &course).is_err());
        assert!(!elsewhere.path().join("index.html").exists());
    }

    #[test]
    fn the_practice_socket_matches_the_engine() {
        let root = Path::new("/p");
        let socket = containers::socket(root);
        if cfg!(target_os = "macos") {
            assert_eq!(socket, root.join("run/docker.sock"));
        } else {
            assert_eq!(socket, root.join("api.sock"));
        }
    }

    /// A server that drops the first FAILS connections, then serves the api page.
    fn late_server(fails: usize) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let mut stream = stream.unwrap();
                if n < fails {
                    continue;
                }
                let mut request = [0; 128];
                let _ = stream.read(&mut request).unwrap();
                write!(stream, "HTTP/1.0 200 OK\r\n\r\nHello from api\n").unwrap();
                return;
            }
        });
        (port, server)
    }

    #[test]
    fn the_front_check_waits_for_the_copied_page() {
        // The published port answers before httpd in the container does.
        let (port, server) = late_server(2);
        assert!(wait_for_page(port, "Hello from api", Duration::from_secs(5)).is_ok());
        server.join().unwrap();
        // web never got the page from api: the step fails.
        let (port, _server) = late_server(usize::MAX);
        let started = std::time::Instant::now();
        assert!(wait_for_page(port, "Hello from api", Duration::from_millis(600)).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
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
                let _ = stream.read(&mut request).unwrap();
                write!(stream, "HTTP/1.0 {status}\r\n\r\n{body}").unwrap();
            });
            assert_eq!(check_page(port, "My Podbag site").is_ok(), expected);
            server.join().unwrap();
        }
    }
}
