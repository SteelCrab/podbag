//! Where images are pulled from (Docker Hub, GitHub, …), who is logged in there, and
//! the login the docker CLI sends with a pull. Logins are read as the docker CLI reads
//! them: `auths` in its config.json, and its credential helpers. Passwords are never
//! kept or shown; only [`registry_auth`] holds one, for the local engine.

use super::choice::{docker_config_dir, find_tool};
use super::registry_presets::PRESETS;
pub use super::registry_presets::RegistryPreset;
use super::run::{self, Run};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// A place to pull images from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum Location {
    /// Docker Hub, where short names go.
    DockerHub,
    /// GitHub Container Registry, `ghcr.io`.
    GitHub,
    /// Amazon ECR: `<account>.dkr.ecr.<region>.amazonaws.com`, given by the user (empty
    /// until then).
    Ecr(String),
    /// Quay.io.
    Quay,
    /// Google Artifact Registry: `<region>-docker.pkg.dev`, given by the user (empty
    /// until then).
    Gar(String),
    /// GitLab, `registry.gitlab.com`.
    GitLab,
    /// Azure Container Registry, at its login server.
    Acr(String),
    /// A fixed or user-configured endpoint in the service catalog.
    Preset(&'static RegistryPreset, String),
    /// Another registry, by host (`registry.example.com:5000`).
    Custom(String),
}

impl Location {
    /// Every kind, in the order to show them. User-supplied hosts start empty.
    pub fn all() -> Vec<Location> {
        let mut all = vec![
            Location::DockerHub,
            Location::GitHub,
            Location::Ecr(String::new()),
            Location::Quay,
            Location::Gar(String::new()),
            Location::GitLab,
            Location::Acr(String::new()),
        ];
        all.extend(
            PRESETS
                .iter()
                .map(|preset| Location::Preset(preset, String::new())),
        );
        all.push(Location::Custom(String::new()));
        all
    }

    /// The user must give a host ([`Location::with_host`]): ECR, GAR, ACR, Custom.
    pub fn needs_host(&self) -> bool {
        if let Location::Preset(preset, _) = self {
            return preset.needs_host();
        }
        matches!(
            self,
            Location::Ecr(_) | Location::Gar(_) | Location::Acr(_) | Location::Custom(_)
        )
    }

    /// The same kind at `host` (for ECR, GAR, ACR, Custom; the others keep their own).
    pub fn with_host(&self, host: &str) -> Location {
        let host = normalize(host);
        match self {
            Location::Ecr(_) => Location::Ecr(host),
            Location::Gar(_) => Location::Gar(host),
            Location::Acr(_) => Location::Acr(host),
            Location::Preset(preset, _) if preset.needs_host() => Location::Preset(preset, host),
            Location::Custom(_) => Location::Custom(host),
            other => other.clone(),
        }
    }

    /// The registry host: `docker.io`, `ghcr.io`, the given host, …
    pub fn host(&self) -> String {
        match self {
            Location::DockerHub => "docker.io".into(),
            Location::GitHub => "ghcr.io".into(),
            Location::Preset(preset, host) => {
                if preset.needs_host() {
                    host.clone()
                } else {
                    preset.example.into()
                }
            }
            Location::Quay => "quay.io".into(),
            Location::GitLab => "registry.gitlab.com".into(),
            Location::Ecr(h) | Location::Gar(h) | Location::Acr(h) | Location::Custom(h) => {
                h.clone()
            }
        }
    }

    /// The name to show: `Docker Hub`, `GitHub`, `Amazon ECR`, …; Custom shows its host.
    pub fn label(&self) -> String {
        match self {
            Location::DockerHub => "Docker Hub".into(),
            Location::GitHub => "GitHub".into(),
            Location::Ecr(_) => "Amazon ECR".into(),
            Location::Quay => "Quay.io".into(),
            Location::Gar(_) => "Google Artifact Registry".into(),
            Location::GitLab => "GitLab".into(),
            Location::Acr(_) => "Azure Container Registry".into(),
            Location::Preset(preset, _) => preset.label.into(),
            Location::Custom(h) if h.is_empty() => "Other registry".into(),
            Location::Custom(h) => h.clone(),
        }
    }

