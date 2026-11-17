//! The container settings that the helper process receives.

use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

/// The PATH a container gets when the caller does not set one. Same as Docker.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub id: String,
    /// Absolute host path of the root filesystem.
    pub rootfs: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub hostname: String,
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
        if !self.rootfs.starts_with('/') {
            return invalid(
                &format!("the root filesystem path {} is not absolute", self.rootfs),
                "pass an absolute path",
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
    /// Each name appears once; the last value given wins.
    pub fn effective_env(&self) -> Vec<(String, String)> {
        let defaults = [
            ("PATH".to_string(), DEFAULT_PATH.to_string()),
            ("HOSTNAME".to_string(), self.hostname.clone()),
            ("HOME".to_string(), "/root".to_string()),
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
        rootfs: "/r".into(),
        command: vec!["sh".into()],
        env: vec![],
        workdir: None,
        hostname: "box".into(),
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
    fn effective_env_adds_defaults() {
        assert_eq!(
            spec().effective_env(),
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
        let env = s.effective_env();
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
        s.rootfs = "relative/dir".into();
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
