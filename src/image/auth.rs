//! Registry credentials, kept where Docker keeps them: `config.json` in `$DOCKER_CONFIG` or
//! `~/.docker`, with its `auths`, `credsStore` and `credHelpers`. A credential helper is the
//! program `docker-credential-<name>`, with `get`, `store` or `erase` and JSON on stdin and
//! stdout. `docker login` and `podbag login` share what they store.

use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The key Docker uses for Docker Hub in `auths` and credential helpers.
pub const DOCKER_HUB_KEY: &str = "https://index.docker.io/v1/";

/// Credentials for a front end's command (the macOS VM, WSL): base64 JSON of a map from
/// registry to [`AuthConfig`]. Read before the config file; never written to disk.
pub const AUTH_ENV: &str = "PODBAG_REGISTRY_AUTH";

/// What a registry login holds, in the JSON form of Docker's `X-Registry-Auth` and
/// `POST /auth` (names compare without case there, so both spellings are read).
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct AuthConfig {
    #[serde(default, alias = "Username", skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(default, alias = "Password", skip_serializing_if = "String::is_empty")]
    pub password: String,
    /// base64 of `username:password`, as config files keep it.
    #[serde(default, alias = "Auth", skip_serializing_if = "String::is_empty")]
    pub auth: String,
    /// An OAuth refresh token a registry gave at login, used instead of the password.
    #[serde(
        default,
        alias = "IdentityToken",
        alias = "identityToken",
        skip_serializing_if = "String::is_empty"
    )]
    pub identitytoken: String,
    /// A bearer token to send as it is.
    #[serde(
        default,
        alias = "RegistryToken",
        alias = "registryToken",
        skip_serializing_if = "String::is_empty"
    )]
    pub registrytoken: String,
    #[serde(
        default,
        alias = "ServerAddress",
        alias = "serverAddress",
        skip_serializing_if = "String::is_empty"
    )]
    pub serveraddress: String,
}

impl std::fmt::Debug for AuthConfig {
    // Never the secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("username", &self.username)
            .field("serveraddress", &self.serveraddress)
            .field("password", &(!self.password.is_empty()))
            .field("identitytoken", &(!self.identitytoken.is_empty()))
            .finish()
    }
}

impl AuthConfig {
    pub fn with_password(username: &str, password: &str) -> AuthConfig {
        AuthConfig {
            username: username.to_string(),
            password: password.to_string(),
            ..AuthConfig::default()
        }
    }

    /// `auth` taken apart into user name and password, when those are not given.
    pub fn normalized(mut self) -> AuthConfig {
        let pair = (self.username.is_empty() && self.password.is_empty())
            .then(|| crate::base64::decode(&self.auth))
            .flatten()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| {
                s.split_once(':')
                    .map(|(u, p)| (u.to_string(), p.to_string()))
            });
        if let Some((u, p)) = pair {
            self.username = u;
            self.password = p;
        }
        self.auth.clear();
        self
    }

    /// Nothing to log in with.
    pub fn is_empty(&self) -> bool {
        self.username.is_empty()
            && self.password.is_empty()
            && self.auth.is_empty()
            && self.identitytoken.is_empty()
            && self.registrytoken.is_empty()
    }

    /// Read `X-Registry-Auth`: base64 (URL-safe or standard) JSON. Empty or `{}` means none.
    pub fn from_header(value: &str) -> Result<Option<AuthConfig>> {
        let bad = |why: &str| {
            Error::new(
                ErrorKind::InvalidInput,
                "cannot read the X-Registry-Auth header",
                why,
                "send base64-encoded JSON, as the docker CLI does",
            )
        };
        if value.trim().is_empty() {
            return Ok(None);
        }
        let json = crate::base64::decode(value).ok_or_else(|| bad("it is not base64"))?;
        let auth: AuthConfig =
            serde_json::from_slice(&json).map_err(|_| bad("it is not the expected JSON"))?;
        let auth = auth.normalized();
        Ok((!auth.is_empty()).then_some(auth))
    }

    /// The value of an `X-Registry-Auth` header for these credentials.
    pub fn to_header(&self) -> String {
        crate::base64::encode(&serde_json::to_vec(self).unwrap_or_default())
    }
}

