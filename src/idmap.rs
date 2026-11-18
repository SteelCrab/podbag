//! User and group ID mapping for rootless containers.

use crate::{Error, ErrorKind, Result};
use std::path::PathBuf;

const SUBUID: &str = "/etc/subuid";
const SUBGID: &str = "/etc/subgid";
const WHAT: &str = "cannot map user IDs for the container";

/// A range of subordinate IDs from /etc/subuid or /etc/subgid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubIdRange {
    pub start: u32,
    pub count: u32,
}

/// Find the first `name:start:count` line for this user, by name or by numeric ID.
pub fn parse_subid(content: &str, user: &str, uid: u32) -> Option<SubIdRange> {
    let uid = uid.to_string();
    content.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut parts = line.split(':');
        let (name, start, count) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || (name != user && name != uid) {
            return None;
        }
        Some(SubIdRange {
            start: start.parse().ok()?,
            count: count.parse().ok()?,
        })
    })
}

/// The user name for a numeric ID in /etc/passwd content.
pub fn user_name(passwd: &str, uid: u32) -> Option<String> {
    let uid = uid.to_string();
    passwd.lines().find_map(|line| {
        let mut parts = line.split(':');
        let name = parts.next()?;
        let _password = parts.next()?;
        (parts.next()? == uid).then(|| name.to_string())
    })
}

/// Arguments for newuidmap/newgidmap: container ID 0 is the caller, IDs from 1 are the range.
pub fn newidmap_args(pid: u32, host_id: u32, range: SubIdRange) -> Vec<String> {
    [pid, 0, host_id, 1, 1, range.start, range.count]
        .iter()
        .map(u32::to_string)
        .collect()
}

/// The name looked up in /etc/subuid and /etc/subgid: the /etc/passwd name, else `$USER`
/// (directory users such as LDAP are not in /etc/passwd), else the numeric ID.
pub fn current_user(passwd: Option<&str>, env_user: Option<&str>, uid: u32) -> String {
    passwd
        .and_then(|p| user_name(p, uid))
        .or_else(|| env_user.filter(|u| !u.is_empty()).map(String::from))
        .unwrap_or_else(|| uid.to_string())
}

/// `current_user` for this process.
pub fn this_user(uid: u32) -> String {
    current_user(
        std::fs::read_to_string("/etc/passwd").ok().as_deref(),
        std::env::var("USER").ok().as_deref(),
        uid,
    )
}

/// Write the UID and GID maps of process `pid` with newuidmap and newgidmap.
pub fn apply_id_maps(pid: u32, uid: u32, gid: u32) -> Result<()> {
    let user = this_user(uid);

    let newuidmap = find_in_path("newuidmap")?;
    let newgidmap = find_in_path("newgidmap")?;
    let uid_range = read_range(SUBUID, &user, uid)?;
    let gid_range = read_range(SUBGID, &user, uid)?;

    // The two maps do not depend on each other: both tools run at the same time (each
    // is a setuid program that takes about a millisecond to start).
    let uids = start_tool(&newuidmap, &newidmap_args(pid, uid, uid_range));
    let gids = start_tool(&newgidmap, &newidmap_args(pid, gid, gid_range));
    // Both are collected, also when the first failed: no child is left behind.
    let uids = uids.and_then(|child| end_tool(&newuidmap, child));
    let gids = gids.and_then(|child| end_tool(&newgidmap, child));
    uids.and(gids)
}

/// Check that newuidmap and newgidmap can be found, before work that would be wasted
/// without them.
pub fn require_tools() -> Result<()> {
    find_in_path("newuidmap")?;
    find_in_path("newgidmap").map(|_| ())
}

fn find_in_path(tool: &str) -> Result<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::MissingRequirement,
                WHAT,
                format!("{tool} was not found in PATH"),
                "install the uidmap package (Debian/Ubuntu) or shadow-utils (Fedora)",
            )
        })
}

fn read_range(file: &str, user: &str, uid: u32) -> Result<SubIdRange> {
    let content = std::fs::read_to_string(file).unwrap_or_default();
    parse_subid(&content, user, uid).ok_or_else(|| {
        Error::new(
            ErrorKind::MissingRequirement,
            WHAT,
            format!("{file} has no entry for user {user}"),
            format!("sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user}"),
        )
    })
}

fn start_tool(tool: &PathBuf, args: &[String]) -> Result<std::process::Child> {
    std::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            Error::io(
                WHAT,
                &e,
                format!("check that {} can be run", tool.display()),
            )
        })
}

fn end_tool(tool: &PathBuf, child: std::process::Child) -> Result<()> {
    let out = child.wait_with_output().map_err(|e| {
        Error::io(
            WHAT,
            &e,
            format!("check that {} can be run", tool.display()),
        )
    })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let why = if stderr.is_empty() {
        format!("{} exited with {}", tool.display(), out.status)
    } else {
        stderr
    };
    Err(Error::new(
        ErrorKind::Runtime,
        WHAT,
        why,
        "check the ranges for your user in /etc/subuid and /etc/subgid; they must not overlap other ranges",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_comes_from_passwd_then_user_then_the_id() {
        let passwd = "root:x:0:0::/root:/bin/sh\nana:x:1000:1000::/home/ana:/bin/sh\n";
        assert_eq!(current_user(Some(passwd), Some("other"), 1000), "ana");
        // Directory users (LDAP, SSSD) are not in /etc/passwd.
        assert_eq!(
            current_user(Some(passwd), Some("ldapuser"), 5000),
            "ldapuser"
        );
        assert_eq!(current_user(None, Some(""), 5000), "5000");
        assert_eq!(current_user(None, None, 5000), "5000");
    }

    const SUBUID: &str = "# comment\n\nroot:100000:65536\nbroken-line\npista:165536:65536\n1001:300000:1000\npista:900000:10\n";

    #[test]
    fn parse_subid_matches_by_name_and_takes_the_first_entry() {
        assert_eq!(
            parse_subid(SUBUID, "pista", 1000),
            Some(SubIdRange {
                start: 165536,
                count: 65536
            })
        );
    }

    #[test]
    fn parse_subid_matches_by_numeric_uid() {
        assert_eq!(
            parse_subid(SUBUID, "someone", 1001),
            Some(SubIdRange {
                start: 300000,
                count: 1000
            })
        );
    }

    #[test]
    fn parse_subid_returns_none_without_an_entry() {
        assert_eq!(parse_subid(SUBUID, "nobody", 4242), None);
        assert_eq!(parse_subid("pista:abc:10\npista:1:\n", "pista", 1000), None);
    }

    #[test]
    fn user_name_reads_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\npista:x:1000:1000::/home/pista:/bin/bash\n";
        assert_eq!(user_name(passwd, 1000), Some("pista".to_string()));
        assert_eq!(user_name(passwd, 7), None);
    }

    #[test]
    fn newidmap_args_map_root_to_the_caller_and_the_rest_to_the_range() {
        assert_eq!(
            newidmap_args(
                4321,
                1000,
                SubIdRange {
                    start: 165536,
                    count: 65536
                }
            ),
            ["4321", "0", "1000", "1", "1", "165536", "65536"]
        );
    }
}
