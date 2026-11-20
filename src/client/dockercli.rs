//! Where the `docker` command goes (the docker CLI's target), and switching it to podbag
//! with a docker context named `podbag`. Other contexts are never changed or removed.

use super::choice::{self, context_endpoint, current_context, docker_config_dir};
use super::run::{self, Run};
use std::path::{Path, PathBuf};

/// The docker context podbag makes for itself.
pub const PODBAG_CONTEXT: &str = "podbag";

/// The key in `ui.json` that holds the context in use before [`use_podbag`].
const PREVIOUS_KEY: &str = "previousDockerContext";

/// Where the docker CLI sends its commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerTarget {
    /// The context in use; None when `DOCKER_HOST` is set (contexts do not apply then).
    pub context: Option<String>,
    /// The engine's address: `unix:///…`, `tcp://…`, `npipe://…`, `ssh://…`.
    pub host: String,
    /// What to show: `podbag`, `Docker Desktop`, `OrbStack`, `Colima`, `Rancher Desktop`,
    /// `Podman`, else the host.
    pub name: String,
    /// The host is podbag's own socket.
    pub is_podbag: bool,
    /// It comes from `DOCKER_HOST`: shells that set it do not follow a context switch.
    pub from_env: bool,
}

impl DockerTarget {
    /// The target for this program's environment.
    pub fn current(podbag_socket: &Path) -> DockerTarget {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        DockerTarget::find(&env, home.as_deref(), podbag_socket)
    }

    /// The target as the docker CLI finds it: `DOCKER_HOST`, else the current context
    /// (`DOCKER_CONTEXT`, else `currentContext` in the CLI's config.json). Only the
    /// CLI's config folder is read.
    pub fn find(
        env: &dyn Fn(&str) -> Option<String>,
        home: Option<&Path>,
        podbag_socket: &Path,
    ) -> DockerTarget {
        if let Some(host) = env("DOCKER_HOST") {
            let (name, is_podbag) = name_of(None, &host, home, podbag_socket);
            return DockerTarget {
                context: None,
                host,
                name,
                is_podbag,
                from_env: true,
            };
        }
        let config = docker_config_dir(env, home);
        let context = env("DOCKER_CONTEXT")
            .or_else(|| config.as_deref().and_then(current_context))
            .unwrap_or_else(|| "default".into());
        let host = match (context.as_str(), &config) {
            ("default", _) | (_, None) => default_host().to_string(),
            (name, Some(config)) => context_endpoint(config, name).unwrap_or_default(),
        };
        let (name, is_podbag) = name_of(Some(&context), &host, home, podbag_socket);
        DockerTarget {
            context: Some(context),
            host,
            name,
            is_podbag,
            from_env: false,
        }
    }
}

/// The name to show for `host` (reached through `context`), and whether it is podbag.
fn name_of(
    context: Option<&str>,
    host: &str,
    home: Option<&Path>,
    podbag_socket: &Path,
) -> (String, bool) {
    let socket = host.strip_prefix("unix://").map(PathBuf::from);
    if let Some(path) = &socket {
        // The default socket is often a link to an engine's own socket.
        let real = std::fs::read_link(path).unwrap_or_else(|_| path.clone());
        if same_file(path, podbag_socket) || same_file(&real, podbag_socket) {
            return ("podbag".into(), true);
        }
    }
    let under = |dir: &str| {
        socket.as_ref().is_some_and(|p| {
            let real = std::fs::read_link(p).unwrap_or_else(|_| p.clone());
            home.is_some_and(|h| p.starts_with(h.join(dir)) || real.starts_with(h.join(dir)))
        })
    };
    let context = context.unwrap_or("");
    let name = if context == "desktop-linux"
        || under(".docker/run")
        || under("Library/Containers/com.docker.docker")
        || host.ends_with("/pipe/docker_engine")
    {
        "Docker Desktop"
    } else if context == "orbstack" || under(".orbstack") {
        "OrbStack"
    } else if context.starts_with("colima") || under(".colima") {
        "Colima"
    } else if context == "rancher-desktop" || under(".rd") {
        "Rancher Desktop"
    } else if host.contains("podman") {
        "Podman"
    } else if host.is_empty() {
        context
    } else {
        host
    };
    (name.to_string(), false)
}

/// The same path, or the same file once links are followed.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// The docker CLI's host when nothing else is set.
pub fn default_host() -> &'static str {
    if cfg!(windows) {
        "npipe:////./pipe/docker_engine"
    } else {
        "unix:///var/run/docker.sock"
    }
}