/// The registry a server address names: `docker.io` for Docker Hub, else the host
/// (`localhost:5000`, `ghcr.io`). Takes `https://…/v1/`, plain hosts and Hub's aliases.
pub fn registry_of(server: &str) -> String {
    let s = server.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split('/').next().unwrap_or("");
    match host {
        ""
        | "docker.io"
        | "index.docker.io"
        | "registry-1.docker.io"
        | "registry.hub.docker.com" => "docker.io".to_string(),
        h => h.to_ascii_lowercase(),
    }
}

/// The key a registry's credentials have in Docker's config and helpers.
pub fn key_of(registry: &str) -> String {
    match registry_of(registry).as_str() {
        "docker.io" => DOCKER_HUB_KEY.to_string(),
        other => other.to_string(),
    }
}

/// Docker's config directory: `$DOCKER_CONFIG`, else `~/.docker`.
pub fn default_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DOCKER_CONFIG").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".docker"))
}

/// Where a login was stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// In this credential helper (its name, without `docker-credential-`).
    Helper(String),
    /// base64 in this config file.
    File(PathBuf),
}

/// Docker's config directory and its credential helpers.
#[derive(Debug, Clone)]
pub struct Credentials {
    dir: PathBuf,
    /// Where the `docker-credential-*` programs are; the PATH when None.
    helper_dir: Option<PathBuf>,
}

impl Credentials {
    /// The user's: see [`default_dir`].
    pub fn from_env() -> Option<Credentials> {
        default_dir().map(Credentials::at)
    }

    pub fn at(dir: impl Into<PathBuf>) -> Credentials {
        Credentials {
            dir: dir.into(),
            helper_dir: None,
        }
    }

