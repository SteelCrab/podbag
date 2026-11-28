//! Per-container logging configuration shared by the CLI, Engine API and supervisor.

use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Driver {
    #[default]
    JsonFile,
    Syslog,
    Journald,
}

/// A container's validated logging driver and its options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(rename = "Type")]
    driver: Driver,
    #[serde(rename = "Config")]
    options: BTreeMap<String, String>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum Address {
    Udp(SocketAddr),
    Tcp(SocketAddr),
    UnixGram(String),
    Unix(String),
}

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Default,
    Rfc3164,
    Rfc5424,
    Rfc5424Micro,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Syslog {
    pub address: Address,
    pub facility: u8,
    pub format: Format,
}

impl LogConfig {
    /// Parse the logging driver name and Docker-compatible options.
    pub fn parse(driver: &str, options: BTreeMap<String, String>) -> Result<Self> {
        let driver = match driver {
            "" | "json-file" => Driver::JsonFile,
            "syslog" => Driver::Syslog,
            "journald" => Driver::Journald,
            _ => return Err(invalid(format!("unknown driver {driver:?}"))),
        };
        let config = Self { driver, options };
        config.validate()?;
        Ok(config)
    }

    /// Read repeated CLI options of the form `key=value`.
    pub fn from_flags(driver: &str, opts: &[String]) -> Result<Self> {
        let mut options = BTreeMap::new();
        for option in opts {
            let Some((key, value)) = option.split_once('=') else {
                return Err(invalid("log options must have the form KEY=VALUE"));
            };
            options.insert(key.to_owned(), value.to_owned());
        }
        Self::parse(driver, options)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        for key in self.options.keys().map(String::as_str) {
            let supported = match self.driver {
                Driver::JsonFile => false,
                Driver::Syslog => matches!(
                    key,
                    "syslog-address" | "syslog-facility" | "syslog-format" | "tag"
                ),
                Driver::Journald => key == "tag",
            };
            if !supported {
                return Err(invalid(format!(
                    "unsupported option {key:?} for {:?}",
                    self.driver
                )));
            }
        }
        if self.driver == Driver::Syslog {
            self.syslog()?;
        }
        if let Some(tag) = self.options.get("tag") {
            render_tag(tag, |_| "")?;
        }
        Ok(())
    }

    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn driver(&self) -> Driver {
        self.driver
    }

    pub(crate) fn syslog(&self) -> Result<Syslog> {
        let option =
            |key, fallback: &'static str| self.options.get(key).map_or(fallback, String::as_str);
        let address = match option("syslog-address", "") {
            "" => Address::UnixGram("/dev/log".into()),
            address => parse_address(address)?,
        };
        let facility = match option("syslog-facility", "") {
            "kern" => 0,
            "user" => 1,
            "mail" => 2,
            "" | "daemon" => 3,
            "auth" | "security" => 4,
            "syslog" => 5,
            "lpr" => 6,
            "news" => 7,
            "uucp" => 8,
            "cron" => 9,
            "authpriv" => 10,
            "ftp" => 11,
            f => match f.strip_prefix("local").map(str::as_bytes) {
                Some([n @ b'0'..=b'7']) => 16 + n - b'0',
                _ => f
                    .parse::<u8>()
                    .ok()
                    .filter(|n| *n <= 23)
                    .ok_or_else(|| invalid(format!("invalid syslog facility {f:?}")))?,
            },
        };
        let format = match option("syslog-format", "") {
            "" => Format::Default,
            "rfc3164" => Format::Rfc3164,
            "rfc5424" => Format::Rfc5424,
            "rfc5424micro" => Format::Rfc5424Micro,
            f => return Err(invalid(format!("invalid syslog format {f:?}"))),
        };
        Ok(Syslog {
            address,
            facility,
            format,
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn tag(&self, spec: &crate::spec::ContainerSpec) -> Result<String> {
        let image = spec.image.as_deref().unwrap_or_default();
        let image = image.strip_prefix("sha256:").unwrap_or(image);
        let tag = self.options.get("tag").map_or("{{.ID}}", String::as_str);
        render_tag(tag, |field| match field {
            ".ID" => spec.id.get(..12).unwrap_or(&spec.id),
            ".FullID" => &spec.id,
            ".Name" => &spec.name,
            ".ImageName" => spec.image_name.as_deref().unwrap_or_default(),
            ".ImageID" => image.get(..12).unwrap_or(image),
            ".ImageFullID" => image,
            _ => unreachable!("tag fields were checked before rendering"),
        })
    }
}

fn parse_address(value: &str) -> Result<Address> {
    let Some((scheme, endpoint)) = value.split_once("://") else {
        return Err(invalid(
            "syslog-address must include a transport and address",
        ));
    };
    match scheme {
        "unix" | "unixgram" => {
            if !endpoint.starts_with('/') || endpoint.len() > 107 || endpoint.contains('\0') {
                return Err(invalid(
                    "syslog-address requires an absolute Unix socket path of at most 107 bytes",
                ));
            }
            Ok(if scheme == "unix" {
                Address::Unix(endpoint.to_owned())
            } else {
                Address::UnixGram(endpoint.to_owned())
            })
        }
        "udp" | "tcp" => {
            let addr = endpoint
                .parse::<SocketAddr>()
                .ok()
                .or_else(|| {
                    let host = endpoint.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(endpoint);
                    host.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 514))
                })
                .or_else(|| {
                    let port = match endpoint {
                        "localhost" => 514,
                        _ => endpoint.strip_prefix("localhost:")?.parse().ok()?,
                    };
                    Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port))
                })
                .filter(|addr| addr.port() != 0)
                .ok_or_else(|| invalid("syslog-address requires an IPv4 or IPv6 address or localhost, and a valid port"))?;
            Ok(if scheme == "udp" {
                Address::Udp(addr)
            } else {
                Address::Tcp(addr)
            })
        }
        _ => Err(invalid(
            "syslog-address supports udp, tcp, unix or unixgram",
        )),
    }
}

fn render_tag<'a>(template: &str, field_value: impl Fn(&str) -> &'a str) -> Result<String> {
    if template.len() > 1024 || template.chars().any(char::is_control) {
        return Err(invalid(
            "log tag must be at most 1024 bytes and contain no control characters",
        ));
    }
    let mut out = String::new();
    let mut rest = template;
    while let Some((text, field)) = rest.split_once("{{") {
        out.push_str(text);
        let Some((field, next)) = field.split_once("}}") else {
            return Err(invalid("unclosed log tag field"));
        };
        let field = field.trim();
        if !matches!(
            field,
            ".ID" | ".FullID" | ".Name" | ".ImageName" | ".ImageID" | ".ImageFullID"
        ) {
            return Err(invalid(format!("unsupported log tag field {field:?}")));
        }
        out.push_str(field_value(field));
        rest = next;
    }
    out.push_str(rest);
    Ok(out)
}

fn invalid(why: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "invalid log configuration",
        why,
        "check the logging driver and options",
    )
}
