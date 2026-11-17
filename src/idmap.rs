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

/// Write the UID and GID maps of process `pid` with newuidmap and newgidmap.
pub fn apply_id_maps(pid: u32, uid: u32, gid: u32) -> Result<()> {
    let user = std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|p| user_name(&p, uid))
        .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| uid.to_string());

    let newuidmap = find_in_path("newuidmap")?;
    let newgidmap = find_in_path("newgidmap")?;
    let uid_range = read_range(SUBUID, &user, uid)?;
    let gid_range = read_range(SUBGID, &user, uid)?;

    run_tool(&newuidmap, &newidmap_args(pid, uid, uid_range))?;
    run_tool(&newgidmap, &newidmap_args(pid, gid, gid_range))
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

fn run_tool(tool: &PathBuf, args: &[String]) -> Result<()> {
    let out = std::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
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