    /// Example to show in the login server field.
    pub fn host_example(&self) -> &'static str {
        match self {
            Location::Ecr(_) => "123456789012.dkr.ecr.us-east-1.amazonaws.com",
            Location::Gar(_) => "us-docker.pkg.dev",
            Location::Acr(_) => "contoso.azurecr.io",
            Location::Preset(preset, _) => preset.example,
            _ => "registry.example.com",
        }
    }

    /// The reference to pull for `short`, an image name here: `me/app:1` at GitHub is
    /// `ghcr.io/me/app:1`; at Docker Hub, `nginx` stays `nginx`. A reference that
    /// already names a registry stays as it is.
    pub fn qualify(&self, short: &str) -> String {
        let short = short.trim();
        let host = self.host();
        if *self == Location::DockerHub || host.is_empty() || names_registry(short) {
            return short.to_string();
        }
        format!("{host}/{short}")
    }

    /// How to log in where a password is not enough: cloud registries give a command.
    pub fn login_hint(&self) -> Option<String> {
        match self {
            Location::Ecr(h) => {
                let host = if h.is_empty() {
                    "<account>.dkr.ecr.<region>.amazonaws.com"
                } else {
                    h
                };
                let region = host.split('.').nth(3).unwrap_or("<region>");
                Some(format!(
                    "aws ecr get-login-password --region {region} | docker login --username AWS --password-stdin {host}"
                ))
            }
            Location::Gar(h) => {
                let host = if h.is_empty() {
                    "<region>-docker.pkg.dev"
                } else {
                    h
                };
                Some(format!("gcloud auth configure-docker {host}"))
            }
            Location::Acr(host) => {
                let name = acr_resource_name(host).unwrap_or("<registry-name>");
                Some(format!("az acr login --name {name}"))
            }
            _ => None,
        }
    }

    /// The location of an image reference (or a registry host).
    pub fn of(reference: &str) -> Location {
        let reference = reference.trim();
        let bare_host = !reference.contains('/') && !reference.contains('@') && {
            match reference.split_once(':') {
                // `localhost:5000`, not `nginx:1.25`.
                Some((_, port)) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
                None => reference.contains('.') || reference == "localhost",
            }
        };
        let host = if bare_host || reference.contains("://") {
            normalize(reference)
        } else {
            registry_host(reference)
        };
        match host.as_str() {
            "docker.io" => Location::DockerHub,
            "ghcr.io" => Location::GitHub,
            "quay.io" => Location::Quay,
            "registry.gitlab.com" => Location::GitLab,
            h if h.contains(".dkr.ecr.")
                && (h.ends_with(".amazonaws.com") || h.ends_with(".amazonaws.com.cn")) =>
            {
                Location::Ecr(host)
            }
            h if h.ends_with("-docker.pkg.dev") => Location::Gar(host),
            h if acr_resource_name(h).is_some() => Location::Acr(host),
            _ => match PRESETS.iter().find(|preset| preset.recognizes(&host)) {
                Some(preset) => Location::Preset(preset, String::new()).with_host(&host),
                None => Location::Custom(host),
            },
        }
    }

    /// The account logged in here, from [`logins`]. A cloud registry with no host yet: any
    /// login of that kind.
    pub fn account<'a>(&self, logins: &'a [(String, String)]) -> Option<&'a str> {
        let host = self.host();
        logins
            .iter()
            .find(|(server, _)| {
                if host.is_empty() {
                    Location::of(server).with_host("") == *self
                } else {
                    normalize(server) == host
                }
            })
            .map(|(_, user)| user.as_str())
    }
}

/// Azure CLI takes the resource name. A protected login server adds a DNS hash.
fn acr_resource_name(host: &str) -> Option<&str> {
    let (label, suffix) = host.split_once('.')?;
    if !matches!(suffix, "azurecr.io" | "azurecr.cn" | "azurecr.us") {
        return None;
    }
    let (name, hash) = label.split_once('-').unwrap_or((label, ""));
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    if label != name && (hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_alphanumeric())) {
        return None;
    }
    Some(name)
}

