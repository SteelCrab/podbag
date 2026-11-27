//! The container settings that the helper process receives.

use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

/// The PATH a container gets when the caller does not set one. Same as Docker.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Where the container's root filesystem comes from. All paths are absolute host paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rootfs {
    /// A plain directory, used as is (`run --rootfs`).
    Dir { path: String },
    /// An image tree as the read-only lower layer, with a private writable upper layer.
    Overlay {
        lower: String,
        upper: String,
        work: String,
        merged: String,
        /// The object store as overlayfs's data-only layer, when `lower` is an image's
        /// EROFS metadata (composefs); None for a hard-link tree.
        #[serde(default)]
        data: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub id: String,
    pub rootfs: Rootfs,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub hostname: String,
    /// `user`, `uid`, `user:group` …; None means root.
    #[serde(default)]
    pub user: Option<String>,
    /// The digest of the image the container uses, if any.
    #[serde(default)]
    pub image: Option<String>,
    /// The image name as the user gave it, for display.
    #[serde(default)]
    pub image_name: Option<String>,
    /// Unique name; the short ID when the user gave none.
    #[serde(default)]
    pub name: String,
    /// Delete the container when it ends (`run --rm`, and the one-shot library calls).
    #[serde(default)]
    pub auto_remove: bool,
    /// Give the command a terminal (`-t`).
    #[serde(default)]
    pub tty: bool,
    /// Connect the caller's stdin (`-i`).
    #[serde(default)]
    pub interactive: bool,
    /// Run in the background (`-d`): the shim leaves the caller's session.
    #[serde(default)]
    pub detach: bool,
    /// Labels (Docker's `--label`; Compose finds its containers by them).
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// Resource limits (`--memory`, `--cpus`, `--pids-limit`).
    #[serde(default)]
    pub limits: crate::cgroup::Limits,
    /// Where the container's cgroup goes.
    #[serde(default)]
    pub cgroup: crate::cgroup::Placement,
    /// Capabilities, no_new_privs and seccomp.
    #[serde(default)]
    pub security: Security,
    /// Volumes, host paths and tmpfs mounts, in order.
    #[serde(default)]
    pub mounts: Vec<crate::volume::Mount>,
    /// How the container is connected. Containers made before networks existed used the
    /// host's network.
    #[serde(default = "crate::network::NetworkMode::legacy")]
    pub network: crate::network::NetworkMode,
    /// Published ports (`-p`).
    #[serde(default)]
    pub publish: Vec<crate::network::PortMapping>,
    /// Further names of the container on its primary network (`--network-alias`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Extra `/etc/hosts` lines (`--add-host`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_hosts: Vec<crate::netfiles::ExtraHost>,
    /// DNS settings for its `/etc/resolv.conf` (`--dns`, `--dns-search`, `--dns-option`).
    #[serde(
        default,
        skip_serializing_if = "crate::netfiles::DnsSettings::is_empty"
    )]
    pub dns: crate::netfiles::DnsSettings,
    /// Seconds allowed for a graceful stop. A negative value allows an unlimited wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_timeout: Option<i64>,
    /// When the container is started again after it ended (`--restart`).
    #[serde(default)]
    pub restart: crate::restart::RestartPolicy,
    /// Its health check: its own, or its image's (`--health-cmd`, `HEALTHCHECK`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<crate::health::HealthConfig>,
    /// The first size of its terminal (rows, columns), with `tty` (Docker's
    /// `ConsoleSize`): output before the first resize is laid out for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_size: Option<(u16, u16)>,
    /// Bridge networks beyond the one in `network`, in order: `eth1`, `eth2`, …
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_networks: Vec<Attachment>,
    /// Namespaces the container shares with others (a Kubernetes pod's) instead of its own.
    #[serde(default, skip_serializing_if = "Namespaces::is_own")]
    pub namespaces: Namespaces,
    /// Device nodes of the host the container may use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<Device>,
    /// GPU selections to resolve on the host when the container starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_requests: Vec<crate::gpu::Request>,
    /// Write the output to this file in the Kubernetes log format, not to `log.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cri_log: Option<String>,
    /// A named pipe the container reads its input from (Kubernetes attach writes to it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_fifo: Option<String>,
}

