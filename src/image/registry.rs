//! A minimal client for the OCI distribution API: manifests, configs and layer blobs, to
//! pull and to push.
//!
//! Every manifest and config is checked against its sha256 digest here. Layer blobs are
//! returned as streams; the caller hashes them while reading.
//!
//! A registry asks who is calling with a 401 and a `WWW-Authenticate` challenge: `Basic`
//! (send the user name and password), or `Bearer` (get a token for the scopes needed, like
//! `repository:app:pull,push`, from the token server it names; with a login, the token
//! server checks the password or the identity token). The client answers the challenge
//! once and keeps what it got for the next requests.

use super::auth::AuthConfig;
use super::manifest::{self, ConfigFile, Descriptor, Fetched};
use super::reference::{self, Reference};
use crate::{Error, ErrorKind, Result};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::time::Duration;

/// Manifests and configs are small; refuse anything larger.
const MAX_DOCUMENT: u64 = 4 * 1024 * 1024;

/// The client ID podbag gives token servers (Docker's is `docker`).
const CLIENT_ID: &str = "podbag";

/// The lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An image resolved for this platform.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The digest of the platform-specific manifest.
    pub manifest_digest: String,
    /// The digest of its config: the image ID Docker's classic store and buildx show.
    pub config_digest: String,
    pub config: ConfigFile,
    pub layers: Vec<Descriptor>,
    /// The image came in OCI's format (else Docker's schema 2).
    pub oci: bool,
}

pub struct Registry {
    agent: ureq::Agent,
    /// The login for the registry this client talks to.
    auth: Option<AuthConfig>,
    /// Bearer tokens per host and scopes.
    tokens: HashMap<String, String>,
    /// Hosts that asked for Basic authentication.
    basic: HashSet<String>,
    /// Ask token servers for a refresh token too (a login).
    offline: bool,
    /// A refresh token a token server gave.
    refresh_token: Option<String>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}

type Response = http::Response<ureq::Body>;