/// Point the `docker` command at podbag: make the context `podbag` (or fix its host),
/// remember the context in use in `ui.json`, then use `podbag`.
pub fn use_podbag(docker: &Path, podbag_socket: &Path) -> Result<(), String> {
    use_podbag_with(&run::system, &ui_json()?, docker, podbag_socket)
}

/// Go back to the context in use before [`use_podbag`] (`default` when none is known).
/// Gives the context now in use.
pub fn switch_back(docker: &Path) -> Result<String, String> {
    switch_back_with(&run::system, &ui_json()?, docker)
}

/// The context [`switch_back`] returns to, if one is remembered.
pub fn previous_context() -> Option<String> {
    previous_in(&ui_json().ok()?)
}

fn ui_json() -> Result<PathBuf, String> {
    Ok(choice::ui_dir()
        .ok_or("cannot find the folder for ui.json")?
        .join("ui.json"))
}

fn previous_in(file: &Path) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    v[PREVIOUS_KEY]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// Run the docker CLI; its error text when it fails.
fn docker_cli(run: &Run, docker: &Path, args: &[&str]) -> Result<String, String> {
    let what = format!("docker {}", args.join(" "));
    let ran = run(docker, args, None).map_err(|e| format!("{}: {e}", docker.display()))?;
    if ran.ok {
        Ok(ran.stdout_text())
    } else {
        Err(format!("{what}: {}", ran.error(&what)))
    }
}

fn use_podbag_with(
    run: &Run,
    ui_json: &Path,
    docker: &Path,
    podbag_socket: &Path,
) -> Result<(), String> {
    if podbag_socket.as_os_str().is_empty() {
        return Err("This host has no Podbag Unix socket for the Docker CLI. Use the Docker CLI inside WSL.".into());
    }
    let want = format!("unix://{}", podbag_socket.display());
    let spec = format!("host={want}");
    let found = docker_cli(run, docker, &["context", "inspect", PODBAG_CONTEXT]).ok();
    match found {
        None => {
            docker_cli(
                run,
                docker,
                &["context", "create", PODBAG_CONTEXT, "--docker", &spec],
            )?;
        }
        Some(text) => {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if v[0]["Endpoints"]["docker"]["Host"].as_str() != Some(want.as_str()) {
                docker_cli(
                    run,
                    docker,
                    &["context", "update", PODBAG_CONTEXT, "--docker", &spec],
                )?;
            }
        }
    }
    let now = docker_cli(run, docker, &["context", "show"])?;
    if !now.is_empty() && now != PODBAG_CONTEXT {
        choice::update_json(ui_json, |v| v[PREVIOUS_KEY] = now.into())?;
    }
    docker_cli(run, docker, &["context", "use", PODBAG_CONTEXT]).map(drop)
}