/// One namespace of a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Namespace {
    /// A new one of its own.
    #[default]
    Own,
    /// The one of the process that starts it (the host's).
    Host,
    /// The one held at this path (a bind mount of a namespace file).
    Join { path: String },
}

/// The namespaces a container shares. The network is shared through `network` (the host's)
/// or `net` (a pod's: `Join`, with the network mode `None`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Namespaces {
    pub net: Namespace,
    pub ipc: Namespace,
    pub uts: Namespace,
    pub pid: Namespace,
}

impl Namespaces {
    pub fn is_own(&self) -> bool {
        *self == Namespaces::default()
    }
}

/// A device node of the host, at `container` in the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub host: String,
    pub container: String,
    /// What the container may do with it: `r`, `w` and `m` (make the node).
    pub access: String,
}

impl Device {
    pub fn new(host: &str, container: &str, access: &str) -> Result<Self> {
        let device = Self {
            host: host.into(),
            container: if container.is_empty() {
                host
            } else {
                container
            }
            .into(),
            access: if access.is_empty() { "rwm" } else { access }.into(),
        };
        device.validate()?;
        Ok(device)
    }

    fn validate(&self) -> Result<()> {
        if !self.host.starts_with('/') || self.host.contains('\0') {
            return Err(device_error(
                "the host path must be an absolute device path",
            ));
        }
        if self.container.contains('\0') || self.container.split('/').any(|part| part == ".") {
            return Err(device_error(
                "the container path contains an invalid component",
            ));
        }
        crate::volume::check_target(&self.container)?;
        if !device_access(&self.access) {
            return Err(device_error(
                "access must contain one or more of r, w and m",
            ));
        }
        Ok(())
    }
}

impl std::str::FromStr for Device {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.as_slice() {
            [host] => Self::new(host, "", ""),
            [host, access] if device_access(access) => Self::new(host, "", access),
            [host, container] => Self::new(host, container, ""),
            [host, container, access] => Self::new(host, container, access),
            _ => Err(device_error(
                "expected a host path, container path and access",
            )),
        }
    }
}

fn device_access(access: &str) -> bool {
    !access.is_empty() && access.bytes().all(|c| matches!(c, b'r' | b'w' | b'm'))
}

fn device_error(why: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "invalid device mapping",
        why,
        "use HOST[:CONTAINER][:rwm], for example /dev/fuse:/dev/fuse:rw",
    )
}

/// A further network of a container (`--network` given again; an entry of Docker's
/// `EndpointsConfig`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub network: String,
    /// Further names of the container on this network.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// At most this many networks for one container: `eth0` to `eth15`.
pub const MAX_NETWORKS: usize = 16;