/// What an upload sends.
#[derive(Clone, Copy)]
pub enum Blob<'a> {
    File(&'a std::path::Path),
    Bytes(&'a [u8]),
}

impl Registry {
    pub fn new() -> Registry {
        Registry::with_auth(None)
    }

    /// A client that logs in with `auth` when the registry asks.
    pub fn with_auth(auth: Option<AuthConfig>) -> Registry {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(format!("podbag/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_resolve(Some(Duration::from_secs(30)))
            // A registry that stops sending for this long is treated as gone.
            .timeout_per_read(Some(Duration::from_secs(60)))
            .build()
            .into();
        Registry {
            agent,
            auth: auth.map(AuthConfig::normalized).filter(|a| !a.is_empty()),
            tokens: HashMap::new(),
            basic: HashSet::new(),
            offline: false,
            refresh_token: None,
        }
    }

    /// Check the login against `registry` (`docker.io`, `localhost:5000`): `GET /v2/`,
    /// answering its challenge. Returns the identity token the registry gave, if any, to
    /// keep instead of the password.
    pub fn login(&mut self, registry: &str) -> Result<Option<String>> {
        let registry = super::auth::registry_of(registry);
        let host = reference::api_host(&registry).to_string();
        let url = format!("{}://{host}/v2/", scheme(&registry));
        self.offline = true;
        let agent = self.agent.clone();
        let what = format!("registry {registry}");
        let response = self.send(&registry, &[], &what, &mut |auth| {
            let mut req = agent.get(&url);
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            req.call()
        })?;
        match response.status().as_u16() {
            200..=299 => Ok(self.refresh_token.clone()),
            other => Err(Error::new(
                ErrorKind::Runtime,
                format!("cannot log in to {registry}"),
                format!("{host} answered HTTP {other} at /v2/"),
                "check the registry's address",
            )),
        }
    }

    /// Fetch the manifest for this platform (through an index if needed) and its config.
    pub fn resolve(&mut self, r: &Reference) -> Result<Resolved> {
        let (content_type, body) =
            self.document(r, "manifests", r.reference(), Some(manifest::ACCEPT))?;
        if let Some(d) = &r.digest {
            verify(r, d, &body)?;
        }

        let (manifest_digest, config, layers) =
            match manifest::parse_manifest(content_type.as_deref(), &body)? {
                Fetched::Manifest { config, layers } => {
                    let digest = r
                        .digest
                        .clone()
                        .unwrap_or_else(|| format!("sha256:{}", sha256_hex(&body)));
                    (digest, config, layers)
                }
                Fetched::Index(list) => {
                    let (arch, variant) = manifest::host_platform();
                    let chosen = manifest::select_platform(&r.name(), &list, arch, variant)?;
                    let (ct, body) =
                        self.document(r, "manifests", &chosen.digest, Some(manifest::ACCEPT))?;
                    verify(r, &chosen.digest, &body)?;
                    match manifest::parse_manifest(ct.as_deref(), &body)? {
                        Fetched::Manifest { config, layers } => (chosen.digest, config, layers),
                        Fetched::Index(_) => {
                            return Err(Error::new(
                                ErrorKind::Runtime,
                                format!("cannot pull {}", r.name()),
                                "the index points to another index",
                                "use an image with a regular manifest",
                            ));
                        }
                    }
                }
            };

        let (_, config_body) = self.document(r, "blobs", &config.digest, None)?;
        verify(r, &config.digest, &config_body)?;
        let config_digest = config.digest.clone();
        let oci = config.media_type == manifest::OCI_CONFIG;
        let config = manifest::parse_config(&config_body)?;
        // A tag can point straight at a single-platform manifest: check its platform too.
        let (arch, _) = manifest::host_platform();
        let os_ok = config.os.is_empty() || config.os == "linux";
        let arch_ok = config.architecture.is_empty() || config.architecture == arch;
        if !os_ok || !arch_ok {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("no linux/{arch} image for {}", r.name()),
                format!("the image provides: {}/{}", config.os, config.architecture),
                format!("use an image built for linux/{arch}"),
            ));
        }
        if config.diff_ids.len() != layers.len() {
            return Err(Error::new(
                ErrorKind::Runtime,
                format!("cannot pull {}", r.name()),
                format!(
                    "the manifest lists {} layers but the config lists {}",
                    layers.len(),
                    config.diff_ids.len()
                ),
                "the image is broken; ask its publisher to rebuild it",
            ));
        }
        Ok(Resolved {
            manifest_digest,
            config_digest,
            config,
            layers,
            oci,
        })
    }

    /// Open a blob as a stream. The caller must check its digest.
    pub fn open_blob(&mut self, r: &Reference, d: &Descriptor) -> Result<Box<dyn Read + Send>> {
        let response = self.get(r, &url(r, "blobs", &d.digest), None)?;
        check_status(r, response.status().as_u16(), "blobs", &d.digest)?;
        Ok(Box::new(response.into_body().into_reader()))
    }

    /// GET a manifest or config. Returns its content type and body.
    fn document(
        &mut self,
        r: &Reference,
        kind: &str,
        reference: &str,
        accept: Option<&str>,
    ) -> Result<(Option<String>, Vec<u8>)> {
        let response = self.get(r, &url(r, kind, reference), accept)?;
        check_status(r, response.status().as_u16(), kind, reference)?;
        let content_type = header(&response, "content-type");
        let body = read_small(response).map_err(|e| unreachable_registry(r.api_host(), e))?;
        Ok((content_type, body))
    }

    fn get(&mut self, r: &Reference, url: &str, accept: Option<&str>) -> Result<Response> {
        let agent = self.agent.clone();
        let scopes = [pull_scope(&r.repository)];
        self.send(&r.registry, &scopes, &r.name(), &mut |auth| {
            let mut req = agent.get(url);
            if let Some(a) = accept {
                req = req.header("Accept", a);
            }
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            req.call()
        })
    }

    /// Whether the repository has this blob already (`HEAD`).
    pub fn blob_exists(&mut self, r: &Reference, digest: &str) -> Result<bool> {
        let agent = self.agent.clone();
        let target = url(r, "blobs", digest);
        let response = self.send(&r.registry, &push_scopes(r, None), &r.name(), &mut |auth| {
            let mut req = agent.head(&target);
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            req.call()
        })?;
        match response.status().as_u16() {
            200..=299 => Ok(true),
            404 => Ok(false),
            other => Err(push_failed(
                r,
                other,
                &format!("checking blob {digest}"),
                "",
            )),
        }
    }

    /// Mount a blob from another repository of the same registry, without sending it.
    /// False when the registry did not mount it (it lacks the blob there, or does not
    /// mount): then it must be uploaded.
    pub fn mount_blob(&mut self, r: &Reference, digest: &str, from: &str) -> Result<bool> {
        let agent = self.agent.clone();
        let base = format!("{}://{}", scheme(&r.registry), r.api_host());
        let target = format!(
            "{base}/v2/{}/blobs/uploads/?mount={}&from={}",
            r.repository,
            encode(digest),
            encode(from)
        );
        let scopes = push_scopes(r, Some(from));
        let response = self.send(&r.registry, &scopes, &r.name(), &mut |auth| {
            let mut req = agent.post(&target);
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            req.send_empty()
        })?;
        match response.status().as_u16() {
            201 => Ok(true),
            202 => {
                // An upload began instead: cancel it.
                if let Some(location) = header(&response, "location") {
                    let session = absolute(&base, &location);
                    let _ = self.send(&r.registry, &scopes, &r.name(), &mut |auth| {
                        let mut req = agent.delete(&session);
                        if let Some(a) = auth {
                            req = req.header("Authorization", a);
                        }
                        req.call()
                    });
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Upload a blob in one request (`POST`, then `PUT` with all of it).
    pub fn upload_blob(&mut self, r: &Reference, digest: &str, blob: Blob<'_>) -> Result<()> {
        let agent = self.agent.clone();
        let base = format!("{}://{}", scheme(&r.registry), r.api_host());
        let start = format!("{base}/v2/{}/blobs/uploads/", r.repository);
        let scopes = push_scopes(r, None);
        let response = self.send(&r.registry, &scopes, &r.name(), &mut |auth| {
            let mut req = agent.post(&start);
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            req.send_empty()
        })?;
        let status = response.status().as_u16();
        let location = header(&response, "location");
        let (202, Some(location)) = (status, location) else {
            let said = registry_said(response);
            return Err(push_failed(r, status, "starting an upload", &said));
        };
        let session = absolute(&base, &location);
        let sep = if session.contains('?') { '&' } else { '?' };
        let target = format!("{session}{sep}digest={}", encode(digest));
        let response = self.send(&r.registry, &scopes, &r.name(), &mut |auth| {
            let mut req = agent
                .put(&target)
                .header("Content-Type", "application/octet-stream");
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            match blob {
                // Opened for each try: a retry sends it from the start.
                Blob::File(path) => req.send(std::fs::File::open(path).map_err(ureq::Error::Io)?),
                Blob::Bytes(data) => req.send(data),
            }
        })?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            other => {
                let said = registry_said(response);
                Err(push_failed(r, other, &format!("uploading {digest}"), &said))
            }
        }
    }

    /// PUT a manifest under the reference's tag. Returns its digest.
    pub fn put_manifest(&mut self, r: &Reference, media_type: &str, body: &[u8]) -> Result<String> {
        let agent = self.agent.clone();
        let target = url(r, "manifests", r.reference());
        let scopes = push_scopes(r, None);
        let response = self.send(&r.registry, &scopes, &r.name(), &mut |auth| {
            let mut req = agent.put(&target).header("Content-Type", media_type);
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            req.send(body)
        })?;
        match response.status().as_u16() {
            200..=299 => Ok(format!("sha256:{}", sha256_hex(body))),
            other => {
                let said = registry_said(response);
                Err(push_failed(r, other, "sending the manifest", &said))
            }
        }
    }

    /// Send a request (`send` makes it, with the Authorization value to use), answering a
    /// 401 challenge once. `scopes` are the token scopes it needs; `what` names it in errors.
    fn send(
        &mut self,
        registry: &str,
        scopes: &[String],
        what: &str,
        send: &mut dyn FnMut(Option<&str>) -> std::result::Result<Response, ureq::Error>,
    ) -> Result<Response> {
        let host = reference::api_host(registry).to_string();
        let key = format!("{host} {}", scopes.join(" "));
        let known = self.known_header(&host, &key);
        let response = send(known.as_deref()).map_err(|e| unreachable_registry(&host, e))?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        let challenge = header(&response, "www-authenticate").unwrap_or_default();
        let scheme_name = challenge
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let answer = match scheme_name.as_str() {
            "basic" => {
                let Some(basic) = self.basic_header() else {
                    return Err(login_required(what, registry));
                };
                self.basic.insert(host.clone());
                basic
            }
            "bearer" => {
                let token = self.fetch_token(registry, scopes, &challenge, what)?;
                self.tokens.insert(key, token.clone());
                format!("Bearer {token}")
            }
            _ => return Err(login_required(what, registry)),
        };
        let response = send(Some(&answer)).map_err(|e| unreachable_registry(&host, e))?;
        if response.status().as_u16() == 401 {
            return Err(match (&self.auth, scheme_name.as_str()) {
                // The user name and password themselves were refused.
                (Some(_), "basic") => login_failed(registry, 401),
                // The token server took the login, and the registry still refuses.
                (Some(a), _) => access_denied(what, registry, &a.username),
                (None, _) => login_required(what, registry),
            });
        }
        Ok(response)
    }

    /// The Authorization value this client already has for a request.
    fn known_header(&self, host: &str, key: &str) -> Option<String> {
        if let Some(t) = self
            .auth
            .as_ref()
            .map(|a| &a.registrytoken)
            .filter(|t| !t.is_empty())
        {
            return Some(format!("Bearer {t}"));
        }
        if self.basic.contains(host) {
            return self.basic_header();
        }
        self.tokens.get(key).map(|t| format!("Bearer {t}"))
    }

    fn basic_header(&self) -> Option<String> {
        let a = self.auth.as_ref().filter(|a| !a.username.is_empty())?;
        let pair = format!("{}:{}", a.username, a.password);
        Some(format!("Basic {}", crate::base64::encode(pair.as_bytes())))
    }

    /// A token from the server the challenge names: anonymous, with the user name and
    /// password (Basic, `account=`), or with the identity token (OAuth `refresh_token`).
    fn fetch_token(
        &mut self,
        registry: &str,
        scopes: &[String],
        challenge: &str,
        what: &str,
    ) -> Result<String> {
        let params = parse_challenge(challenge).ok_or_else(|| login_required(what, registry))?;
        let realm = params
            .get("realm")
            .ok_or_else(|| login_required(what, registry))?;
        let scopes: Vec<String> = if scopes.is_empty() {
            params
                .get("scope")
                .map(|s| s.split(' ').map(String::from).collect())
                .unwrap_or_default()
        } else {
            scopes.to_vec()
        };
        let service = params.get("service").cloned();
        let auth = self.auth.clone();
        let realm_host = realm
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or(realm)
            .to_string();
        let fail = |e: ureq::Error| unreachable_registry(&realm_host, e);
        let response = match &auth {
            Some(a) if !a.identitytoken.is_empty() => {
                let mut form = vec![
                    "grant_type=refresh_token".to_string(),
                    format!("refresh_token={}", encode(&a.identitytoken)),
                    format!("client_id={CLIENT_ID}"),
                ];
                if let Some(s) = &service {
                    form.push(format!("service={}", encode(s)));
                }
                if !scopes.is_empty() {
                    form.push(format!("scope={}", encode(&scopes.join(" "))));
                }
                self.agent
                    .post(realm)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .send(form.join("&"))
                    .map_err(fail)?
            }
            _ => {
                let mut query = Vec::new();
                if let Some(s) = &service {
                    query.push(format!("service={}", encode(s)));
                }
                for s in &scopes {
                    query.push(format!("scope={}", encode(s)));
                }
                if let Some(a) = auth.as_ref().filter(|a| !a.username.is_empty()) {
                    query.push(format!("account={}", encode(&a.username)));
                    if self.offline {
                        query.push(format!("client_id={CLIENT_ID}"));
                        query.push("offline_token=true".to_string());
                    }
                }
                let token_url = if query.is_empty() {
                    realm.clone()
                } else {
                    let sep = if realm.contains('?') { '&' } else { '?' };
                    format!("{realm}{sep}{}", query.join("&"))
                };
                let mut req = self.agent.get(&token_url);
                if let Some(basic) = self.basic_header() {
                    req = req.header("Authorization", basic);
                }
                req.call().map_err(fail)?
            }
        };
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(match &auth {
                Some(_) => login_failed(registry, status),
                None => login_required(what, registry),
            });
        }
        if !response.status().is_success() {
            return Err(Error::new(
                ErrorKind::Runtime,
                format!("cannot get a token for {what}"),
                format!("the token server {realm_host} answered HTTP {status}"),
                "try again later",
            ));
        }
        let body = read_small(response).map_err(fail)?;
        #[derive(serde::Deserialize)]
        struct TokenReply {
            token: Option<String>,
            access_token: Option<String>,
            refresh_token: Option<String>,
        }
        let reply: TokenReply =
            serde_json::from_slice(&body).map_err(|_| login_required(what, registry))?;
        if let Some(r) = reply.refresh_token.filter(|r| !r.is_empty()) {
            self.refresh_token = Some(r);
        }
        reply
            .token
            .or(reply.access_token)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| login_required(what, registry))
    }
}

/// One result of [`search`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub star_count: u64,
    #[serde(default)]
    pub is_official: bool,
    #[serde(default)]
    pub is_automated: bool,
}

/// Search a registry's index (`GET /v1/search`, Docker Hub's search API). A term that
/// starts with a registry (`localhost:5000/app`) searches that registry; any other
/// searches Docker Hub.
pub fn search(term: &str, limit: u32) -> Result<Vec<SearchResult>> {
    let (registry, query) = match term.split_once('/') {
        Some((first, rest))
            if first.contains('.') || first.contains(':') || first == "localhost" =>
        {
            (first.to_string(), rest.to_string())
        }
        _ => ("docker.io".to_string(), term.to_string()),
    };
    let host = match registry.as_str() {
        "docker.io" => "index.docker.io",
        other => other,
    };
    let url = format!(
        "{}://{host}/v1/search?q={}&n={limit}",
        scheme(&registry),
        encode(&query)
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(format!("podbag/{}", env!("CARGO_PKG_VERSION")))
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
    let response = agent
        .get(&url)
        .call()
        .map_err(|e| unreachable_registry(host, e))?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(Error::new(
            ErrorKind::Unsupported,
            format!("cannot search {registry}"),
            format!("{host} answered HTTP {status} to a search"),
            "search on the registry's web site",
        ));
    }
    #[derive(serde::Deserialize)]
    struct Reply {
        #[serde(default)]
        results: Vec<SearchResult>,
    }
    let body = read_small(response).map_err(|e| unreachable_registry(host, e))?;
    let reply: Reply = serde_json::from_slice(&body).map_err(|e| {
        Error::new(
            ErrorKind::Runtime,
            format!("cannot search {registry}"),
            format!("the answer is not the expected JSON: {e}"),
            "search on the registry's web site",
        )
    })?;
    let mut results = reply.results;
    results.truncate(limit as usize);
    Ok(results)
}

/// `http` for local registries (as Docker does), else `https`.
fn scheme(registry: &str) -> &'static str {
    if reference::is_insecure(registry) {
        "http"
    } else {
        "https"
    }
}

