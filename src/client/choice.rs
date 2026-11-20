//! Which engine the TUI and the desktop app manage: podbag, Docker (Docker Engine or
//! Docker Desktop), or a socket given by path. The choice is saved in `ui.json` in
//! [`ui_dir`], which both programs read.

use super::Endpoint;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// An engine to manage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// podbag's own socket (started when needed).
    Podbag,
    /// Docker's socket, found as the docker CLI finds it. Never started.
    Docker,
    /// A socket given by path.
    Socket(PathBuf),
}

impl Choice {
    /// `podbag`, `docker`, a `unix://` URL or an absolute path.
    pub fn parse(text: &str) -> Option<Choice> {
        let text = text.trim();
        match text.to_ascii_lowercase().as_str() {
            "podbag" => return Some(Choice::Podbag),
            "docker" => return Some(Choice::Docker),
            _ => {}
        }
        let path = text.strip_prefix("unix://").unwrap_or(text);
        (Path::new(path).is_absolute()).then(|| Choice::Socket(PathBuf::from(path)))
    }

    /// As saved: `podbag`, `docker` or `unix:///path`.
    pub fn to_setting(&self) -> String {
        match self {
            Choice::Podbag => "podbag".into(),
            Choice::Docker => "docker".into(),
            Choice::Socket(p) => format!("unix://{}", p.display()),
        }
    }

    /// What the user sees: `podbag`, `Docker` or the socket's path.
    pub fn label(&self) -> String {
        match self {
            Choice::Podbag => "podbag".into(),
            Choice::Docker => "Docker".into(),
            Choice::Socket(p) => p.display().to_string(),
        }
    }
}

/// An engine and where its API is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The engine in use. With no saved choice and `DOCKER_HOST` set to a socket, the
    /// socket.
    pub choice: Choice,
    pub endpoint: Endpoint,
}

impl Target {
    /// The engine for `choice`; None: the saved choice, else today's rule
    /// (`DOCKER_HOST` when it is `unix://…`, else podbag).
    pub fn find(choice: Option<&Choice>, podbag: &Path) -> Result<Target, String> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let saved = if choice.is_none() { saved() } else { None };
        match choice.or(saved.as_ref()) {
            None => {
                let endpoint = Endpoint::find(podbag)?;
                let choice = match &endpoint {
                    Endpoint::Socket { path, own: false } => Choice::Socket(path.clone()),
                    _ => Choice::Podbag,
                };
                Ok(Target { choice, endpoint })
            }
            Some(Choice::Podbag) => Ok(Target {
                choice: Choice::Podbag,
                endpoint: Endpoint::podbag_default(podbag)?,
            }),
            Some(Choice::Docker) => Ok(Target {
                choice: Choice::Docker,
                endpoint: docker_endpoint(&env, home.as_deref(), &docker_cli()),
            }),
            Some(Choice::Socket(p)) => Ok(Target {
                choice: Choice::Socket(p.clone()),
                endpoint: Endpoint::Socket {
                    path: p.clone(),
                    own: false,
                },
            }),
        }
    }

    pub fn is_docker(&self) -> bool {
        self.choice == Choice::Docker
    }

    /// Make sure something answers: podbag is started when needed (see
    /// [`Endpoint::ensure_running`]); Docker never is.
    pub fn ensure_running(&self, podbag: &Path, start_vm: bool) -> Result<(), String> {
        match (&self.choice, &self.endpoint) {
            (Choice::Docker, Endpoint::Socket { .. }) if !self.endpoint.listening() => {
                Err(docker_down().into())
            }
            (Choice::Docker, _) => Ok(()),
            _ => self.endpoint.ensure_running(podbag, start_vm),
        }
    }
}

/// What to say when Docker does not answer.
pub fn docker_down() -> &'static str {
    if cfg!(target_os = "linux") {
        "Docker is not running. Start it: sudo systemctl start docker"
    } else {
        "Docker is not running. Start Docker Desktop."
    }
}