/// Check a container's networks: further networks join a bridge network, no network
/// comes twice, and aliases are host names on a user network (the default network gives
/// no names, as with Docker).
pub fn check_networks(
    network: &crate::network::NetworkMode,
    aliases: &[String],
    extra: &[Attachment],
) -> Result<()> {
    use crate::network::{DEFAULT, NetworkMode};
    let invalid = |why: String, next: &str| {
        Err(Error::new(
            ErrorKind::InvalidInput,
            "invalid network settings",
            why,
            next,
        ))
    };
    let primary = match network {
        NetworkMode::Bridge { network } => network.as_str(),
        NetworkMode::Host | NetworkMode::None => {
            let mode = if *network == NetworkMode::Host {
                "host"
            } else {
                "none"
            };
            if !extra.is_empty() {
                return invalid(
                    format!("a container with --network {mode} cannot join other networks"),
                    "give only bridge networks, or only one network mode",
                );
            }
            if !aliases.is_empty() {
                return invalid(
                    "network aliases are only for user-defined networks".to_string(),
                    "create a network with `podbag network create` and use it",
                );
            }
            return Ok(());
        }
    };
    if extra.len() + 1 > MAX_NETWORKS {
        return invalid(
            format!("a container joins at most {MAX_NETWORKS} networks"),
            "give fewer networks",
        );
    }
    let mut seen = vec![primary];
    for a in extra {
        if matches!(a.network.as_str(), "host" | "none") {
            return invalid(
                format!(
                    "--network {} cannot be joined with other networks",
                    a.network
                ),
                "give only bridge networks, or only one network mode",
            );
        }
        if seen.contains(&a.network.as_str()) {
            return invalid(
                format!("network {} is given twice", a.network),
                "give each network once",
            );
        }
        seen.push(&a.network);
    }
    let lists = std::iter::once((primary, aliases)).chain(
        extra
            .iter()
            .map(|a| (a.network.as_str(), a.aliases.as_slice())),
    );
    for (network, aliases) in lists {
        if network == DEFAULT && !aliases.is_empty() {
            return invalid(
                "network aliases are only for user-defined networks; the default network gives no names"
                    .to_string(),
                "create a network with `podbag network create` and use it",
            );
        }
        for alias in aliases {
            crate::network::check_alias(alias)?;
        }
    }
    Ok(())
}

/// The security settings of a container (and of commands `exec` runs in it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Security {
    /// `--cap-add`: capability names, or `ALL`.
    pub cap_add: Vec<String>,
    /// `--cap-drop`: capability names, or `ALL`.
    pub cap_drop: Vec<String>,
    /// `--security-opt no-new-privileges`: setuid programs cannot gain privileges.
    pub no_new_privileges: bool,
    /// The default seccomp filter; `--security-opt seccomp=unconfined` turns it off.
    pub seccomp: bool,
    /// `--privileged`: every capability, no seccomp filter, no masked or read-only kernel
    /// files, a writable /sys and cgroup. Rootless, all of it inside the user namespace.
    pub privileged: bool,
    /// Supplementary groups beyond the user's own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<u32>,
    /// The user's own group is a supplementary group too.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub own_group_too: bool,
    /// The processes' `oom_score_adj`; None: 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oom_score_adj: Option<i32>,
    /// The root filesystem is read-only (its mounts are as they say).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub read_only_rootfs: bool,
    /// Kernel files to hide, instead of Docker's list (absolute paths).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub masked_paths: Option<Vec<String>>,
    /// Kernel files to make read-only, instead of Docker's list (absolute paths).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly_paths: Option<Vec<String>>,
}

impl Default for Security {
    fn default() -> Self {
        Security {
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            no_new_privileges: false,
            seccomp: true,
            privileged: false,
            groups: Vec::new(),
            own_group_too: false,
            oom_score_adj: None,
            read_only_rootfs: false,
            masked_paths: None,
            readonly_paths: None,
        }
    }
}

/// One `--security-opt` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityOpt {
    NoNewPrivileges(bool),
    SeccompUnconfined,
}

/// Read a `--security-opt` value, as Docker writes them (`=` or the older `:`).
pub fn parse_security_opt(s: &str) -> Result<SecurityOpt> {
    let bad = |why: &str| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid security option {s:?}"),
            why,
            "use no-new-privileges[=true|false] or seccomp=unconfined",
        )
    };
    let (key, value) = match s.split_once(['=', ':']) {
        Some((k, v)) => (k, Some(v)),
        None => (s, None),
    };
    match (key, value) {
        ("no-new-privileges", None | Some("true")) => Ok(SecurityOpt::NoNewPrivileges(true)),
        ("no-new-privileges", Some("false")) => Ok(SecurityOpt::NoNewPrivileges(false)),
        ("no-new-privileges", Some(_)) => Err(bad("no-new-privileges takes true or false")),
        ("seccomp", Some("unconfined")) => Ok(SecurityOpt::SeccompUnconfined),
        ("seccomp", _) => Err(bad("custom seccomp profiles are not supported")),
        _ => Err(bad("podbag does not know this option")),
    }
}