fn pull_scope(repository: &str) -> String {
    format!("repository:{repository}:pull")
}

/// The scopes of a push: the repository, and the one a blob is mounted from.
fn push_scopes(r: &Reference, mount_from: Option<&str>) -> Vec<String> {
    let mut scopes = vec![format!("repository:{}:pull,push", r.repository)];
    scopes.extend(mount_from.map(pull_scope));
    scopes
}

fn url(r: &Reference, kind: &str, reference: &str) -> String {
    format!(
        "{}://{}/v2/{}/{kind}/{reference}",
        scheme(&r.registry),
        r.api_host(),
        r.repository
    )
}

/// A Location header as a full URL.
fn absolute(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        location.to_string()
    } else if location.starts_with('/') {
        format!("{base}{location}")
    } else {
        format!("{base}/{location}")
    }
}

fn header(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn read_small(response: Response) -> std::result::Result<Vec<u8>, ureq::Error> {
    response
        .into_body()
        .with_config()
        .limit(MAX_DOCUMENT)
        .read_to_vec()
}

/// The first message of a registry's error reply (`{"errors": [{"message": …}]}`).
fn registry_said(response: Response) -> String {
    #[derive(serde::Deserialize)]
    struct Errors {
        #[serde(default)]
        errors: Vec<One>,
    }
    #[derive(serde::Deserialize)]
    struct One {
        #[serde(default)]
        code: String,
        #[serde(default)]
        message: String,
    }
    let body = read_small(response).unwrap_or_default();
    match serde_json::from_slice::<Errors>(&body) {
        Ok(e) => e
            .errors
            .first()
            .map(|e| format!("{} ({})", e.message, e.code))
            .unwrap_or_default(),
        Err(_) => String::from_utf8_lossy(&body)
            .chars()
            .take(200)
            .collect::<String>()
            .trim()
            .to_string(),
    }
}

fn check_status(r: &Reference, status: u16, kind: &str, reference: &str) -> Result<()> {
    match status {
        200..=299 => Ok(()),
        404 if kind == "manifests" => Err(Error::new(
            ErrorKind::NotFound,
            format!("cannot find image {}", r.name()),
            format!("the registry has no manifest for {reference}"),
            "check the image name and tag",
        )),
        other => Err(Error::new(
            ErrorKind::Runtime,
            format!("cannot pull {}", r.name()),
            format!("the registry answered HTTP {other} for {kind}/{reference}"),
            "try again later",
        )),
    }
}

fn verify(r: &Reference, digest: &str, body: &[u8]) -> Result<()> {
    let actual = format!("sha256:{}", sha256_hex(body));
    if actual == digest {
        return Ok(());
    }
    Err(digest_mismatch(r, digest))
}

/// The error for content that does not match its digest.
pub fn digest_mismatch(r: &Reference, digest: &str) -> Error {
    Error::new(
        ErrorKind::Runtime,
        format!("cannot pull {}", r.name()),
        format!(
            "sha256 mismatch for {digest}: the downloaded content is not what the registry promised"
        ),
        "try again; if it keeps failing, the registry or a proxy is corrupting data",
    )
}

fn unreachable_registry(host: &str, e: ureq::Error) -> Error {
    Error::new(
        ErrorKind::Runtime,
        format!("cannot reach registry {host}"),
        e.to_string(),
        "check the network connection",
    )
}

/// `podbag login`, or `podbag login <registry>` for any but Docker Hub.
fn login_command(registry: &str) -> String {
    match registry {
        "docker.io" => "podbag login".to_string(),
        other => format!("podbag login {other}"),
    }
}

fn login_required(what: &str, registry: &str) -> Error {
    Error::new(
        ErrorKind::MissingRequirement,
        format!("cannot access {what}"),
        "the registry requires a login, or the repository does not exist",
        format!(
            "check the name, or log in with `{}`",
            login_command(registry)
        ),
    )
}

/// The registry (or its token server) refused the user name and password.
fn login_failed(registry: &str, status: u16) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!("login to {registry} failed"),
        format!("the registry refused the user name and password (HTTP {status})"),
        format!("check them, then run `{}` again", login_command(registry)),
    )
}