/// Where Docker's API is, in the docker CLI's order: `DOCKER_HOST`; the current context
/// (`DOCKER_CONTEXT`, or `currentContext` in the CLI's config.json); then the usual
/// sockets (the first that exists, else the first). A host that is not a Unix socket
/// (tcp, ssh, a Windows named pipe) is reached through `docker system dial-stdio`.
pub fn docker_endpoint(
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
    docker: &Path,
) -> Endpoint {
    let dial = || {
        Endpoint::Command(vec![
            docker.as_os_str().to_owned(),
            OsString::from("system"),
            OsString::from("dial-stdio"),
        ])
    };
    let host = |h: &str| Endpoint::from_docker_host(h).unwrap_or_else(dial);
    if let Some(h) = env("DOCKER_HOST") {
        return host(&h);
    }
    if let Some(config) = &docker_config_dir(env, home)
        && let Some(h) = context_host(config, env("DOCKER_CONTEXT"))
    {
        return host(&h);
    }
    if cfg!(windows) {
        return dial();
    }
    let mut places: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        places.extend(home.map(|h| h.join(".docker/run/docker.sock")));
    } else {
        places.extend(env("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("docker.sock")));
    }
    places.push(PathBuf::from("/var/run/docker.sock"));
    let path = places
        .iter()
        .find(|p| p.exists())
        .unwrap_or(&places[0])
        .clone();
    Endpoint::Socket { path, own: false }
}

/// The host of the docker CLI's current context; None for `default` (DOCKER_HOST or the
/// usual socket) or when there is none.
fn context_host(config: &Path, named: Option<String>) -> Option<String> {
    let name = named.or_else(|| current_context(config))?;
    if name.is_empty() || name == "default" {
        return None;
    }
    context_endpoint(config, &name)
}

/// The docker CLI's config folder: `DOCKER_CONFIG`, else `~/.docker`.
pub fn docker_config_dir(
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    env("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".docker")))
}

/// `currentContext` in the docker CLI's config.json, if set.
pub(super) fn current_context(config: &Path) -> Option<String> {
    let text = std::fs::read(config.join("config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&text).ok()?;
    v["currentContext"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// The Docker host of context `name`, from `contexts/meta/<sha256 of the name>/meta.json`.
pub(super) fn context_endpoint(config: &Path, name: &str) -> Option<String> {
    use sha2::Digest;
    let dir: String = sha2::Sha256::digest(name.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let meta = config.join("contexts/meta").join(dir).join("meta.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
    v["Endpoints"]["docker"]["Host"]
        .as_str()
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// The docker CLI: in `PATH`, or where Docker Desktop and package managers put it (a
/// desktop app gets a short `PATH`). `docker` when it is not found.
pub fn docker_cli() -> PathBuf {
    find_docker_cli().unwrap_or_else(|| {
        PathBuf::from(if cfg!(windows) {
            "docker.exe"
        } else {
            "docker"
        })
    })
}

/// The docker CLI, if it is installed.
pub fn find_docker_cli() -> Option<PathBuf> {
    find_tool("docker")
}

/// A program of the Docker tools (`docker`, `docker-credential-osxkeychain`, …): in
/// `PATH`, else where Docker Desktop and package managers put it. `.exe` is added on
/// Windows.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".docker/bin"));
    }
    if cfg!(windows) {
        dirs.push(PathBuf::from(
            r"C:\Program Files\Docker\Docker\resources\bin",
        ));
    } else {
        dirs.extend(
            [
                "/usr/local/bin",
                "/opt/homebrew/bin",
                "/usr/bin",
                "/Applications/Docker.app/Contents/Resources/bin",
            ]
            .map(PathBuf::from),
        );
    }
    dirs.into_iter()
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

/// The folder of the UIs' shared files (`ui.json`, the update check's `update.json`):
/// macOS `~/.podbag` (`PODBAG_MAC_HOME`), Linux podbag's config folder
/// (`~/.config/podbag`), Windows `%LOCALAPPDATA%\podbag`. `PODBAG_UI_DIR` sets another.
pub fn ui_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PODBAG_UI_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    #[cfg(target_os = "macos")]
    {
        crate::macos::host::paths().ok().map(|p| p.home)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        crate::paths::dirs().ok().map(|d| d.config)
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(|p| PathBuf::from(p).join("AppData").join("Local"))
            })
            .map(|d| d.join("podbag"))
    }
}

/// The saved choice, if any.
pub fn saved() -> Option<Choice> {
    saved_in(&ui_dir()?.join("ui.json"))
}

pub fn saved_in(file: &Path) -> Option<Choice> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    Choice::parse(v["engine"].as_str()?)
}

/// Save the choice for both UIs.
pub fn save(choice: &Choice) -> Result<(), String> {
    let dir = ui_dir().ok_or("cannot find the folder for ui.json")?;
    save_in(&dir.join("ui.json"), choice)
}

/// Save the choice in `file`; other keys there stay.
pub fn save_in(file: &Path, choice: &Choice) -> Result<(), String> {
    update_json(file, |v| {
        v["engine"] = serde_json::Value::String(choice.to_setting());
    })
}

/// Change a JSON object file in place: written beside it, then renamed over it.
pub fn update_json(file: &Path, change: impl FnOnce(&mut serde_json::Value)) -> Result<(), String> {
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let mut v: serde_json::Value = std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    change(&mut v);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| err(dir, e))?;
    }
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let tmp = file.with_file_name(format!(".{name}.{}", std::process::id()));
    let text = serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| err(&tmp, e))?;
    std::fs::rename(&tmp, file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        err(file, e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn socket(p: impl Into<PathBuf>) -> Endpoint {
        Endpoint::Socket {
            path: p.into(),
            own: false,
        }
    }

    #[test]
    fn choices_parse_and_save_as_text() {
        assert_eq!(Choice::parse("podbag"), Some(Choice::Podbag));
        assert_eq!(Choice::parse(" Docker "), Some(Choice::Docker));
        assert_eq!(
            Choice::parse("unix:///run/docker.sock"),
            Some(Choice::Socket("/run/docker.sock".into()))
        );
        assert_eq!(
            Choice::parse("/tmp/x.sock"),
            Some(Choice::Socket("/tmp/x.sock".into()))
        );
        assert_eq!(Choice::parse("tcp://1.2.3.4:2375"), None);
        assert_eq!(Choice::parse("relative.sock"), None);
        assert_eq!(Choice::parse(""), None);
        let c = Choice::Socket("/a/b.sock".into());
        assert_eq!(Choice::parse(&c.to_setting()), Some(c));
    }

    #[test]
    fn the_choice_is_saved_with_the_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/ui.json");
        assert_eq!(saved_in(&file), None);
        save_in(&file, &Choice::Docker).unwrap();
        assert_eq!(saved_in(&file), Some(Choice::Docker));
        std::fs::write(&file, r#"{"engine": "docker", "other": 1}"#).unwrap();
        save_in(&file, &Choice::Podbag).unwrap();
        assert_eq!(saved_in(&file), Some(Choice::Podbag));
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(v["other"], 1);
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(saved_in(&file), None);
    }

    #[test]
    fn docker_host_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let docker = Path::new("/usr/bin/docker");
        let e = docker_endpoint(
            &env_of(&[("DOCKER_HOST", "unix:///x/d.sock")]),
            Some(dir.path()),
            docker,
        );
        assert_eq!(e, socket("/x/d.sock"));
        // Not a socket: through the docker CLI, which knows how to reach it.
        let e = docker_endpoint(
            &env_of(&[("DOCKER_HOST", "ssh://me@box")]),
            Some(dir.path()),
            docker,
        );
        assert_eq!(
            e,
            Endpoint::Command(vec![
                "/usr/bin/docker".into(),
                "system".into(),
                "dial-stdio".into()
            ])
        );
    }

    /// A docker CLI config with context `name` pointing at `host`.
    fn context(config: &Path, name: &str, host: &str, current: bool) {
        use sha2::Digest;
        let dir: String = sha2::Sha256::digest(name.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let meta = config.join("contexts/meta").join(dir);
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(
            meta.join("meta.json"),
            serde_json::json!({"Name": name, "Endpoints": {"docker": {"Host": host}}}).to_string(),
        )
        .unwrap();
        if current {
            std::fs::write(
                config.join("config.json"),
                serde_json::json!({"currentContext": name}).to_string(),
            )
            .unwrap();
        }
    }

    #[test]
    fn then_the_current_context() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".docker");
        context(&config, "desktop-linux", "unix:///ctx/docker.sock", true);
        context(&config, "other", "unix:///other/docker.sock", false);
        let docker = Path::new("docker");
        assert_eq!(
            docker_endpoint(&env_of(&[]), Some(home.path()), docker),
            socket("/ctx/docker.sock")
        );
        // DOCKER_CONTEXT names another one.
        assert_eq!(
            docker_endpoint(
                &env_of(&[("DOCKER_CONTEXT", "other")]),
                Some(home.path()),
                docker
            ),
            socket("/other/docker.sock")
        );
        // DOCKER_CONFIG moves the config.
        let elsewhere = tempfile::tempdir().unwrap();
        context(elsewhere.path(), "far", "unix:///far.sock", true);
        let cfg = elsewhere.path().display().to_string();
        assert_eq!(
            docker_endpoint(
                &env_of(&[("DOCKER_CONFIG", &cfg)]),
                Some(home.path()),
                docker
            ),
            socket("/far.sock")
        );
        // DOCKER_HOST still wins.
        assert_eq!(
            docker_endpoint(
                &env_of(&[("DOCKER_HOST", "unix:///h.sock")]),
                Some(home.path()),
                docker
            ),
            socket("/h.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn then_the_usual_places() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".docker");
        // The default context means the usual places.
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.json"),
            r#"{"currentContext": "default"}"#,
        )
        .unwrap();
        let run = tempfile::tempdir().unwrap();
        let run_dir = run.path().display().to_string();
        let env = env_of(&[("XDG_RUNTIME_DIR", &run_dir)]);
        let first = if cfg!(target_os = "macos") {
            home.path().join(".docker/run/docker.sock")
        } else {
            run.path().join("docker.sock")
        };
        let docker = Path::new("docker");
        // None exists: the first (or /var/run/docker.sock when it is there).
        let none = docker_endpoint(&env, Some(home.path()), docker);
        let var_run = Path::new("/var/run/docker.sock");
        if var_run.exists() {
            assert_eq!(none, socket(var_run));
        } else {
            assert_eq!(none, socket(&first));
        }
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::write(&first, "").unwrap();
        assert_eq!(
            docker_endpoint(&env, Some(home.path()), docker),
            socket(&first)
        );
    }

    #[cfg(unix)]
    #[test]
    fn docker_is_never_started() {
        let t = Target {
            choice: Choice::Docker,
            endpoint: socket("/nonexistent/docker.sock"),
        };
        assert_eq!(
            t.ensure_running(Path::new("/bin/false"), true),
            Err(docker_down().to_string())
        );
    }

    #[test]
    fn an_explicit_choice_wins_over_docker_host() {
        let t = Target::find(Some(&Choice::Socket("/s.sock".into())), Path::new("podbag")).unwrap();
        assert_eq!(t.endpoint, socket("/s.sock"));
        assert!(!t.is_docker());
    }
}