impl ContainerSpec {
    pub fn validate(&self) -> Result<()> {
        let invalid = |why: &str, next: &str| {
            Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid container settings",
                why,
                next,
            ))
        };
        if self.command.is_empty() || self.command[0].is_empty() {
            return invalid(
                "no command was given",
                "give the command to run in the container",
            );
        }
        let paths: Vec<&str> = match &self.rootfs {
            Rootfs::Dir { path } => vec![path],
            Rootfs::Overlay {
                lower,
                upper,
                work,
                merged,
                data,
            } => {
                let mut paths: Vec<&str> = vec![lower, upper, work, merged];
                paths.extend(data.as_deref());
                paths
            }
        };
        if let Some(p) = paths.iter().find(|p| !p.starts_with('/')) {
            return invalid(
                &format!("the root filesystem path {p} is not absolute"),
                "pass an absolute path",
            );
        }
        if matches!(self.rootfs, Rootfs::Overlay { .. })
            && paths.iter().any(|p| p.contains(',') || p.contains(':'))
        {
            return invalid(
                "the podbag data directory path contains ',' or ':', which overlayfs cannot use",
                "set PODBAG_DATA_DIR to a path without them",
            );
        }
        crate::caps::effective(&self.security.cap_add, &self.security.cap_drop)?;
        for m in &self.mounts {
            crate::volume::check_target(&m.target)?;
        }
        for device in &self.devices {
            device.validate()?;
        }
        if self.hostname.is_empty() {
            return invalid("the hostname is empty", "give a non-empty hostname");
        }
        if let Some(dir) = &self.workdir {
            if !dir.starts_with('/') {
                return invalid(
                    &format!("the working directory {dir} is not absolute"),
                    "pass an absolute path inside the container, such as /app",
                );
            }
        }
        check_networks(&self.network, &self.aliases, &self.extra_networks)?;
        self.extra_hosts.iter().try_for_each(|e| e.check())?;
        self.dns.check()?;
        self.check_namespaces()
    }

    /// A pod's network is joined with the network mode `None`; the host's is the mode
    /// `Host`. Joined namespaces are absolute paths.
    fn check_namespaces(&self) -> Result<()> {
        let invalid = |why: &str| {
            Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid namespace settings",
                why,
                "report this bug",
            ))
        };
        let ns = &self.namespaces;
        match &ns.net {
            Namespace::Own => {}
            Namespace::Host => return invalid("the host's network is the network mode host"),
            Namespace::Join { .. } if self.network != crate::network::NetworkMode::None => {
                return invalid("a joined network namespace needs the network mode none");
            }
            Namespace::Join { .. } => {}
        }
        for n in [&ns.net, &ns.ipc, &ns.uts, &ns.pid] {
            if let Namespace::Join { path } = n
                && !path.starts_with('/')
            {
                return invalid("a namespace path is not absolute");
            }
        }
        Ok(())
    }

    /// Whether the container gets a network namespace of its own (and sets up its
    /// loopback interface).
    pub fn own_netns(&self) -> bool {
        self.network != crate::network::NetworkMode::Host && self.namespaces.net == Namespace::Own
    }

    /// The bridge networks the container joins, its primary one first; none with the
    /// host's network or none.
    pub fn bridge_networks(&self) -> Vec<&str> {
        match &self.network {
            crate::network::NetworkMode::Bridge { network } => std::iter::once(network.as_str())
                .chain(self.extra_networks.iter().map(|a| a.network.as_str()))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The container's aliases on network number `index` of [`Self::bridge_networks`].
    pub fn aliases_on(&self, index: usize) -> &[String] {
        match index {
            0 => &self.aliases,
            n => self
                .extra_networks
                .get(n - 1)
                .map_or(&[][..], |a| a.aliases.as_slice()),
        }
    }

    /// The environment the command gets: defaults first, then the caller's values.
    /// Each name appears once; the last value given wins. `home` is the user's home
    /// directory, used when nobody set HOME.
    pub fn effective_env(&self, home: &str) -> Vec<(String, String)> {
        let defaults = [
            ("PATH".to_string(), DEFAULT_PATH.to_string()),
            ("HOSTNAME".to_string(), self.hostname.clone()),
            ("HOME".to_string(), home.to_string()),
        ];
        let mut out: Vec<(String, String)> = Vec::new();
        for (k, v) in defaults.into_iter().chain(self.env.iter().cloned()) {
            match out.iter_mut().find(|(name, _)| *name == k) {
                Some(slot) => slot.1 = v,
                None => out.push((k, v)),
            }
        }
        out
    }
}

#[cfg(test)]
pub(crate) fn spec_fixture() -> ContainerSpec {
    ContainerSpec {
        id: "a".repeat(64),
        rootfs: Rootfs::Dir { path: "/r".into() },
        command: vec!["sh".into()],
        env: vec![],
        workdir: None,
        hostname: "box".into(),
        user: None,
        image: None,
        image_name: None,
        name: "box".into(),
        auto_remove: false,
        tty: false,
        interactive: false,
        detach: false,
        labels: Default::default(),
        limits: Default::default(),
        cgroup: Default::default(),
        security: Default::default(),
        mounts: Vec::new(),
        network: crate::network::NetworkMode::Host,
        publish: Vec::new(),
        aliases: Vec::new(),
        extra_hosts: Vec::new(),
        dns: Default::default(),
        stop_timeout: None,
        restart: Default::default(),
        health: None,
        console_size: None,
        extra_networks: Vec::new(),
        namespaces: Default::default(),
        devices: Vec::new(),
        gpu_requests: Vec::new(),
        cri_log: None,
        stdin_fifo: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn further_networks_and_aliases_are_checked() {
        use crate::network::NetworkMode;
        let mut spec = spec_fixture();
        spec.network = NetworkMode::Bridge {
            network: "app".into(),
        };
        spec.aliases = vec!["web".into(), "p-web-1".into()];
        spec.extra_networks = vec![Attachment {
            network: "back".into(),
            aliases: vec!["w".into()],
        }];
        spec.validate().unwrap();
        assert_eq!(spec.bridge_networks(), ["app", "back"]);
        assert_eq!(spec.aliases_on(0), ["web", "p-web-1"]);
        assert_eq!(spec.aliases_on(1), ["w"]);
        assert!(spec.aliases_on(2).is_empty());
        let refused = |change: &dyn Fn(&mut ContainerSpec)| {
            let mut s = spec.clone();
            change(&mut s);
            s.validate().map_err(|e| e.kind)
        };
        let invalid = Err(ErrorKind::InvalidInput);
        // The host's network or none: no further networks, no aliases.
        assert_eq!(refused(&|s| s.network = NetworkMode::Host), invalid);
        assert_eq!(
            refused(&|s| {
                s.network = NetworkMode::None;
                s.extra_networks.clear();
            }),
            invalid
        );
        // One network twice.
        assert_eq!(
            refused(&|s| s.extra_networks[0].network = "app".into()),
            invalid
        );
        assert_eq!(
            refused(&|s| s.extra_networks.push(s.extra_networks[0].clone())),
            invalid
        );
        assert_eq!(
            refused(&|s| s.extra_networks[0].network = "host".into()),
            invalid
        );
        // An alias that would break the hosts file.
        assert_eq!(refused(&|s| s.aliases.push("a b".into())), invalid);
        assert_eq!(
            refused(&|s| s.extra_networks[0].aliases.push("x\ny".into())),
            invalid
        );
        // Aliases on the default network, which has no names.
        assert_eq!(
            refused(&|s| {
                s.network = NetworkMode::default();
            }),
            invalid
        );
        assert_eq!(
            refused(&|s| s.extra_networks[0].network = crate::network::DEFAULT.into()),
            invalid
        );
        // The default network without aliases is a network like the others.
        let mut both = spec.clone();
        both.network = NetworkMode::default();
        both.aliases.clear();
        both.validate().unwrap();
        // At most 16 networks (eth0 to eth15).
        assert_eq!(
            refused(&|s| {
                s.extra_networks = (0..16)
                    .map(|i| Attachment {
                        network: format!("n{i}"),
                        aliases: Vec::new(),
                    })
                    .collect();
            }),
            invalid
        );
    }

    #[test]
    fn namespaces_are_checked_and_old_settings_have_their_own() {
        use crate::network::NetworkMode;
        let json = serde_json::to_value(spec_fixture()).unwrap();
        assert!(json.get("namespaces").is_none(), "{json}");
        let spec: ContainerSpec = serde_json::from_value(json).unwrap();
        assert!(spec.namespaces.is_own());
        assert!(!spec.own_netns(), "the fixture uses the host's network");

        let mut s = spec_fixture();
        s.network = NetworkMode::None;
        assert!(s.own_netns());
        s.namespaces.net = Namespace::Join {
            path: "/run/x/net".into(),
        };
        s.namespaces.uts = Namespace::Host;
        s.validate().unwrap();
        assert!(!s.own_netns());
        let back: ContainerSpec =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.namespaces, s.namespaces);

        let mut bad = s.clone();
        bad.network = NetworkMode::default();
        assert_eq!(bad.validate().unwrap_err().kind, ErrorKind::InvalidInput);
        let mut bad = s.clone();
        bad.namespaces.net = Namespace::Host;
        assert_eq!(bad.validate().unwrap_err().kind, ErrorKind::InvalidInput);
        let mut bad = s.clone();
        bad.namespaces.ipc = Namespace::Join {
            path: "relative".into(),
        };
        assert_eq!(bad.validate().unwrap_err().kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn settings_without_a_restart_policy_load() {
        let mut json = serde_json::to_value(spec_fixture()).unwrap();
        json.as_object_mut().unwrap().remove("restart");
        let spec: ContainerSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.restart, crate::restart::RestartPolicy::No);
    }

    #[test]
    fn settings_without_privileged_load() {
        let mut json = serde_json::to_value(spec_fixture()).unwrap();
        let security = json["security"].as_object_mut().unwrap();
        security.remove("privileged");
        let spec: ContainerSpec = serde_json::from_value(json).unwrap();
        assert!(!spec.security.privileged);
        assert!(spec.security.seccomp, "{:?}", spec.security);
    }

    #[test]
    fn settings_without_further_networks_load() {
        let mut json = serde_json::to_value(spec_fixture()).unwrap();
        let map = json.as_object_mut().unwrap();
        map.remove("aliases");
        map.remove("extra_networks");
        let spec: ContainerSpec = serde_json::from_value(json).unwrap();
        assert!(spec.aliases.is_empty() && spec.extra_networks.is_empty());
    }
    use crate::ErrorKind;

    #[test]
    fn security_opt_parsing() {
        let p = |s: &str| parse_security_opt(s).unwrap();
        assert_eq!(p("no-new-privileges"), SecurityOpt::NoNewPrivileges(true));
        assert_eq!(
            p("no-new-privileges=false"),
            SecurityOpt::NoNewPrivileges(false)
        );
        assert_eq!(
            p("no-new-privileges:true"),
            SecurityOpt::NoNewPrivileges(true)
        );
        assert_eq!(p("seccomp=unconfined"), SecurityOpt::SeccompUnconfined);
        for bad in [
            "apparmor=x",
            "seccomp=/tmp/p.json",
            "no-new-privileges=maybe",
        ] {
            assert_eq!(
                parse_security_opt(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad}"
            );
        }
    }

    #[test]
    fn unknown_capability_names_are_invalid_settings() {
        let mut s = spec_fixture();
        s.security.cap_add = vec!["NOPE".into()];
        assert_eq!(s.validate().unwrap_err().kind, ErrorKind::InvalidInput);
    }

    fn spec() -> ContainerSpec {
        spec_fixture()
    }

    #[test]
    fn labels_default_to_empty_and_round_trip() {
        let mut s = spec();
        assert!(s.labels.is_empty());
        let mut json = serde_json::to_value(&s).unwrap();
        json.as_object_mut().unwrap().remove("labels");
        let back: ContainerSpec = serde_json::from_value(json).unwrap();
        assert!(back.labels.is_empty(), "an old config has no labels");
        s.labels.insert("com.example.role".into(), "web".into());
        let back: ContainerSpec =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.labels["com.example.role"], "web");
    }

    #[test]
    fn effective_env_uses_the_given_home_only_when_unset() {
        let mut s = spec();
        assert!(
            s.effective_env("/home/app")
                .contains(&("HOME".to_string(), "/home/app".to_string()))
        );
        s.env = vec![("HOME".into(), "/custom".into())];
        let env = s.effective_env("/home/app");
        assert!(env.contains(&("HOME".to_string(), "/custom".to_string())));
        assert_eq!(env.iter().filter(|(k, _)| k == "HOME").count(), 1);
    }

    #[test]
    fn overlay_paths_must_not_contain_separators() {
        let mut s = spec();
        s.rootfs = Rootfs::Overlay {
            lower: "/a,b".into(),
            upper: "/u".into(),
            work: "/w".into(),
            merged: "/m".into(),
            data: None,
        };
        let e = s.validate().unwrap_err();
        assert!(e.why.contains("',' or ':'"), "{}", e.why);
        s.rootfs = Rootfs::Overlay {
            lower: "/l".into(),
            upper: "/u".into(),
            work: "/w".into(),
            merged: "/m".into(),
            data: None,
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn the_data_layer_path_is_checked_like_the_others() {
        let mut s = spec();
        let with_data = |data: &str| Rootfs::Overlay {
            lower: "/r/images/ab".into(),
            upper: "/u".into(),
            work: "/w".into(),
            merged: "/m".into(),
            data: Some(data.into()),
        };
        s.rootfs = with_data("objects");
        assert!(s.validate().unwrap_err().why.contains("not absolute"));
        s.rootfs = with_data("/s/ob:jects");
        assert!(s.validate().unwrap_err().why.contains("',' or ':'"));
        s.rootfs = with_data("/s/objects");
        assert!(s.validate().is_ok());
    }

    #[test]
    fn effective_env_adds_defaults() {
        assert_eq!(
            spec().effective_env("/root"),
            vec![
                ("PATH".to_string(), DEFAULT_PATH.to_string()),
                ("HOSTNAME".to_string(), "box".to_string()),
                ("HOME".to_string(), "/root".to_string()),
            ]
        );
    }

    #[test]
    fn effective_env_lets_the_user_and_the_last_value_win() {
        let mut s = spec();
        s.env = vec![
            ("PATH".into(), "/bin".into()),
            ("A".into(), "1".into()),
            ("A".into(), "2".into()),
        ];
        let env = s.effective_env("/root");
        assert_eq!(env.iter().filter(|(k, _)| k == "PATH").count(), 1);
        assert!(env.contains(&("PATH".to_string(), "/bin".to_string())));
        assert!(env.contains(&("A".to_string(), "2".to_string())));
        assert!(!env.contains(&("A".to_string(), "1".to_string())));
    }

    #[test]
    fn validate_rejects_bad_specs() {
        let mut s = spec();
        s.command.clear();
        assert_eq!(s.validate().unwrap_err().kind, ErrorKind::InvalidInput);
        let mut s = spec();
        s.rootfs = Rootfs::Dir {
            path: "relative/dir".into(),
        };
        assert_eq!(s.validate().unwrap_err().kind, ErrorKind::InvalidInput);
        let mut s = spec();
        s.hostname = String::new();
        assert_eq!(s.validate().unwrap_err().kind, ErrorKind::InvalidInput);
        let mut s = spec();
        s.workdir = Some("not/absolute".into());
        assert_eq!(s.validate().unwrap_err().kind, ErrorKind::InvalidInput);
        assert!(spec().validate().is_ok());
    }
}
