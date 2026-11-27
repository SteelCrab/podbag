use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Options for a Docker-compatible local NFS volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NfsOptions {
    #[serde(rename = "type")]
    fs_type: String,
    device: String,
    #[serde(rename = "o", default, skip_serializing_if = "Option::is_none")]
    options: Option<String>,
}

pub fn parse_options(options: BTreeMap<String, String>) -> Result<Option<NfsOptions>> {
    if options.is_empty() {
        return Ok(None);
    }
    let bad = |why: &str| {
        Error::new(
            ErrorKind::InvalidInput,
            "invalid local volume options",
            why,
            "set type=nfs or nfs4, device=SERVER:/export and, if needed, o=addr=ADDRESS",
        )
    };
    let fs_type = options.get("type").ok_or_else(|| bad("type is required"))?;
    if !matches!(fs_type.as_str(), "nfs" | "nfs4") {
        return Err(bad("the local driver accepts only nfs and nfs4 options"));
    }
    if options
        .keys()
        .any(|k| !matches!(k.as_str(), "type" | "device" | "o"))
    {
        return Err(bad("the local driver accepts type, device and o"));
    }
    let device = options
        .get("device")
        .ok_or_else(|| bad("device is required"))?;
    let mount = NfsOptions {
        fs_type: fs_type.clone(),
        device: device.clone(),
        options: options.get("o").cloned(),
    };
    let (server, _) = mount
        .export()
        .ok_or_else(|| bad("device must be SERVER:/export or :/export"))?;
    if device.contains('\0') || mount.options.as_ref().is_some_and(|o| o.contains('\0')) {
        return Err(bad("options must not contain NUL bytes"));
    }
    if mount.address().is_empty() || mount.parameter("addr").is_some_and(str::is_empty) {
        return Err(bad("set addr in o when the device omits the server"));
    }
    if !server.is_empty() && server.chars().any(char::is_whitespace) {
        return Err(bad("the device server must not contain whitespace"));
    }
    Ok(Some(mount))
}

impl NfsOptions {
    fn export(&self) -> Option<(&str, &str)> {
        let (server, path) = self.device.split_once(":/")?;
        let bracketed_ip = server.starts_with('[') && server.ends_with(']');
        (bracketed_ip || !server.contains(':')).then_some((server, path))
    }

    fn address(&self) -> &str {
        let server = self.export().map(|(server, _)| server).unwrap_or_default();
        self.parameter("addr").unwrap_or(server)
    }

    fn parameter(&self, key: &str) -> Option<&str> {
        self.parameters()
            .filter_map(|option| option.split_once('='))
            .filter_map(|(k, value)| (k == key).then_some(value))
            .last()
    }

    #[cfg(any(test, target_os = "linux"))]
    pub(crate) fn source(&self) -> String {
        if self.device.starts_with(':') {
            let address = self.address();
            if address.contains(':') && !address.starts_with('[') {
                return format!("[{address}]{}", self.device);
            }
            return format!("{address}{}", self.device);
        }
        self.device.clone()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn resolve_address(&self) -> std::io::Result<std::net::IpAddr> {
        use std::net::ToSocketAddrs;
        (self.address().trim_matches(['[', ']']), 0)
            .to_socket_addrs()?
            .next()
            .map(|sock| sock.ip())
            .ok_or_else(|| std::io::Error::other("the NFS server has no IP address"))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn fs_type(&self) -> &str {
        &self.fs_type
    }

    pub(crate) fn parameters(&self) -> impl Iterator<Item = &str> {
        self.options
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
    }

    pub(crate) fn read_only(&self) -> bool {
        self.parameters()
            .filter(|option| matches!(*option, "ro" | "rw"))
            .last()
            == Some("ro")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(fields: &[(&str, &str)]) -> BTreeMap<String, String> {
        fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn docker_nfs_address_uses_option_or_device_host() {
        let mount = parse_options(opts(&[
            ("type", "nfs"),
            ("device", ":/data"),
            ("o", "vers=4,addr=::1,rw"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(mount.source(), "[::1]:/data");
        assert_eq!(mount.address(), "::1");
        let mount = parse_options(opts(&[("type", "nfs4"), ("device", "[::1]:/data")]))
            .unwrap()
            .unwrap();
        assert_eq!(mount.source(), "[::1]:/data");
        assert_eq!(mount.address(), "[::1]");
    }

    #[test]
    fn invalid_volume_options_are_refused() {
        for invalid in [
            opts(&[("type", "nfs"), ("device", ":/data")]),
            opts(&[("type", "nfs"), ("device", "server:relative")]),
            opts(&[("type", "cifs"), ("device", "server:/data")]),
            opts(&[
                ("type", "nfs"),
                ("device", "server:/data"),
                ("other", "arg"),
            ]),
        ] {
            assert_eq!(
                parse_options(invalid).unwrap_err().kind,
                ErrorKind::InvalidInput
            );
        }
    }
}