/// Whether an error is [`login_failed`]'s: a refused login.
pub fn is_login_failure(e: &Error) -> bool {
    e.kind == ErrorKind::InvalidInput
        && e.what.starts_with("login to ")
        && e.what.ends_with(" failed")
}

fn access_denied(what: &str, registry: &str, user: &str) -> Error {
    let who = if user.is_empty() {
        "this login".to_string()
    } else {
        format!("user {user}")
    };
    Error::new(
        ErrorKind::MissingRequirement,
        format!("cannot access {what}"),
        format!("the registry refused {who}: no access to the repository, or it does not exist"),
        format!(
            "check the name and your access, or log in again with `{}`",
            login_command(registry)
        ),
    )
}

fn push_failed(r: &Reference, status: u16, doing: &str, said: &str) -> Error {
    let why = if said.is_empty() {
        format!("the registry answered HTTP {status} {doing}")
    } else {
        format!("the registry answered HTTP {status} {doing}: {said}")
    };
    Error::new(
        ErrorKind::Runtime,
        format!("cannot push {}", r.name()),
        why,
        "check the name and your access to the repository",
    )
}

/// Parse `Bearer realm="…",service="…",scope="…"` into its parameters.
fn parse_challenge(header: &str) -> Option<HashMap<String, String>> {
    let header = header.trim();
    let rest = header
        .get(..6)
        .filter(|s| s.eq_ignore_ascii_case("bearer"))
        .map(|_| header[6..].trim_start())?;
    let mut params = HashMap::new();
    let mut chars = rest.chars().peekable();
    loop {
        let key: String = chars
            .by_ref()
            .take_while(|&c| c != '=')
            .collect::<String>()
            .trim()
            .to_string();
        if key.is_empty() {
            break;
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                value.push(c);
            }
            while chars.peek().is_some_and(|&c| c != ',') {
                chars.next();
            }
        } else {
            value = chars
                .by_ref()
                .take_while(|&c| c != ',')
                .collect::<String>()
                .trim()
                .to_string();
        }
        chars.next_if_eq(&',');
        params.insert(key.to_ascii_lowercase(), value);
    }
    Some(params)
}

