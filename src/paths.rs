//! Where podbag keeps its files.

use crate::{Error, ErrorKind, Result};
use std::path::PathBuf;

/// The three directories podbag uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// Data that must survive a reboot: containers, images, volumes.
    pub data: PathBuf,
    /// Data that a reboot may delete: locks, sockets, pids.
    pub runtime: PathBuf,
    /// Settings.
    pub config: PathBuf,
}

impl Dirs {
    pub fn container_dir(&self, id: &str) -> PathBuf {
        self.data.join("containers").join(id)
    }

    pub fn volume_dir(&self, name: &str) -> PathBuf {
        self.data.join("volumes").join(name)
    }
}

/// The directories for the current process.
pub fn dirs() -> Result<Dirs> {
    let euid = effective_uid();
    resolve_dirs(euid == 0, euid, |k| std::env::var(k).ok())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn effective_uid() -> u32 {
    u32::MAX
}

/// Decide the directories from the user and the environment.
pub fn resolve_dirs(is_root: bool, uid: u32, var: impl Fn(&str) -> Option<String>) -> Result<Dirs> {
    let get = |k: &str| var(k).filter(|v| !v.is_empty());
    let pick = |override_var: &str, default: Result<PathBuf>| -> Result<PathBuf> {
        match get(override_var) {
            Some(v) => Ok(PathBuf::from(v)),
            None => default,
        }
    };

    if is_root {
        return Ok(Dirs {
            data: pick("PODBAG_DATA_DIR", Ok("/var/lib/podbag".into()))?,
            runtime: pick("PODBAG_RUNTIME_DIR", Ok("/run/podbag".into()))?,
            config: pick("PODBAG_CONFIG_DIR", Ok("/etc/podbag".into()))?,
        });
    }

    let home = get("HOME").map(PathBuf::from);
    let under_xdg_or_home = |xdg: &str, home_rel: &str, which: &str| -> Result<PathBuf> {
        if let Some(base) = get(xdg) {
            return Ok(PathBuf::from(base).join("podbag"));
        }
        match &home {
            Some(h) => Ok(h.join(home_rel).join("podbag")),
            None => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("cannot find the podbag {which} directory"),
                format!("neither {xdg} nor HOME is set"),
                format!(
                    "set HOME, or set PODBAG_{}_DIR to a writable directory",
                    which.to_uppercase()
                ),
            )),
        }
    };

    Ok(Dirs {
        data: pick(
            "PODBAG_DATA_DIR",
            under_xdg_or_home("XDG_DATA_HOME", ".local/share", "data"),
        )?,
        runtime: pick(
            "PODBAG_RUNTIME_DIR",
            Ok(match get("XDG_RUNTIME_DIR") {
                Some(base) => PathBuf::from(base).join("podbag"),
                None => PathBuf::from(format!("/tmp/podbag-{uid}")),
            }),
        )?,
        config: pick(
            "PODBAG_CONFIG_DIR",
            under_xdg_or_home("XDG_CONFIG_HOME", ".config", "config"),
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::path::PathBuf;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn rootless_uses_xdg_directories() {
        let d = resolve_dirs(
            false,
            1000,
            vars(&[
                ("XDG_DATA_HOME", "/x/data"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("XDG_CONFIG_HOME", "/x/cfg"),
            ]),
        )
        .unwrap();
        assert_eq!(d.data, PathBuf::from("/x/data/podbag"));
        assert_eq!(d.runtime, PathBuf::from("/run/user/1000/podbag"));
        assert_eq!(d.config, PathBuf::from("/x/cfg/podbag"));
        assert_eq!(
            d.container_dir("abc"),
            PathBuf::from("/x/data/podbag/containers/abc")
        );
    }

    #[test]
    fn rootless_falls_back_to_home_and_tmp() {
        let d = resolve_dirs(false, 1000, vars(&[("HOME", "/home/u")])).unwrap();
        assert_eq!(d.data, PathBuf::from("/home/u/.local/share/podbag"));
        assert_eq!(d.runtime, PathBuf::from("/tmp/podbag-1000"));
        assert_eq!(d.config, PathBuf::from("/home/u/.config/podbag"));
    }

    #[test]
    fn root_uses_system_directories() {
        let d = resolve_dirs(true, 0, vars(&[("HOME", "/root")])).unwrap();
        assert_eq!(d.data, PathBuf::from("/var/lib/podbag"));
        assert_eq!(d.runtime, PathBuf::from("/run/podbag"));
        assert_eq!(d.config, PathBuf::from("/etc/podbag"));
    }

    #[test]
    fn overrides_apply_per_directory_in_both_modes() {
        let d = resolve_dirs(
            true,
            0,
            vars(&[("PODBAG_DATA_DIR", "/d"), ("PODBAG_RUNTIME_DIR", "")]),
        )
        .unwrap();
        assert_eq!(d.data, PathBuf::from("/d"));
        assert_eq!(d.runtime, PathBuf::from("/run/podbag"));
    }

    #[test]
    fn rootless_without_home_explains_how_to_fix() {
        let e = resolve_dirs(false, 1000, vars(&[])).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert_eq!(e.what, "cannot find the podbag data directory");
        assert!(e.next.contains("PODBAG_DATA_DIR"));
    }
}