    /// Run credential helpers from `dir` instead of the PATH.
    pub fn with_helper_dir(mut self, dir: impl Into<PathBuf>) -> Credentials {
        self.helper_dir = Some(dir.into());
        self
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    fn read(&self) -> Result<serde_json::Map<String, serde_json::Value>> {
        let path = self.config_path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
            Err(e) => {
                return Err(Error::io(
                    format!("cannot read {}", path.display()),
                    &e,
                    "check its permissions",
                ));
            }
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Default::default());
        }
        match serde_json::from_slice(&bytes) {
            Ok(serde_json::Value::Object(map)) => Ok(map),
            Ok(_) | Err(_) => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("cannot read {}", path.display()),
                "it is not a JSON object",
                "fix the file, or move it away",
            )),
        }
    }

    /// The helper for this key: its own (`credHelpers`), else the default (`credsStore`).
    fn helper_for(
        config: &serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<String> {
        let registry = registry_of(key);
        let own = config
            .get("credHelpers")
            .and_then(|h| h.as_object())
            .and_then(|h| h.iter().find(|(k, _)| registry_of(k) == registry))
            .and_then(|(_, v)| v.as_str());
        own.or_else(|| config.get("credsStore").and_then(|v| v.as_str()))
            .filter(|h| !h.is_empty())
            .map(str::to_string)
    }

    /// The `auths` entry for a registry: under its key, or any key naming the same host
    /// (`https://ghcr.io`, `http://localhost:5000/v2/`).
    fn auths_entry<'a>(
        config: &'a serde_json::Map<String, serde_json::Value>,
        registry: &str,
    ) -> Option<(&'a String, &'a serde_json::Value)> {
        let auths = config.get("auths")?.as_object()?;
        let key = key_of(registry);
        auths.get_key_value(&key).or_else(|| {
            auths
                .iter()
                .find(|(k, _)| registry_of(k) == registry_of(registry))
        })
    }

    /// The stored login for a registry (`docker.io`, `localhost:5000`), if any. A helper
    /// runs when the registry has one of its own, or when it is the default and the
    /// registry has an entry in `auths` (Docker writes one for each login it stores in a
    /// helper): a registry never logged in to costs no helper run.
    pub fn get(&self, registry: &str) -> Result<Option<AuthConfig>> {
        let config = self.read()?;
        let registry = registry_of(registry);
        let entry = Credentials::auths_entry(&config, &registry);
        let own_helper = config
            .get("credHelpers")
            .and_then(|h| h.as_object())
            .is_some_and(|h| h.keys().any(|k| registry_of(k) == registry));
        if let Some(helper) = Credentials::helper_for(&config, &registry)
            && (own_helper || entry.is_some())
        {
            let key = entry.map_or_else(|| key_of(&registry), |(k, _)| k.clone());
            return self.helper_get(&helper, &key);
        }
        let Some((_, value)) = entry else {
            return Ok(None);
        };
        let auth: AuthConfig = serde_json::from_value(value.clone()).unwrap_or_default();
        let auth = auth.normalized();
        Ok((!auth.is_empty()).then_some(AuthConfig {
            serveraddress: key_of(&registry),
            ..auth
        }))
    }

    /// Store a login: in the helper when one is set, else base64 in config.json (mode
    /// 0600). Other settings in the file stay as they are.
    pub fn store(&self, registry: &str, auth: &AuthConfig) -> Result<Stored> {
        let mut config = self.read()?;
        let registry = registry_of(registry);
        let key = key_of(&registry);
        let mut entry = serde_json::Map::new();
        let stored = match Credentials::helper_for(&config, &key) {
            Some(helper) => {
                let (user, secret) = if auth.identitytoken.is_empty() {
                    (auth.username.as_str(), auth.password.as_str())
                } else {
                    ("<token>", auth.identitytoken.as_str())
                };
                let input =
                    serde_json::json!({"ServerURL": key, "Username": user, "Secret": secret});
                self.run_helper(&helper, "store", input.to_string().as_bytes())?;
                Stored::Helper(helper)
            }
            None => {
                let pair = if auth.identitytoken.is_empty() {
                    format!("{}:{}", auth.username, auth.password)
                } else {
                    format!("{}:", auth.username)
                };
                entry.insert("auth".into(), crate::base64::encode(pair.as_bytes()).into());
                if !auth.identitytoken.is_empty() {
                    entry.insert("identitytoken".into(), auth.identitytoken.clone().into());
                }
                Stored::File(self.config_path())
            }
        };
        let auths = config
            .entry("auths")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if !auths.is_object() {
            *auths = serde_json::Value::Object(Default::default());
        }
        if let Some(map) = auths.as_object_mut() {
            // One entry per registry: an older one under another spelling goes.
            map.retain(|k, _| registry_of(k) != registry);
            map.insert(key, serde_json::Value::Object(entry));
        }
        self.write(&config)?;
        Ok(stored)
    }

    /// Remove a login. Returns whether there was one.
    pub fn erase(&self, registry: &str) -> Result<bool> {
        let mut config = self.read()?;
        let registry = registry_of(registry);
        let key = key_of(&registry);
        let mut found = false;
        if let Some(map) = config.get_mut("auths").and_then(|a| a.as_object_mut()) {
            let before = map.len();
            map.retain(|k, _| registry_of(k) != registry);
            found = map.len() != before;
        }
        if let Some(helper) = Credentials::helper_for(&config, &key) {
            // A helper that has nothing for the key says so; that is not an error here.
            if self.run_helper(&helper, "erase", key.as_bytes()).is_ok() {
                found = true;
            }
        }
        if found {
            self.write(&config)?;
        }
        Ok(found)
    }

    fn helper_get(&self, helper: &str, key: &str) -> Result<Option<AuthConfig>> {
        #[derive(Deserialize)]
        struct Reply {
            #[serde(rename = "Username", default)]
            username: String,
            #[serde(rename = "Secret", default)]
            secret: String,
        }
        let out = match self.run_helper(helper, "get", key.as_bytes()) {
            Ok(out) => out,
            Err(e) if e.why.contains("credentials not found") => return Ok(None),
            Err(e) => return Err(e),
        };
        let reply: Reply = serde_json::from_slice(&out).map_err(|_| {
            Error::new(
                ErrorKind::Runtime,
                format!("cannot read the answer of docker-credential-{helper}"),
                "it is not the expected JSON",
                "check the credential helper",
            )
        })?;
        if reply.secret.is_empty() {
            return Ok(None);
        }
        let mut auth = AuthConfig {
            serveraddress: key.to_string(),
            ..AuthConfig::default()
        };
        if reply.username == "<token>" {
            auth.identitytoken = reply.secret;
        } else {
            auth.username = reply.username;
            auth.password = reply.secret;
        }
        Ok(Some(auth))
    }

    /// Run `docker-credential-<helper> <action>` with `input`; its output.
    fn run_helper(&self, helper: &str, action: &str, input: &[u8]) -> Result<Vec<u8>> {
        let program = format!("docker-credential-{helper}");
        let path = match &self.helper_dir {
            Some(d) => d.join(&program),
            None => PathBuf::from(&program),
        };
        let fail = |why: String| {
            Error::new(
                ErrorKind::MissingRequirement,
                format!("cannot {action} credentials with {program}"),
                why,
                format!(
                    "install {program}, or remove credsStore or credHelpers from {}",
                    self.config_path().display()
                ),
            )
        };
        let mut command = Command::new(&path);
        command
            .arg(action)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = spawn_retrying(&mut command).map_err(|e| fail(e.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input);
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(mut s) = child.stdout.take() {
            let _ = s.read_to_end(&mut out);
        }
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_end(&mut err);
        }
        let status = child.wait().map_err(|e| fail(e.to_string()))?;
        if !status.success() {
            // Helpers write their error to stdout ("credentials not found in native keychain").
            let said = String::from_utf8_lossy(if out.is_empty() { &err } else { &out })
                .trim()
                .to_string();
            return Err(Error::new(
                ErrorKind::Runtime,
                format!("{program} {action} failed"),
                if said.is_empty() {
                    status.to_string()
                } else {
                    said
                },
                "check the credential helper",
            ));
        }
        Ok(out)
    }

    /// Write config.json whole: to a new file (0600), renamed over the old one.
    fn write(&self, config: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
        let path = self.config_path();
        let fail = |e: std::io::Error| {
            Error::io(
                format!("cannot write {}", path.display()),
                &e,
                "check the permissions of the directory",
            )
        };
        create_private_dir(&self.dir).map_err(fail)?;
        let mut bytes = serde_json::to_vec_pretty(config).unwrap_or_default();
        bytes.push(b'\n');
        let tmp = self
            .dir
            .join(format!(".config.json.{}.tmp", std::process::id()));
        let written = (|| {
            let mut f = private_file(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written.map_err(fail)
    }
}

/// A login for `registry`, as a command finds it: a front end's ([`AUTH_ENV`]) first,
/// then the user's Docker config. Problems reading them mean no login: a public image
/// still pulls, and a private one fails with a hint to log in.
pub fn lookup(registry: &str) -> Option<AuthConfig> {
    let registry = registry_of(registry);
    if let Some(found) = from_env_var(&registry) {
        return found;
    }
    Credentials::from_env()?.get(&registry).ok().flatten()
}

static FRONT_END: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Read [`AUTH_ENV`] and remove it from the environment, so that no process this one
/// starts (a container's helper, the keeper, a container) inherits the login.
///
/// # Safety
///
/// No other thread may read or write the environment meanwhile: call it first thing in
/// `main`.
pub unsafe fn take_front_end_logins() {
    FRONT_END.get_or_init(|| {
        let value = std::env::var(AUTH_ENV).ok();
        if value.is_some() {
            // SAFETY: the caller promises that no other thread uses the environment.
            unsafe { std::env::remove_var(AUTH_ENV) };
        }
        value
    });
}

/// The front end's credentials for a registry: None when the variable is not set (then the
/// config is read), Some(None) when it is set without this registry.
fn from_env_var(registry: &str) -> Option<Option<AuthConfig>> {
    let value = FRONT_END
        .get_or_init(|| std::env::var(AUTH_ENV).ok())
        .clone()
        .filter(|v| !v.is_empty())?;
    Some(
        decode_map(&value)
            .and_then(|map| {
                map.into_iter()
                    .find(|(k, _)| registry_of(k) == registry)
                    .map(|(_, a)| a.normalized())
            })
            .filter(|a| !a.is_empty()),
    )
}

/// The value of [`AUTH_ENV`] for these logins.
pub fn encode_map(logins: &[(String, AuthConfig)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = logins
        .iter()
        .filter_map(|(k, a)| Some((registry_of(k), serde_json::to_value(a).ok()?)))
        .collect();
    crate::base64::encode(&serde_json::to_vec(&map).unwrap_or_default())
}

fn decode_map(value: &str) -> Option<Vec<(String, AuthConfig)>> {
    let json = crate::base64::decode(value)?;
    let map: std::collections::BTreeMap<String, AuthConfig> = serde_json::from_slice(&json).ok()?;
    Some(map.into_iter().collect())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Start `command`. A helper that was just written can be busy for a moment ("Text file
/// busy"): a child that another thread forked still holds it open until that child execs.
/// Try again a few times (0.6 s in all) before giving up.
fn spawn_retrying(command: &mut Command) -> std::io::Result<std::process::Child> {
    #[cfg(unix)]
    {
        let mut wait = std::time::Duration::from_millis(10);
        for _ in 0..6 {
            match command.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(wait);
                    wait *= 2;
                }
                other => return other,
            }
        }
    }
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, json: &str) {
        std::fs::write(dir.join("config.json"), json).unwrap();
    }

    #[test]
    fn registries_and_keys_like_docker() {
        for hub in [
            "",
            "docker.io",
            "index.docker.io",
            "https://index.docker.io/v1/",
            "registry-1.docker.io",
        ] {
            assert_eq!(registry_of(hub), "docker.io", "{hub:?}");
            assert_eq!(key_of(hub), DOCKER_HUB_KEY);
        }
        assert_eq!(registry_of("https://ghcr.io"), "ghcr.io");
        assert_eq!(registry_of("http://localhost:5000/v2/"), "localhost:5000");
        assert_eq!(key_of("localhost:5000"), "localhost:5000");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_helper_still_open_for_writing_starts_once_it_is_closed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("helper");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Still open for writing, as in a child that another test forked and that has not
        // reached exec yet: exec fails with "Text file busy" meanwhile.
        let held = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(held);
        });
        let status = spawn_retrying(&mut Command::new(&path))
            .unwrap()
            .wait()
            .unwrap();
        closer.join().unwrap();
        assert!(status.success());
    }

    #[test]
    fn reads_auths_in_every_form() {
        let tmp = tempfile::tempdir().unwrap();
        // The `auth` values are base64 of "user:password", made here from test words.
        let auth = |pair: &str| crate::base64::encode(pair.as_bytes());
        let config = format!(
            r#"{{"auths": {{
                "https://index.docker.io/v1/": {{"auth": "{}"}},
                "https://ghcr.io": {{"auth": "{}"}},
                "localhost:5000": {{"auth": "{}", "identitytoken": "refresh-1"}},
                "empty.example": {{}}
            }}, "detachKeys": "ctrl-x"}}"#,
            auth("hub:x1"),
            auth("gh:x2"),
            auth("u:"),
        );
        write_config(tmp.path(), &config);
        let c = Credentials::at(tmp.path());
        let hub = c.get("docker.io").unwrap().unwrap();
        assert_eq!(
            (hub.username.as_str(), hub.password.as_str()),
            ("hub", "x1")
        );
        assert_eq!(hub.serveraddress, DOCKER_HUB_KEY);
        let gh = c.get("ghcr.io").unwrap().unwrap();
        assert_eq!((gh.username.as_str(), gh.password.as_str()), ("gh", "x2"));
        let local = c.get("localhost:5000").unwrap().unwrap();
        assert_eq!(local.username, "u");
        assert_eq!(local.identitytoken, "refresh-1");
        assert!(c.get("empty.example").unwrap().is_none());
        assert!(c.get("quay.io").unwrap().is_none());
        // No file, or an empty one: no logins.
        let none = tempfile::tempdir().unwrap();
        assert!(
            Credentials::at(none.path())
                .get("docker.io")
                .unwrap()
                .is_none()
        );
        write_config(none.path(), "");
        assert!(
            Credentials::at(none.path())
                .get("docker.io")
                .unwrap()
                .is_none()
        );
        write_config(none.path(), "[1]");
        assert_eq!(
            Credentials::at(none.path())
                .get("docker.io")
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn stores_in_the_file_privately_and_keeps_other_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("docker");
        let c = Credentials::at(&dir);
        let stored = c
            .store("localhost:5000", &AuthConfig::with_password("u", "p:w"))
            .unwrap();
        assert_eq!(stored, Stored::File(dir.join("config.json")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("config.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let got = c.get("localhost:5000").unwrap().unwrap();
        assert_eq!((got.username.as_str(), got.password.as_str()), ("u", "p:w"));

        write_config(
            &dir,
            r#"{"auths": {"http://localhost:5000": {"auth": "eDp5"}}, "psFormat": "table"}"#,
        );
        c.store("localhost:5000", &AuthConfig::with_password("u2", "p2"))
            .unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(raw["psFormat"], "table");
        assert_eq!(raw["auths"].as_object().unwrap().len(), 1, "{raw}");
        assert_eq!(raw["auths"]["localhost:5000"]["auth"], "dTI6cDI=");

        assert!(c.erase("localhost:5000").unwrap());
        assert!(c.get("localhost:5000").unwrap().is_none());
        assert!(!c.erase("localhost:5000").unwrap());
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(raw["psFormat"], "table");
    }

    /// A credential helper that keeps its logins in files next to it.
    #[cfg(unix)]
    fn fake_helper(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let script = r#"#!/bin/sh
dir=$(dirname "$0")/store
mkdir -p "$dir"
case "$1" in
  store) input=$(cat); key=$(printf '%s' "$input" | sed 's/.*"ServerURL":"\([^"]*\)".*/\1/' | tr '/:' '__'); printf '%s' "$input" > "$dir/$key" ;;
  get) key=$(cat | tr '/:' '__'); if [ -f "$dir/$key" ]; then cat "$dir/$key"; else echo "credentials not found in native keychain"; exit 1; fi ;;
  erase) key=$(cat | tr '/:' '__'); if [ -f "$dir/$key" ]; then rm "$dir/$key"; else echo "credentials not found in native keychain"; exit 1; fi ;;
  *) exit 2 ;;