/// Percent-encode a query value, keeping characters that are safe in a URL query.
fn encode(v: &str) -> String {
    v.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bearer_challenge() {
        let p = parse_challenge(r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull,push""#).unwrap();
        assert_eq!(p["realm"], "https://auth.docker.io/token");
        assert_eq!(p["service"], "registry.docker.io");
        assert_eq!(p["scope"], "repository:library/alpine:pull,push");
        assert!(parse_challenge("Basic realm=x").is_none());
        assert!(parse_challenge(r#"bearer realm="x""#).is_some());
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(
            encode("repository:library/alpine:pull,push"),
            "repository:library/alpine:pull%2Cpush"
        );
        assert_eq!(encode("a b"), "a%20b");
    }

    #[test]
    fn locations_become_full_urls() {
        let base = "http://127.0.0.1:5000";
        assert_eq!(
            absolute(base, "/v2/a/blobs/uploads/1?x=y"),
            "http://127.0.0.1:5000/v2/a/blobs/uploads/1?x=y"
        );
        assert_eq!(
            absolute(base, "https://s3.example/up"),
            "https://s3.example/up"
        );
    }

    #[test]
    fn a_refused_login_is_recognized() {
        assert!(is_login_failure(&login_failed("ghcr.io", 401)));
        assert!(!is_login_failure(&login_required("x", "ghcr.io")));
        assert_eq!(
            login_required("x", "docker.io").next,
            "check the name, or log in with `podbag login`"
        );
    }
}
