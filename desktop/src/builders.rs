//! Scope Buildx operations to the Desktop session's local Docker endpoint.

use podbag::client::choice::docker_config_dir;
use podbag::client::{Choice, DockerTarget, Endpoint, PODBAG_CONTEXT, Target};
use podbag::image::registry::sha256_hex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    Select,
    Start,
    Stop,
    Remove,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Availability {
    Ready,
    NonLocal,
    MissingDocker,
    MissingBuildx,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    availability: Availability,
    scope: Option<String>,
    context: Option<String>,
    outside_scope: bool,
    builders: Vec<Builder>,
}

#[derive(Serialize)]
pub struct Builder {
    name: String,
    driver: String,
    current: bool,
    managed: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    name: String,
    endpoint: String,
    status: String,
    buildkit: String,
    platforms: String,
    error: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Inspection {
    nodes: Vec<Node>,
    error: String,
}

struct Scope {
    docker: PathBuf,
    config: PathBuf,
    store: PathBuf,
    context: Option<String>,
    host: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StoredBuilder {
    name: String,
    driver: String,
    nodes: Vec<StoredNode>,
    #[serde(default)]
    dynamic: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StoredNode {
    endpoint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Current {
    key: String,
    name: String,
    global: bool,
}

pub fn list(target: &Target) -> Result<Catalog, String> {
    let Some(docker) = podbag::client::find_docker_cli() else {
        return Ok(unavailable(Availability::MissingDocker));
    };
    let Some(scope) = Scope::current(target, docker)? else {
        return Ok(unavailable(Availability::NonLocal));
    };
    if scope.run(&["version"]).is_err() {
        return Ok(unavailable(Availability::MissingBuildx));
    }
    let mut builders = scope.builders()?;
    let selected = scope.selected()?;
    let mut outside_scope = false;
    if let Some(selected) = selected {
        if let Some(builder) = builders.iter_mut().find(|item| item.name == selected) {
            builder.current = true;
        } else {
            outside_scope = true;
        }
    } else if let Some(builder) = builders
        .iter_mut()
        .find(|item| item.name == scope.default_name())
    {
        builder.current = true;
    }
    Ok(Catalog {
        availability: Availability::Ready,
        scope: Some(scope.token()),
        context: Some(scope.key().to_owned()),
        outside_scope,
        builders,
    })
}

pub fn inspect(target: &Target, token: &str, name: &str) -> Result<Inspection, String> {
    let scope = confirmed_scope(target, token)?;
    scope.builder(name)?;
    scope
        .run(&["inspect", "--", name])
        .map(|out| inspection(&out))
}

pub fn act(target: &Target, token: &str, name: &str, action: Action) -> Result<(), String> {
    confirmed_scope(target, token)?.act(name, action)
}

fn unavailable(availability: Availability) -> Catalog {
    Catalog {
        availability,
        scope: None,
        context: None,
        outside_scope: false,
        builders: Vec::new(),
    }
}

fn confirmed_scope(target: &Target, token: &str) -> Result<Scope, String> {
    let docker = podbag::client::find_docker_cli().ok_or("Docker CLI is unavailable.")?;
    let scope = Scope::current(target, docker)?
        .ok_or("Select a local Docker context to manage builders.")?;
    if scope.token() != token {
        return Err("The Docker context has changed. Reload the builder list.".into());
    }
    Ok(scope)
}

impl Scope {
    fn current(target: &Target, docker: PathBuf) -> Result<Option<Self>, String> {
        let env = |key: &str| std::env::var(key).ok().filter(|s| !s.is_empty());
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let config = docker_config_dir(&env, home.as_deref())
            .ok_or("Docker configuration is unavailable.")?;
        let config = absolute(config)?;
        let store = env("BUILDX_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| config.join("buildx"));
        let store = absolute(store)?;
        let named = |name: &str| {
            DockerTarget::find(
                &|key| match key {
                    "DOCKER_HOST" => None,
                    "DOCKER_CONTEXT" => Some(name.to_owned()),
                    _ => env(key),
                },
                home.as_deref(),
                Path::new(""),
            )
        };

        let (host, context) = match &target.endpoint {
            Endpoint::Socket { path, .. } if path.is_absolute() => {
                let host = target.endpoint.describe();
                let context = match &target.choice {
                    Choice::Context(name) => {
                        if !same_host(&named(name).host, &host) {
                            return Err(
                                "The Docker context has changed. Reopen the engine view.".into()
                            );
                        }
                        Some(name.clone())
                    }
                    Choice::Podbag | Choice::Docker => {
                        let current = DockerTarget::find(&env, home.as_deref(), path);
                        current
                            .context
                            .filter(|_| same_host(&current.host, &host))
                            .or_else(|| {
                                (target.choice == Choice::Podbag
                                    && same_host(&named(PODBAG_CONTEXT).host, &host))
                                .then(|| PODBAG_CONTEXT.to_owned())
                            })
                    }
                    _ => None,
                };
                (host, context)
            }
            Endpoint::Command(_) if cfg!(windows) => {
                let actual = match &target.choice {
                    Choice::Context(name) => named(name),
                    Choice::Docker => DockerTarget::find(&env, home.as_deref(), Path::new("")),
                    _ => return Ok(None),
                };
                if !local_host(&actual.host) {
                    return Ok(None);
                }
                // Docker-backed Windows sessions use the local named pipe.
                (actual.host, actual.context)
            }
            _ => return Ok(None),
        };
        if !local_host(&host) || context.as_deref().is_some_and(|name| !valid_name(name)) {
            return Ok(None);
        }
        Ok(Some(Self {
            docker,
            config,
            store,
            context,
            host,
        }))
    }

    fn default_name(&self) -> &str {
        self.context.as_deref().unwrap_or("default")
    }

    fn key(&self) -> &str {
        self.context
            .as_deref()
            .filter(|s| *s != "default")
            .unwrap_or(&self.host)
    }

    fn token(&self) -> String {
        sha256_hex(
            serde_json::json!([self.config, self.store, self.context, self.host])
                .to_string()
                .as_bytes(),
        )
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = Command::new(&self.docker);
        cmd.arg("--config")
            .arg(&self.config)
            .env("BUILDX_CONFIG", &self.store)
            .env_remove("BUILDX_BUILDER")
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_TLS_VERIFY")
            .env_remove("DOCKER_CERT_PATH")
            .stdin(Stdio::null());
        if let Some(name) = &self.context {
            cmd.arg(format!("--context={name}"));
        } else {
            cmd.arg(format!("--host={}", self.host));
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let output = cmd
            .arg("buildx")
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.trim().is_empty() {
                Err(format!("Docker Buildx exited with {}.", output.status))
            } else {
                Err(error.trim().to_owned())
            }
        }
    }

    fn owns_endpoint(&self, endpoint: &str) -> bool {
        self.context.as_deref() == Some(endpoint) || same_host(&self.host, endpoint)
    }

    fn builders(&self) -> Result<Vec<Builder>, String> {
        let instances = self.store.join("instances");
        let records = match fs::read_dir(&instances) {
            Ok(entries) => Some(entries),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("Cannot read Buildx builders: {e}")),
        };
        let mut builders = Vec::new();
        // A named Buildx instance shadows a Docker context of the same name.
        let mut shadows_default = false;
        for entry in records.into_iter().flatten() {
            let entry = entry.map_err(|e| e.to_string())?;
            let filename = entry.file_name();
            let Some(name) = filename.to_str().filter(|name| valid_name(name)) else {
                continue;
            };
            shadows_default |= name == self.default_name();
            if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
                continue;
            }
            let Some(group) = read_json::<StoredBuilder>(&entry.path())? else {
                continue;
            };
            if group.name != name
                || group.nodes.is_empty()
                || group.dynamic
                || !group
                    .nodes
                    .iter()
                    .all(|node| self.owns_endpoint(&node.endpoint))
            {
                continue;
            }
            builders.push(Builder {
                managed: group.driver == "docker-container",
                name: group.name,
                driver: group.driver,
                current: false,
            });
        }
        builders.sort_by(|a, b| a.name.cmp(&b.name));
        if !shadows_default {
            builders.insert(
                0,
                Builder {
                    name: self.default_name().into(),
                    driver: "docker".into(),
                    current: false,
                    managed: false,
                },
            );
        }
        Ok(builders)
    }

    fn builder(&self, name: &str) -> Result<Builder, String> {
        if !valid_name(name) {
            return Err("Invalid Buildx builder name.".into());
        }
        self.builders()?
            .into_iter()
            .find(|builder| builder.name == name)
            .ok_or_else(|| "The builder is outside this Docker context or no longer exists.".into())
    }

    fn act(&self, name: &str, action: Action) -> Result<(), String> {
        let builder = self.builder(name)?;
        if !matches!(action, Action::Select) && !builder.managed {
            return Err(
                "Only local docker-container builders can be started, stopped or removed.".into(),
            );
        }
        let args: &[&str] = match action {
            Action::Select => &["use", "--default", "--", name],
            Action::Start => &["inspect", "--bootstrap", "--", name],
            Action::Stop => &["stop", "--", name],
            Action::Remove => &["rm", "--", name],
        };
        self.run(args)?;
        Ok(())
    }

    fn selected(&self) -> Result<Option<String>, String> {
        if let Some(current) = read_json::<Current>(&self.store.join("current"))?
            && !current.name.is_empty()
            && (current.global || current.key == self.key())
            && valid_name(&current.name)
            && self.store.join("instances").join(&current.name).is_file()
        {
            return Ok(Some(current.name));
        }
        let key = sha256_hex(self.key().as_bytes());
        match fs::read_to_string(self.store.join("defaults").join(&key[..20])) {
            Ok(name) if valid_name(&name) && self.store.join("instances").join(&name).is_file() => {
                Ok(Some(name))
            }
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("Cannot read the Buildx selection: {e}")),
        }
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join(path))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("Cannot read {}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
    }
}

fn valid_name(name: &str) -> bool {
    let alphanumeric = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    name.bytes().next().is_some_and(alphanumeric)
        && name.bytes().all(|c| alphanumeric(c) || b"._-".contains(&c))
}

fn local_host(host: &str) -> bool {
    host.strip_prefix("unix://")
        .is_some_and(|p| Path::new(p).is_absolute())
        || cfg!(windows)
            && host
                .strip_prefix("npipe:////./pipe/")
                .is_some_and(|p| !p.is_empty())
}

fn same_host(a: &str, b: &str) -> bool {
    if !local_host(a) || !local_host(b) {
        return false;
    }
    a == b
        || Endpoint::from_docker_host(a)
            .zip(Endpoint::from_docker_host(b))
            .is_some_and(|(left, right)| left.same_target(&right))
}

// Buildx inspect has text output. Keep stable wire fields and omit driver options
// and file contents, which can hold private registry or proxy configuration.
fn inspection(output: &str) -> Inspection {
    let mut result = Inspection::default();
    let mut in_nodes = false;
    for line in output.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let line = line.trim_end();
        if line == "Nodes:" {
            in_nodes = true;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if !in_nodes {
            if key == "Error" {
                result.error = value.to_owned();
            }
            continue;
        }
        if key == "Name" {
            result.nodes.push(Node {
                name: value.into(),
                ..Node::default()
            });
            continue;
        }
        let Some(node) = result.nodes.last_mut() else {
            continue;
        };
        let target = match key {
            "Endpoint" => &mut node.endpoint,
            "Status" => &mut node.status,
            "BuildKit version" => &mut node.buildkit,
            "Platforms" => &mut node.platforms,
            "Error" => &mut node.error,
            _ => continue,
        };
        *target = value.to_owned();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope(root: &Path) -> Scope {
        Scope {
            docker: root.join("docker"),
            config: root.join("config"),
            store: root.join("builders"),
            context: Some("scope-a".into()),
            host: "unix:///tmp/scope-a.sock".into(),
        }
    }

    fn instance(scope: &Scope, name: &str, driver: &str, endpoints: &[&str]) {
        let dir = scope.store.join("instances");
        fs::create_dir_all(&dir).unwrap();
        let nodes: Vec<_> = endpoints
            .iter()
            .map(|endpoint| json!({ "Endpoint": endpoint }))
            .collect();
        fs::write(
            dir.join(name),
            json!({ "Name": name, "Driver": driver, "Nodes": nodes }).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn filters_all_builder_nodes_to_the_active_endpoint_and_scope() {
        let root = tempfile::tempdir().unwrap();
        let mut scope = scope(root.path());
        // A local endpoint in the platform's own form: /tmp/… is not an absolute path on
        // Windows, so a unix:// host there is not local.
        if cfg!(windows) {
            scope.host = "npipe:////./pipe/scope-a".into();
        }
        let host = scope.host.clone();
        instance(&scope, "local", "docker-container", &["scope-a", &host]);
        instance(&scope, "different", "docker-container", &["scope-b"]);
        instance(
            &scope,
            "mixed",
            "docker-container",
            &["scope-a", "ssh://outside"],
        );
        instance(&scope, "unbound", "docker-container", &["default"]);
        let builders = scope.builders().unwrap();
        assert_eq!(
            builders.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            ["scope-a", "local"]
        );
        assert!(!builders[0].managed && builders[1].managed);
        for name in ["../outside", "-x", "mixed", "different", "unbound"] {
            assert!(scope.builder(name).is_err(), "{name}");
        }
        instance(&scope, "scope-a", "docker-container", &["scope-b"]);
        assert!(
            scope.builder("scope-a").is_err(),
            "a stored name shadows the built-in"
        );
    }

    #[test]
    fn selection_uses_the_context_default_and_recognizes_existing_global_selection() {
        let root = tempfile::tempdir().unwrap();
        let scope = scope(root.path());
        instance(&scope, "local", "docker-container", &["scope-a"]);
        instance(&scope, "different", "docker-container", &["scope-b"]);
        let defaults = scope.store.join("defaults");
        fs::create_dir_all(&defaults).unwrap();
        fs::write(
            defaults.join(&sha256_hex(scope.key().as_bytes())[..20]),
            "local",
        )
        .unwrap();
        fs::write(
            scope.store.join("current"),
            r#"{"Key":"scope-b","Name":"different","Global":false}"#,
        )
        .unwrap();
        assert_eq!(scope.selected().unwrap().as_deref(), Some("local"));
        fs::write(
            scope.store.join("current"),
            r#"{"Key":"scope-b","Name":"different","Global":true}"#,
        )
        .unwrap();
        assert_eq!(scope.selected().unwrap().as_deref(), Some("different"));
        assert!(scope.builder("different").is_err());
    }

    #[test]
    fn uses_public_inspection_fields_and_ignores_driver_configuration() {
        let found = inspection(
            "Name: fixture\nDriver: docker-container\nNodes:\nName: worker0\nEndpoint: scope-a\nDriver Options: secret=unused\nStatus: running\nBuildKit version: v0.33.1\nPlatforms: linux/arm64\nDevices:\n  Name: local-device\n  Error: nested value\n",
        );
        assert_eq!(found.nodes.len(), 1);
        let node = &found.nodes[0];
        assert_eq!(node.name, "worker0");
        assert_eq!(node.status, "running");
        assert_eq!(node.endpoint, "scope-a");
        assert_eq!(node.buildkit, "v0.33.1");
        assert!(node.error.is_empty());
        assert!(!serde_json::to_string(&found).unwrap().contains("secret"));
    }

    #[cfg(unix)]
    #[test]
    fn invokes_only_explicit_scoped_commands_and_protects_the_builtin() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let scope = scope(root.path());
        instance(&scope, "local", "docker-container", &["scope-a"]);
        let output = root.path().join("invocations");
        fs::write(
            &scope.docker,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n",
                output.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&scope.docker, fs::Permissions::from_mode(0o700)).unwrap();
        for action in [Action::Start, Action::Stop, Action::Remove] {
            assert!(scope.act("scope-a", action).is_err());
        }
        assert!(scope.act("-invalid", Action::Remove).is_err());
        for action in [Action::Select, Action::Start, Action::Stop, Action::Remove] {
            scope.act("local", action).unwrap();
        }
        let args = fs::read_to_string(output).unwrap();
        let commands = args
            .lines()
            .map(|line| line.split_once(" buildx ").unwrap())
            .collect::<Vec<_>>();
        let prefix = format!("--config {} --context=scope-a", scope.config.display());
        assert!(commands.iter().all(|(scope, _)| *scope == prefix));
        assert_eq!(
            commands
                .into_iter()
                .map(|(_, command)| command)
                .collect::<Vec<_>>(),
            [
                "use --default -- local",
                "inspect --bootstrap -- local",
                "stop -- local",
                "rm -- local"
            ]
        );
    }
}
