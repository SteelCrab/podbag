//! A minimal client for the OCI distribution API: manifests, configs and layer blobs.
//!
//! Every manifest and config is checked against its sha256 digest here. Layer blobs are
//! returned as streams; the caller hashes them while reading.

use super::manifest::{self, ConfigFile, Descriptor, Fetched};
use super::reference::Reference;
use crate::{Error, ErrorKind, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

/// Manifests and configs are small; refuse anything larger.
const MAX_DOCUMENT: u64 = 4 * 1024 * 1024;

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
    pub config: ConfigFile,
    pub layers: Vec<Descriptor>,
}

pub struct Registry {
    agent: ureq::Agent,
    /// Bearer tokens per "registry/repository".
    tokens: HashMap<String, String>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}

impl Registry {
    pub fn new() -> Registry {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(format!("openpod/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_resolve(Some(Duration::from_secs(30)))
            // A registry that stops sending for this long is treated as gone.
            .timeout_per_read(Some(Duration::from_secs(60)))
            .build()
            .into();
        Registry {
            agent,
            tokens: HashMap::new(),
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
            config,
            layers,
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
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response
            .into_body()
            .with_config()
            .limit(MAX_DOCUMENT)
            .read_to_vec()
            .map_err(|e| unreachable_registry(r, e))?;
        Ok((content_type, body))
    }

    /// GET with this repository's token. On a Bearer challenge, fetch a token and retry once.
    fn get(
        &mut self,
        r: &Reference,
        url: &str,
        accept: Option<&str>,
    ) -> Result<http::Response<ureq::Body>> {
        let key = format!("{}/{}", r.registry, r.repository);
        let response = self.send(r, url, accept, self.tokens.get(&key).cloned())?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or_default();
        let token = self.fetch_token(r, &challenge)?;
        self.tokens.insert(key, token.clone());
        let response = self.send(r, url, accept, Some(token))?;
        if response.status().as_u16() == 401 {
            return Err(login_required(r));
        }
        Ok(response)
    }

    fn send(
        &self,
        r: &Reference,
        url: &str,
        accept: Option<&str>,
        token: Option<String>,
    ) -> Result<http::Response<ureq::Body>> {
        let mut request = self.agent.get(url);
        if let Some(a) = accept {
            request = request.header("Accept", a);
        }
        if let Some(t) = token {
            request = request.header("Authorization", format!("Bearer {t}"));
        }
        request.call().map_err(|e| unreachable_registry(r, e))
    }

    /// The anonymous token flow: GET realm?service=…&scope=…
    fn fetch_token(&self, r: &Reference, challenge: &str) -> Result<String> {
        let params = parse_challenge(challenge).ok_or_else(|| login_required(r))?;
        let realm = params.get("realm").ok_or_else(|| login_required(r))?;
        let mut query = Vec::new();
        for key in ["service", "scope"] {
            if let Some(v) = params.get(key) {
                query.push(format!("{key}={}", encode(v)));
            }
        }
        let token_url = if query.is_empty() {
            realm.clone()
        } else {
            format!("{realm}?{}", query.join("&"))
        };
        let response = self
            .agent
            .get(&token_url)
            .call()
            .map_err(|e| unreachable_registry(r, e))?;
        if !response.status().is_success() {
            return Err(login_required(r));
        }
        let body = response
            .into_body()
            .with_config()
            .limit(MAX_DOCUMENT)
            .read_to_vec()
            .map_err(|e| unreachable_registry(r, e))?;
        #[derive(serde::Deserialize)]
        struct TokenReply {
            token: Option<String>,
            access_token: Option<String>,
        }
        let reply: TokenReply = serde_json::from_slice(&body).map_err(|_| login_required(r))?;
        reply
            .token
            .or(reply.access_token)
            .ok_or_else(|| login_required(r))
    }
}

fn url(r: &Reference, kind: &str, reference: &str) -> String {
    let scheme = if r.insecure() { "http" } else { "https" };
    format!(
        "{scheme}://{}/v2/{}/{kind}/{reference}",
        r.api_host(),
        r.repository
    )
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

fn unreachable_registry(r: &Reference, e: ureq::Error) -> Error {
    Error::new(
        ErrorKind::Runtime,
        format!("cannot reach registry {}", r.api_host()),
        e.to_string(),
        "check the network connection",
    )
}

fn login_required(r: &Reference) -> Error {
    Error::new(
        ErrorKind::MissingRequirement,
        format!("cannot access image {}", r.name()),
        "the registry requires a login",
        "private registries are not supported yet; use a public image",
    )
}

/// Parse `Bearer realm="…",service="…",scope="…"` into its parameters.
fn parse_challenge(header: &str) -> Option<HashMap<String, String>> {
    let rest = header.trim().strip_prefix("Bearer")?.trim_start();
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
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(
            encode("repository:library/alpine:pull,push"),
            "repository:library/alpine:pull%2Cpush"
        );
    }
}
