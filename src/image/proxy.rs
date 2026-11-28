//! Engine-local registry proxy settings. Only registry HTTP clients read this file.

use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use ureq::{Proxy, ProxyProtocol};

pub const MAX_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Inherited,
    Direct,
    Manual,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub mode: Mode,
    pub url: String,
    pub bypass: String,
}

fn invalid(why: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "invalid registry proxy settings",
        why,
        "use an HTTP or HTTPS proxy URL without credentials and a comma-separated host bypass list",
    )
}

fn file_error(path: &Path, error: &std::io::Error) -> Error {
    Error::io(
        format!("cannot use registry proxy settings at {}", path.display()),
        error,
        "check the permissions of the Podbag configuration directory",
    )
}

impl Settings {
    pub(crate) fn normalized(mut self) -> Result<Self> {
        if self.mode != Mode::Manual {
            self.url.clear();
            self.bypass.clear();
            return Ok(self);
        }
        self.url = self.url.trim().to_string();
        if self.url.len() > 2048 || self.bypass.len() > 4096 {
            return Err(invalid("the proxy URL or bypass list is too long"));
        }
        let rules: Vec<_> = self
            .bypass
            .split(',')
            .map(str::trim)
            .filter(|rule| !rule.is_empty())
            .collect();
        if !rules.iter().all(|rule| valid_rule(rule)) {
            return Err(invalid(
                "bypass rules must be hostnames, IP addresses, domain suffixes or *",
            ));
        }
        self.bypass = rules.join(", ");
        self.manual_proxy()?;
        Ok(self)
    }

    fn manual_proxy(&self) -> Result<Proxy> {
        let bad_url =
            || invalid("use http://host[:port] or https://host[:port] without credentials");
        let url: http::Uri = self.url.parse().map_err(|_| bad_url())?;
        let protocol = match url.scheme_str() {
            Some("http") => ProxyProtocol::Http,
            Some("https") => ProxyProtocol::Https,
            _ => return Err(bad_url()),
        };
        let authority = url.authority().ok_or_else(bad_url)?;
        if authority.as_str().contains('@')
            || authority.host().is_empty()
            || !valid_host(authority.host())
            || url.query().is_some()
            || !matches!(url.path(), "" | "/")
            || self.url.contains('#')
        {
            return Err(bad_url());
        }
        let mut builder = Proxy::builder(protocol).host(authority.host());
        if let Some(port) = url.port_u16() {
            if port == 0 {
                return Err(bad_url());
            }
            builder = builder.port(port);
        } else if authority
            .as_str()
            .strip_prefix(authority.host())
            .is_some_and(|suffix| !suffix.is_empty())
        {
            return Err(bad_url());
        }
        for rule in self
            .bypass
            .split(',')
            .map(str::trim)
            .filter(|rule| !rule.is_empty())
        {
            if rule.parse::<std::net::Ipv6Addr>().is_ok() {
                builder = builder.no_proxy(&format!("[{rule}]"));
            }
            builder = builder.no_proxy(rule);
        }
        builder.build().map_err(|_| bad_url())
    }

    pub(crate) fn proxy(&self) -> Result<Option<Proxy>> {
        match self.mode {
            Mode::Inherited => Ok(Proxy::try_from_env()),
            Mode::Direct => Ok(None),
            Mode::Manual => self.manual_proxy().map(Some),
        }
    }
}

fn valid_host(host: &str) -> bool {
    let ip = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    ip.parse::<std::net::IpAddr>().is_ok()
        || host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn valid_rule(rule: &str) -> bool {
    rule == "*"
        || valid_host(
            rule.strip_prefix("*.")
                .or_else(|| rule.strip_prefix('.'))
                .unwrap_or(rule),
        )
}

pub fn read_json(mut input: impl Read) -> Result<Settings> {
    let mut bytes = Vec::new();
    (&mut input)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("the settings could not be read"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("the settings are too long"));
    }
    // The parser can quote untrusted values. Never include its text in an error.
    serde_json::from_slice::<Settings>(&bytes)
        .map_err(|_| invalid("the settings are not valid JSON"))?
        .normalized()
}

pub fn load(dirs: &Dirs) -> Result<Settings> {
    let path = dirs.config.join("registry-proxy.json");
    match std::fs::File::open(&path) {
        Ok(file) => read_json(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(file_error(&path, &error)),
    }
}

pub fn save(dirs: &Dirs, settings: Settings) -> Result<Settings> {
    let settings = settings.normalized()?;
    let path = dirs.config.join("registry-proxy.json");
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dirs.config)?;
        let mut file = tempfile::NamedTempFile::new_in(&dirs.config)?;
        serde_json::to_writer_pretty(&mut file, &settings)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
        Ok(())
    };
    write().map_err(|error| file_error(&path, &error))?;
    Ok(settings)
}