esac
"#;
        let path = dir.join("docker-credential-test");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_creds_store_helper_holds_the_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        fake_helper(&bin);
        let dir = tmp.path().join("docker");
        std::fs::create_dir(&dir).unwrap();
        write_config(&dir, r#"{"credsStore": "test"}"#);
        let c = Credentials::at(&dir).with_helper_dir(&bin);
        assert!(c.get("docker.io").unwrap().is_none());
        let stored = c
            .store("docker.io", &AuthConfig::with_password("hub-user", "x3"))
            .unwrap();
        assert_eq!(stored, Stored::Helper("test".into()));
        // The file names the login, without the secret.
        let text = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!text.contains("\"x3") && !text.contains("aHVi"), "{text}");
        assert!(text.contains(DOCKER_HUB_KEY), "{text}");
        let got = c.get("docker.io").unwrap().unwrap();
        assert_eq!(
            (got.username.as_str(), got.password.as_str()),
            ("hub-user", "x3")
        );
        // An identity token goes in as the user `<token>`, and comes back as one.
        c.store(
            "ghcr.io",
            &AuthConfig {
                username: "me".into(),
                identitytoken: "refresh-9".into(),
                ..AuthConfig::default()
            },
        )
        .unwrap();
        let got = c.get("ghcr.io").unwrap().unwrap();
        assert_eq!(got.identitytoken, "refresh-9");
        assert!(got.password.is_empty());
        assert!(c.erase("docker.io").unwrap());
        assert!(c.get("docker.io").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_registry_helper_comes_before_the_store_and_a_missing_one_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        fake_helper(&bin);
        let dir = tmp.path().join("docker");
        std::fs::create_dir(&dir).unwrap();
        write_config(
            &dir,
            r#"{"credsStore": "missing", "credHelpers": {"registry.example": "test"}}"#,
        );
        let c = Credentials::at(&dir).with_helper_dir(&bin);
        c.store("registry.example", &AuthConfig::with_password("ru", "rp"))
            .unwrap();
        assert_eq!(c.get("registry.example").unwrap().unwrap().password, "rp");
        // A registry never logged in to runs no helper.
        assert!(c.get("other.example").unwrap().is_none());
        let e = c
            .store("other.example", &AuthConfig::with_password("a", "b"))
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::MissingRequirement);
        assert!(e.what.contains("docker-credential-missing"), "{e:?}");
    }

    #[test]
    fn the_header_and_the_front_end_variable() {
        let auth = AuthConfig {
            username: "u".into(),
            password: "p".into(),
            serveraddress: "localhost:5000".into(),
            ..AuthConfig::default()
        };
        let header = auth.to_header();
        assert_eq!(AuthConfig::from_header(&header).unwrap().unwrap(), auth);
        // The docker CLI's form: URL-safe, capitalized names or `auth`.
        let cli = crate::base64::encode(
            br#"{"Username":"u","Password":"p","ServerAddress":"localhost:5000"}"#,
        )
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string();
        assert_eq!(AuthConfig::from_header(&cli).unwrap().unwrap(), auth);
        let with_auth =
            crate::base64::encode(br#"{"auth":"dTpw","serveraddress":"localhost:5000"}"#);
        assert_eq!(AuthConfig::from_header(&with_auth).unwrap().unwrap(), auth);
        assert!(AuthConfig::from_header("").unwrap().is_none());
        assert!(
            AuthConfig::from_header(&crate::base64::encode(b"{}"))
                .unwrap()
                .is_none()
        );
        assert!(AuthConfig::from_header("%%%").is_err());
        // Never the secret in debug output.
        assert!(!format!("{auth:?}").contains("\"p\""));

        let value = encode_map(&[("https://index.docker.io/v1/".into(), auth.clone())]);
        let map = decode_map(&value).unwrap();
        assert_eq!(map, vec![("docker.io".to_string(), auth)]);
    }
}
