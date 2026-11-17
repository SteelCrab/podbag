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
            } => vec![lower, upper, work, merged],
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn spec() -> ContainerSpec {
        spec_fixture()
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
        };
        let e = s.validate().unwrap_err();
        assert!(e.why.contains("',' or ':'"), "{}", e.why);
        s.rootfs = Rootfs::Overlay {
            lower: "/l".into(),
            upper: "/u".into(),
            work: "/w".into(),
            merged: "/m".into(),
        };
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