/// The first part of `reference` is a registry host: it has a `.` or `:`, or is
/// `localhost`, and more follows.
fn names_registry(reference: &str) -> bool {
    match reference.split_once('/') {
        Some((first, _)) => first.contains('.') || first.contains(':') || first == "localhost",
        None => false,
    }
}

/// The registry host of an image reference, as the docker CLI sees it: the first part
/// when it has a `.` or `:` or is `localhost`, else `docker.io`.
pub fn registry_host(reference: &str) -> String {
    let reference = reference.trim();
    if names_registry(reference) {
        normalize(reference.split('/').next().unwrap_or(""))
    } else {
        "docker.io".into()
    }
}

/// A server as written in config.json (`https://index.docker.io/v1/`, `ghcr.io`) as a
/// plain host; Docker Hub's names are all `docker.io`.
fn normalize(server: &str) -> String {
    let s = server.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split('/').next().unwrap_or("").to_ascii_lowercase();
    match host.as_str() {
        "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => {
            "docker.io".into()
        }
        _ => host,
    }
}

/// The docker CLI's config folder for this user (`DOCKER_CONFIG`, else `~/.docker`).
pub fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    docker_config_dir(&env, home.as_deref())
}

/// Who is logged in where, as (registry host, account), sorted by host. From `auths`
/// in config.json (only the user name is decoded) and the credential helpers' `list`
/// (which gives no secrets).
pub fn logins(config_dir: &Path) -> Vec<(String, String)> {
    logins_with(&run::system, config_dir)
}

/// The account logged in at `location`, if any.
pub fn login_state(location: &Location) -> Option<String> {
    let all = logins(&config_dir()?);
    location.account(&all).map(str::to_string)
}

/// The `X-Registry-Auth` value the docker CLI sends to pull `reference`: base64url of
/// `{"username", "password", "serveraddress"}`. None when there is no login. It is a
/// secret: never log it, and send it only to the local engine.
pub fn registry_auth(reference: &str) -> Option<String> {
    registry_auth_with(&run::system, &config_dir()?, reference)
}

/// Log in to `server` (empty: Docker Hub). With `podbag login` when this podbag has it,
/// else with the docker CLI. The password goes only through stdin.
pub fn login(
    podbag: &Path,
    docker: Option<&Path>,
    server: &str,
    user: &str,
    password: &str,
) -> Result<(), String> {
    login_with(&run::system, podbag, docker, server, user, password)
}

