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
}

impl Default for Security {
    fn default() -> Self {
        Security {
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            no_new_privileges: false,
            seccomp: true,
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
        _ => Err(bad("openpod does not know this option")),
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
                "the openpod data directory path contains ',' or ':', which overlayfs cannot use",
                "set OPENPOD_DATA_DIR to a path without them",
            );
        }
        crate::caps::effective(&self.security.cap_add, &self.security.cap_drop)?;
        for m in &self.mounts {
            crate::volume::check_target(&m.target)?;
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
        Ok(())
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