fn switch_back_with(run: &Run, ui_json: &Path, docker: &Path) -> Result<String, String> {
    let to = previous_in(ui_json)
        .filter(|n| n != PODBAG_CONTEXT)
        .unwrap_or_else(|| "default".into());
    let to = match docker_cli(run, docker, &["context", "use", &to]) {
        Ok(_) => to,
        // The context is gone: the default one.
        Err(_) if to != "default" => {
            docker_cli(run, docker, &["context", "use", "default"])?;
            "default".to_string()
        }
        Err(e) => return Err(e),
    };
    if previous_in(ui_json).is_some() {
        choice::update_json(ui_json, |v| {
            if let Some(o) = v.as_object_mut() {
                o.remove(PREVIOUS_KEY);
            }
        })?;
    }
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::run::Ran;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    /// Context `name` pointing at `host` in a docker CLI config folder.
    fn context(config: &Path, name: &str, host: &str) {
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
    }

    fn current(config: &Path, name: &str) {
        std::fs::create_dir_all(config).unwrap();
        std::fs::write(
            config.join("config.json"),
            serde_json::json!({ "currentContext": name }).to_string(),
        )
        .unwrap();
    }

    const PODBAG_SOCK: &str = "/home/me/.podbag/docker.sock";

    fn find(env: &[(&str, &str)], home: &Path) -> DockerTarget {
        DockerTarget::find(&env_of(env), Some(home), Path::new(PODBAG_SOCK))
    }

    #[test]
    fn docker_host_wins_and_contexts_do_not_apply() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".docker");
        context(&config, "colima", "unix:///c.sock");
        current(&config, "colima");
        let t = find(
            &[
                ("DOCKER_HOST", "unix:///home/me/.podbag/docker.sock"),
                ("DOCKER_CONTEXT", "colima"),
            ],
            home.path(),
        );
        assert_eq!(
            t,
            DockerTarget {
                context: None,
                host: "unix:///home/me/.podbag/docker.sock".into(),
                name: "podbag".into(),
                is_podbag: true,
                from_env: true,
            }
        );
        let t = find(&[("DOCKER_HOST", "tcp://10.0.0.5:2375")], home.path());
        assert_eq!(t.name, "tcp://10.0.0.5:2375");
        assert!(!t.is_podbag && t.from_env);
    }

    #[test]
    fn the_current_context_and_its_host() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".docker");
        let desktop = format!("unix://{}/.docker/run/docker.sock", home.path().display());
        context(&config, "desktop-linux", &desktop);
        context(&config, "podbag", &format!("unix://{PODBAG_SOCK}"));
        let orb = format!("unix://{}/.orbstack/run/docker.sock", home.path().display());
        context(&config, "orbstack", &orb);
        current(&config, "desktop-linux");
        let t = find(&[], home.path());
        assert_eq!(t.context.as_deref(), Some("desktop-linux"));
        assert_eq!(t.host, desktop);
        assert_eq!(t.name, "Docker Desktop");
        assert!(!t.is_podbag && !t.from_env);
        // DOCKER_CONTEXT names another one.
        let t = find(&[("DOCKER_CONTEXT", "podbag")], home.path());
        assert_eq!(t.context.as_deref(), Some("podbag"));
        assert!(t.is_podbag);
        assert_eq!(t.name, "podbag");
        assert_eq!(
            find(&[("DOCKER_CONTEXT", "orbstack")], home.path()).name,
            "OrbStack"
        );
        // DOCKER_CONFIG moves the config folder.
        let other = tempfile::tempdir().unwrap();
        let rd = format!("unix://{}/.rd/docker.sock", home.path().display());
        context(other.path(), "rancher-desktop", &rd);
        current(other.path(), "rancher-desktop");
        let cfg = other.path().display().to_string();
        let t = find(&[("DOCKER_CONFIG", &cfg)], home.path());
        assert_eq!(t.name, "Rancher Desktop");
    }

    #[test]
    fn default_means_the_default_socket() {
        let home = tempfile::tempdir().unwrap();
        // No config at all.
        let t = find(&[], home.path());
        assert_eq!(t.context.as_deref(), Some("default"));
        assert_eq!(t.host, default_host());
        assert!(!t.from_env);
        current(&home.path().join(".docker"), "default");
        assert_eq!(find(&[], home.path()).host, default_host());
    }

    #[test]
    fn names_of_other_engines() {
        let home = Path::new("/home/me");
        let name = |host: &str| find(&[("DOCKER_HOST", host)], home).name;
        assert_eq!(
            name("unix:///home/me/.colima/default/docker.sock"),
            "Colima"
        );
        assert_eq!(
            name("unix:///home/me/.docker/run/docker.sock"),
            "Docker Desktop"
        );
        assert_eq!(name("npipe:////./pipe/docker_engine"), "Docker Desktop");
        assert_eq!(name("unix:///run/user/1000/podman/podman.sock"), "Podman");
        assert_eq!(name("ssh://me@box"), "ssh://me@box");
    }

    /// A docker CLI with contexts, current one, and the commands it got.
    #[derive(Default)]
    struct FakeDocker {
        contexts: BTreeMap<String, String>,
        current: String,
        calls: Vec<String>,
    }

    fn fake_docker(contexts: &[(&str, &str)], current: &str) -> Arc<Mutex<FakeDocker>> {
        Arc::new(Mutex::new(FakeDocker {
            contexts: contexts
                .iter()
                .map(|(n, h)| (n.to_string(), h.to_string()))
                .collect(),
            current: current.into(),
            calls: Vec::new(),
        }))
    }

    fn runner(
        state: Arc<Mutex<FakeDocker>>,
    ) -> impl Fn(&Path, &[&str], Option<&[u8]>) -> std::io::Result<Ran> {
        move |program, args, _| {
            assert_eq!(program, Path::new("/bin/docker"));
            let mut s = state.lock().unwrap();
            s.calls.push(args.join(" "));
            let ok = |out: String| {
                Ok(Ran {
                    ok: true,
                    stdout: out.into_bytes(),
                    stderr: Vec::new(),
                })
            };
            let fail = |err: &str| {
                Ok(Ran {
                    ok: false,
                    stdout: Vec::new(),
                    stderr: err.as_bytes().to_vec(),
                })
            };
            match args {
                ["context", "show"] => ok(format!("{}\n", s.current)),
                ["context", "inspect", name] => match s.contexts.get(*name) {
                    Some(host) => ok(serde_json::json!([{"Name": name,
                        "Endpoints": {"docker": {"Host": host}}}])
                    .to_string()),
                    None => fail("context not found"),
                },
                ["context", "create" | "update", name, "--docker", spec] => {
                    let host = spec.strip_prefix("host=").unwrap().to_string();
                    s.contexts.insert(name.to_string(), host);
                    ok(String::new())
                }
                ["context", "use", name]
                    if s.contexts.contains_key(*name) || *name == "default" =>
                {
                    s.current = name.to_string();
                    ok(String::new())
                }
                ["context", "use", _] => fail("context not found"),
                _ => fail("unknown command"),
            }
        }
    }

    #[test]
    fn use_podbag_makes_the_context_once_and_remembers_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let ui = dir.path().join("ui.json");
        std::fs::write(&ui, r#"{"engine": "docker"}"#).unwrap();
        let state = fake_docker(&[("desktop-linux", "unix:///d.sock")], "desktop-linux");
        let run = runner(state.clone());
        let docker = Path::new("/bin/docker");
        let sock = Path::new(PODBAG_SOCK);
        use_podbag_with(&run, &ui, docker, sock).unwrap();
        {
            let s = state.lock().unwrap();
            assert_eq!(s.current, "podbag");
            assert_eq!(s.contexts["podbag"], format!("unix://{PODBAG_SOCK}"));
            assert_eq!(s.contexts["desktop-linux"], "unix:///d.sock");
            assert!(s.calls.contains(&format!(
                "context create podbag --docker host=unix://{PODBAG_SOCK}"
            )));
        }
        assert_eq!(previous_in(&ui).as_deref(), Some("desktop-linux"));
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&ui).unwrap()).unwrap();
        assert_eq!(v["engine"], "docker");
        // Again: no new context, and podbag is not remembered as the one before.
        state.lock().unwrap().calls.clear();
        use_podbag_with(&run, &ui, docker, sock).unwrap();
        let s = state.lock().unwrap();
        assert!(
            !s.calls
                .iter()
                .any(|c| c.contains("create") || c.contains("update")),
            "{:?}",
            s.calls
        );
        assert_eq!(previous_in(&ui).as_deref(), Some("desktop-linux"));
    }

    #[test]
    fn a_podbag_context_with_another_host_is_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let ui = dir.path().join("ui.json");
        let state = fake_docker(&[("podbag", "unix:///old.sock")], "default");
        use_podbag_with(
            &runner(state.clone()),
            &ui,
            Path::new("/bin/docker"),
            Path::new(PODBAG_SOCK),
        )
        .unwrap();
        let s = state.lock().unwrap();
        assert_eq!(s.contexts["podbag"], format!("unix://{PODBAG_SOCK}"));
        assert!(
            s.calls
                .iter()
                .any(|c| c.starts_with("context update podbag"))
        );
        assert_eq!(previous_in(&ui).as_deref(), Some("default"));
    }

    #[test]
    fn switch_back_uses_the_remembered_context() {
        let dir = tempfile::tempdir().unwrap();
        let ui = dir.path().join("ui.json");
        let state = fake_docker(&[("colima", "unix:///c.sock")], "colima");
        let run = runner(state.clone());
        let docker = Path::new("/bin/docker");
        use_podbag_with(&run, &ui, docker, Path::new(PODBAG_SOCK)).unwrap();
        assert_eq!(switch_back_with(&run, &ui, docker).unwrap(), "colima");
        assert_eq!(state.lock().unwrap().current, "colima");
        assert_eq!(previous_in(&ui), None);
        // Nothing remembered: default.
        assert_eq!(switch_back_with(&run, &ui, docker).unwrap(), "default");
        // A remembered context that is gone: default too.
        choice::update_json(&ui, |v| v[PREVIOUS_KEY] = "gone".into()).unwrap();
        assert_eq!(switch_back_with(&run, &ui, docker).unwrap(), "default");
        assert_eq!(state.lock().unwrap().current, "default");
    }
}