/// The docker CLI's config.json, or an empty object.
fn read_config(config_dir: &Path) -> Value {
    std::fs::read(config_dir.join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

/// `docker-credential-<name>`, where the docker CLI would find it.
fn helper_program(name: &str) -> PathBuf {
    let program = format!("docker-credential-{name}");
    find_tool(&program).unwrap_or_else(|| PathBuf::from(program))
}

/// A helper's `list`: server → account. No secrets.
fn helper_list(run: &Run, name: &str) -> Vec<(String, String)> {
    let Ok(ran) = run(&helper_program(name), &["list"], None) else {
        return Vec::new();
    };
    let v: Value = serde_json::from_slice(&ran.stdout).unwrap_or_default();
    match (ran.ok, v.as_object()) {
        (true, Some(map)) => map
            .iter()
            .filter_map(|(s, u)| Some((s.clone(), u.as_str()?.to_string())))
            .collect(),
        _ => Vec::new(),
    }
}

/// A helper's `get` for `server`: (account, secret).
fn helper_get(run: &Run, name: &str, server: &str) -> Option<(String, String)> {
    let ran = run(&helper_program(name), &["get"], Some(server.as_bytes())).ok()?;
    if !ran.ok {
        return None;
    }
    let v: Value = serde_json::from_slice(&ran.stdout).ok()?;
    let secret = v["Secret"].as_str().filter(|s| !s.is_empty())?;
    Some((
        v["Username"].as_str().unwrap_or("").to_string(),
        secret.to_string(),
    ))
}

/// The `auth` of an `auths` entry, decoded: (account, password).
fn decode_auth(entry: &Value) -> Option<(String, String)> {
    let raw = b64_decode(entry["auth"].as_str()?)?;
    let text = String::from_utf8(raw).ok()?;
    let (user, password) = text.split_once(':')?;
    (!user.is_empty()).then(|| (user.to_string(), password.to_string()))
}

fn logins_with(run: &Run, config_dir: &Path) -> Vec<(String, String)> {
    let config = read_config(config_dir);
    let mut found: std::collections::BTreeMap<String, String> = Default::default();
    if let Some(auths) = config["auths"].as_object() {
        for (server, entry) in auths {
            // Only the user name is kept.
            if let Some((user, _)) = decode_auth(entry) {
                found.insert(normalize(server), user);
            }
        }
    }
    if let Some(store) = config["credsStore"].as_str().filter(|s| !s.is_empty()) {
        for (server, user) in helper_list(run, store) {
            found.insert(normalize(&server), user);
        }
    }
    if let Some(helpers) = config["credHelpers"].as_object() {
        for (server, helper) in helpers {
            let Some(helper) = helper.as_str() else {
                continue;
            };
            let host = normalize(server);
            if let Some((_, user)) = helper_list(run, helper)
                .into_iter()
                .find(|(s, _)| normalize(s) == host)
            {
                found.insert(host, user);
            }
        }
    }
    found.into_iter().filter(|(s, _)| !s.is_empty()).collect()
}

fn registry_auth_with(run: &Run, config_dir: &Path, reference: &str) -> Option<String> {
    let config = read_config(config_dir);
    let host = registry_host(reference);
    let keys_of = |section: &str| -> Vec<String> {
        config[section]
            .as_object()
            .map(|m| m.keys().filter(|k| normalize(k) == host).cloned().collect())
            .unwrap_or_default()
    };
    // The server as the config writes it; Docker Hub's name in the docker CLI.
    let mut servers = keys_of("credHelpers");
    servers.extend(keys_of("auths"));
    servers.push(if host == "docker.io" {
        "https://index.docker.io/v1/".into()
    } else {
        host.clone()
    });
    let mut seen = std::collections::HashSet::new();
    servers.retain(|s| seen.insert(s.clone()));
    let helper_of = |server: &str| {
        config["credHelpers"][server]
            .as_str()
            .or(config["credsStore"].as_str())
            .filter(|h| !h.is_empty())
            .map(str::to_string)
    };
    let (server, user, secret) = servers.iter().find_map(|server| {
        let found = helper_of(server)
            .and_then(|h| helper_get(run, &h, server))
            .or_else(|| decode_auth(&config["auths"][server.as_str()]));
        found.map(|(user, secret)| (server.clone(), user, secret))
    })?;
    // An identity token (`<token>` as the user) is sent as one.
    let body = if user == "<token>" || user.is_empty() {
        serde_json::json!({"identitytoken": secret, "serveraddress": server})
    } else {
        serde_json::json!({"username": user, "password": secret, "serveraddress": server})
    };
    Some(b64(body.to_string().as_bytes(), true))
}

fn login_with(
    run: &Run,
    podbag: &Path,
    docker: Option<&Path>,
    server: &str,
    user: &str,
    password: &str,
) -> Result<(), String> {
    let podbag_has_login = run(podbag, &["login", "--help"], None).is_ok_and(|r| r.ok);
    let program = match (podbag_has_login, docker) {
        (true, _) => podbag,
        (false, Some(docker)) => docker,
        (false, None) => {
            return Err(
                "logging in needs the docker CLI (this podbag has no `podbag login`)".into(),
            );
        }
    };
    let mut args = vec!["login"];
    if !server.trim().is_empty() {
        args.push(server.trim());
    }
    args.extend(["--username", user, "--password-stdin"]);
    let what = format!("{} login", program.display());
    let ran = run(program, &args, Some(password.as_bytes())).map_err(|e| format!("{what}: {e}"))?;
    if ran.ok {
        Ok(())
    } else {
        Err(ran.error(&what))
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 with padding; `url`: the URL-safe letters (`-` and `_`).
fn b64(data: &[u8], url: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                let c = B64[(n >> (18 - 6 * i) & 63) as usize];
                out.push(match (url, c) {
                    (true, b'+') => '-',
                    (true, b'/') => '_',
                    _ => c as char,
                });
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base64 in either alphabet, padded or not; None when it is not base64.
fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut n, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        n = n << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
            n &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::run::Ran;
    use std::sync::{Arc, Mutex};

    #[test]
    fn hosts_labels_and_all() {
        let all = Location::all();
        assert_eq!(all.len(), 8 + PRESETS.len());
        assert_eq!(all[0], Location::DockerHub);
        assert_eq!(Location::DockerHub.host(), "docker.io");
        assert_eq!(Location::GitHub.host(), "ghcr.io");
        assert_eq!(Location::Quay.host(), "quay.io");
        assert_eq!(Location::GitLab.host(), "registry.gitlab.com");
        assert_eq!(Location::GitHub.label(), "GitHub");
        assert_eq!(Location::Ecr(String::new()).label(), "Amazon ECR");
        let ecr = Location::Ecr(String::new()).with_host("123.dkr.ecr.eu-west-1.amazonaws.com");
        assert_eq!(ecr.host(), "123.dkr.ecr.eu-west-1.amazonaws.com");
        assert!(ecr.needs_host() && !Location::GitHub.needs_host());
        assert_eq!(Location::GitHub.with_host("x.io"), Location::GitHub);
        assert_eq!(
            Location::Custom("r.example.com:5000".into()).label(),
            "r.example.com:5000"
        );
    }

    #[test]
    fn catalog_locations_qualify_and_recognize_their_servers() {
        for preset in PRESETS {
            let location = Location::Preset(preset, String::new()).with_host(preset.example);
            let reference = format!("{}/team/tool:1", preset.example);
            assert_eq!(location.qualify("team/tool:1"), reference);
            assert_eq!(
                location.qualify("elsewhere.example/team/tool:1"),
                "elsewhere.example/team/tool:1"
            );
            if preset.recognizes(preset.example) {
                assert_eq!(Location::of(&reference), location);
            }
            let unrelated = format!("{}.unrelated.example", preset.example);
            assert!(!preset.recognizes(&unrelated));
        }
        assert_eq!(
            Location::Custom("localhost:5000".into()).qualify("team/tool"),
            "localhost:5000/team/tool"
        );
    }

    #[test]
    fn alibaba_recognizes_personal_enterprise_and_regional_login_servers() {
        for host in [
            "crpi-example.cn-hangzhou.personal.cr.aliyuncs.com",
            "team-registry.cn-shanghai.cr.aliyuncs.com",
            "team-registry-vpc.cn-shanghai.cr.aliyuncs.com",
            "registry.cn-hangzhou.aliyuncs.com",
            "registry-vpc.cn-hangzhou.aliyuncs.com",
            "registry-intl.cn-hangzhou.aliyuncs.com",
            "registry-intl-vpc.cn-hangzhou.aliyuncs.com",
        ] {
            let reference = format!("{host}/team/api:2");
            let loc = Location::of(&reference);
            assert!(matches!(&loc, Location::Preset(preset, _) if preset.id == "alibaba"));
            assert_eq!(loc.qualify("team/api:2"), reference);
        }
        let other = "registry-intl.cn-hangzhou.aliyuncs.com.example.org";
        assert_eq!(Location::of(other), Location::Custom(other.into()));
    }

    #[test]
    fn short_names_are_qualified() {
        assert_eq!(Location::DockerHub.qualify("nginx"), "nginx");
        assert_eq!(Location::DockerHub.qualify("me/app:1"), "me/app:1");
        assert_eq!(Location::GitHub.qualify("me/app:1"), "ghcr.io/me/app:1");
        assert_eq!(Location::Quay.qualify("org/tool"), "quay.io/org/tool");
        assert_eq!(
            Location::GitLab.qualify("g/p/img"),
            "registry.gitlab.com/g/p/img"
        );
        let ecr = Location::Ecr("1.dkr.ecr.us-east-1.amazonaws.com".into());
        assert_eq!(ecr.qualify("app"), "1.dkr.ecr.us-east-1.amazonaws.com/app");
        let gar = Location::Gar("europe-docker.pkg.dev".into());
        assert_eq!(
            gar.qualify("proj/repo/img:2"),
            "europe-docker.pkg.dev/proj/repo/img:2"
        );
        assert_eq!(
            Location::Custom("localhost:5000".into()).qualify("a"),
            "localhost:5000/a"
        );
        // A reference with a registry stays.
        assert_eq!(Location::GitHub.qualify("quay.io/x/y"), "quay.io/x/y");
        assert_eq!(Location::GitHub.qualify("localhost/x"), "localhost/x");
        assert_eq!(
            Location::Quay.qualify(" docker.io/library/nginx "),
            "docker.io/library/nginx"
        );
        // No host given yet: as it is.
        assert_eq!(Location::Ecr(String::new()).qualify("app"), "app");
    }

    #[test]
    fn references_have_a_location() {
        assert_eq!(registry_host("nginx"), "docker.io");
        assert_eq!(registry_host("me/app:1"), "docker.io");
        assert_eq!(registry_host("index.docker.io/library/nginx"), "docker.io");
        assert_eq!(registry_host("localhost:5000/a"), "localhost:5000");
        assert_eq!(Location::of("ghcr.io/me/app"), Location::GitHub);
        assert_eq!(Location::of("alpine"), Location::DockerHub);
        assert_eq!(
            Location::of("1.dkr.ecr.ap-south-1.amazonaws.com/app"),
            Location::Ecr("1.dkr.ecr.ap-south-1.amazonaws.com".into())
        );
        assert_eq!(
            Location::of("us-docker.pkg.dev/p/r/i"),
            Location::Gar("us-docker.pkg.dev".into())
        );
        assert_eq!(
            Location::of("r.example.com/x"),
            Location::Custom("r.example.com".into())
        );
    }

    #[test]
    fn hints_for_cloud_registries() {
        let ecr = Location::Ecr("1.dkr.ecr.eu-west-1.amazonaws.com".into());
        assert_eq!(
            ecr.login_hint().unwrap(),
            "aws ecr get-login-password --region eu-west-1 | docker login --username AWS --password-stdin 1.dkr.ecr.eu-west-1.amazonaws.com"
        );
        let gar = Location::Gar("asia-docker.pkg.dev".into());
        assert_eq!(
            gar.login_hint().unwrap(),
            "gcloud auth configure-docker asia-docker.pkg.dev"
        );
        assert!(
            Location::Ecr(String::new())
                .login_hint()
                .unwrap()
                .contains("<region>")
        );
        assert_eq!(Location::GitHub.login_hint(), None);
    }

    #[test]
    fn azure_uses_the_full_login_server_for_images_and_the_resource_name_for_login() {
        let blank = Location::Acr(String::new());
        assert!(blank.needs_host());
        assert_eq!(
            blank.login_hint().unwrap(),
            "az acr login --name <registry-name>"
        );
        for host in [
            "contoso.azurecr.io",
            "contoso-a1b2.azurecr.io",
            "contoso.azurecr.us",
            "contoso.azurecr.cn",
        ] {
            let acr = blank.with_host(host);
            let image = format!("{host}/team/app:1");
            assert_eq!(Location::of(&image), acr);
            assert_eq!(acr.qualify("team/app:1"), image);
            assert_eq!(acr.login_hint().unwrap(), "az acr login --name contoso");
        }
        for host in [
            "contoso.azurecr.io.example.org",
            "contoso.eastus.data.azurecr.io",
            "contoso-.azurecr.io",
        ] {
            assert_eq!(Location::of(host), Location::Custom(host.into()));
        }
    }

    #[test]
    fn base64_both_ways() {
        for text in ["", "a", "ab", "abc", "user:pw-1", "\u{ff}\u{fe}?>"] {
            assert_eq!(
                b64_decode(&b64(text.as_bytes(), false)).unwrap(),
                text.as_bytes()
            );
            assert_eq!(
                b64_decode(&b64(text.as_bytes(), true)).unwrap(),
                text.as_bytes()
            );
        }
        assert_eq!(b64(b"hi?>", false), "aGk/Pg==");
        assert_eq!(b64(b"hi?>", true), "aGk_Pg==");
        assert_eq!(b64_decode("not base64!"), None);
    }

    /// A fake value, made at run time.
    fn secret(n: u32) -> String {
        format!("pw-{n}")
    }

    fn config(dir: &Path, v: Value) {
        std::fs::write(dir.join("config.json"), v.to_string()).unwrap();
    }

    /// A runner for credential helpers: `docker-credential-<name>` knows `entries`
    /// (server, user, secret); it records what it was asked.
    fn helpers(
        entries: Vec<(&'static str, &'static str, String, String)>,
        asked: Arc<Mutex<Vec<String>>>,
    ) -> impl Fn(&Path, &[&str], Option<&[u8]>) -> std::io::Result<Ran> {
        move |program, args, stdin| {
            let prog = program.file_name().unwrap().to_string_lossy().to_string();
            let helper = prog.strip_prefix("docker-credential-").unwrap().to_string();
            asked
                .lock()
                .unwrap()
                .push(format!("{helper} {}", args.join(" ")));
            let mine = entries.iter().filter(|e| e.0 == helper);
            let ok = |v: Value| {
                Ok(Ran {
                    ok: true,
                    stdout: v.to_string().into_bytes(),
                    stderr: Vec::new(),
                })
            };
            match args {
                ["list"] => ok(mine
                    .map(|e| (e.1.to_string(), Value::from(e.2.clone())))
                    .collect()),
                ["get"] => {
                    let server = String::from_utf8(stdin.unwrap().to_vec()).unwrap();
                    match mine.into_iter().find(|e| e.1 == server) {
                        Some(e) => ok(
                            serde_json::json!({"ServerURL": server, "Username": e.2, "Secret": e.3}),
                        ),
                        None => Ok(Ran {
                            ok: false,
                            stdout: b"credentials not found in native keychain".to_vec(),
                            stderr: Vec::new(),
                        }),
                    }
                }
                _ => panic!("unexpected {args:?}"),
            }
        }
    }

    #[test]
    fn logins_from_auths_and_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let auth = b64(format!("alice:{}", secret(1)).as_bytes(), false);
        config(
            dir.path(),
            serde_json::json!({
                "auths": {
                    "https://index.docker.io/v1/": {},
                    "registry.example.com": {"auth": auth},
                    "ghcr.io": {}
                },
                "credsStore": "teststore",
                "credHelpers": {"quay.io": "testquay"}
            }),
        );
        let asked = Arc::default();
        let run = helpers(
            vec![
                (
                    "teststore",
                    "https://index.docker.io/v1/",
                    "bob".into(),
                    secret(2),
                ),
                ("teststore", "ghcr.io", "carol".into(), secret(3)),
                ("testquay", "quay.io", "dave".into(), secret(4)),
                ("testquay", "other.io", "erin".into(), secret(5)),
            ],
            asked,
        );
        let found = logins_with(&run, dir.path());
        assert_eq!(
            found,
            [
                ("docker.io".to_string(), "bob".to_string()),
                ("ghcr.io".into(), "carol".into()),
                ("quay.io".into(), "dave".into()),
                ("registry.example.com".into(), "alice".into()),
            ]
        );
        let text = format!("{found:?}");
        for n in 1..=5 {
            assert!(!text.contains(&secret(n)));
        }
        assert_eq!(Location::DockerHub.account(&found), Some("bob"));
        assert_eq!(Location::GitLab.account(&found), None);
        assert_eq!(
            Location::Custom("registry.example.com".into()).account(&found),
            Some("alice")
        );
        // No config: nobody.
        let empty = tempfile::tempdir().unwrap();
        assert!(logins_with(&run, empty.path()).is_empty());
    }

    fn decode_auth(value: &str) -> Value {
        serde_json::from_slice(&b64_decode(value).unwrap()).unwrap()
    }

    #[test]
    fn registry_auth_from_auths_and_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let auth = b64(format!("alice:{}", secret(1)).as_bytes(), false);
        config(
            dir.path(),
            serde_json::json!({
                "auths": {"registry.example.com": {"auth": auth}, "ghcr.io": {}},
                "credHelpers": {"ghcr.io": "testgh"}
            }),
        );
        let asked = Arc::default();
        let run = helpers(
            vec![("testgh", "ghcr.io", "carol".into(), secret(3))],
            Arc::clone(&asked),
        );
        let v = decode_auth(
            &registry_auth_with(&run, dir.path(), "registry.example.com/team/app:1").unwrap(),
        );
        assert_eq!(v["username"], "alice");
        assert_eq!(v["password"], secret(1));
        assert_eq!(v["serveraddress"], "registry.example.com");
        let header = registry_auth_with(&run, dir.path(), "ghcr.io/me/app").unwrap();
        assert!(!header.contains('+') && !header.contains('/'));
        let v = decode_auth(&header);
        assert_eq!(v["username"], "carol");
        assert_eq!(v["password"], secret(3));
        assert_eq!(v["serveraddress"], "ghcr.io");
        assert_eq!(asked.lock().unwrap().as_slice(), ["testgh get"]);
        assert_eq!(registry_auth_with(&run, dir.path(), "quay.io/x/y"), None);
        // Docker Hub through the store, under the docker CLI's server name.
        config(
            dir.path(),
            serde_json::json!({"auths": {"https://index.docker.io/v1/": {}}, "credsStore": "teststore"}),
        );
        let run = helpers(
            vec![(
                "teststore",
                "https://index.docker.io/v1/",
                "bob".into(),
                secret(2),
            )],
            Arc::default(),
        );
        let v = decode_auth(&registry_auth_with(&run, dir.path(), "nginx").unwrap());
        assert_eq!(v["username"], "bob");
        assert_eq!(v["serveraddress"], "https://index.docker.io/v1/");
    }

    #[test]
    fn login_uses_podbag_then_docker_with_the_password_on_stdin() {
        let pw = secret(9);
        let calls: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        let make = |podbag_has_login: bool| {
            let calls = Arc::clone(&calls);
            move |program: &Path, args: &[&str], stdin: Option<&[u8]>| {
                let prog = program.display().to_string();
                let input = String::from_utf8(stdin.unwrap_or_default().to_vec()).unwrap();
                calls
                    .lock()
                    .unwrap()
                    .push((prog.clone(), args.join(" "), input));
                let ok = prog != "/bin/podbag" || podbag_has_login;
                Ok(Ran {
                    ok,
                    stdout: Vec::new(),
                    stderr: b"unknown command".to_vec(),
                })
            }
        };
        let podbag = Path::new("/bin/podbag");
        let docker = Path::new("/bin/docker");
        login_with(&make(true), podbag, Some(docker), "ghcr.io", "me", &pw).unwrap();
        login_with(&make(false), podbag, Some(docker), "ghcr.io", "me", &pw).unwrap();
        let e = login_with(&make(false), podbag, None, "ghcr.io", "me", &pw).unwrap_err();
        assert!(e.contains("docker"), "{e}");
        let calls = calls.lock().unwrap();
        let line = |i: usize| {
            (
                calls[i].0.as_str(),
                calls[i].1.as_str(),
                calls[i].2.as_str(),
            )
        };
        assert_eq!(line(0), ("/bin/podbag", "login --help", ""));
        assert_eq!(
            line(1),
            (
                "/bin/podbag",
                "login ghcr.io --username me --password-stdin",
                pw.as_str()
            )
        );
        assert_eq!(line(2), ("/bin/podbag", "login --help", ""));
        assert_eq!(
            line(3),
            (
                "/bin/docker",
                "login ghcr.io --username me --password-stdin",
                pw.as_str()
            )
        );
        // The password is never an argument.
        assert!(calls.iter().all(|c| !c.1.contains(&pw)));
    }

    #[test]
    fn a_failed_login_gives_the_tool_s_message() {
        let run = |_: &Path, args: &[&str], _: Option<&[u8]>| {
            Ok(Ran {
                ok: args == ["login", "--help"],
                stdout: Vec::new(),
                stderr: b"unauthorized: incorrect username or password".to_vec(),
            })
        };
        let e = login_with(&run, Path::new("podbag"), None, "", "me", &secret(1)).unwrap_err();
        assert!(e.contains("unauthorized"), "{e}");
    }
}
