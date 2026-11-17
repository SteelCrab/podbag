//! Resolving a container user (`app`, `101`, `app:staff`, …) against the image's
//! /etc/passwd and /etc/group, the way Docker does.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::{Error, ErrorKind, Result};

/// Who the container command runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups, sorted.
    pub groups: Vec<u32>,
    pub home: String,
}

struct PasswdEntry<'a> {
    name: &'a str,
    uid: u32,
    gid: u32,
    home: &'a str,
}

/// Resolve `user` (`name`, `uid`, `name:group`, `uid:gid`, …). None or "" means root.
/// A numeric uid that /etc/passwd does not list gets gid 0 and home `/`, like Docker.
pub fn resolve_user(user: Option<&str>, passwd: &str, group: &str) -> Result<Credentials> {
    let spec = user.filter(|u| !u.is_empty()).unwrap_or("0");
    let (user_part, group_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let users: Vec<PasswdEntry> = passwd.lines().filter_map(parse_passwd).collect();

    let entry = match user_part.parse::<u32>() {
        Ok(uid) => users
            .into_iter()
            .find(|e| e.uid == uid)
            .map(Ok)
            .unwrap_or(Err(uid)),
        Err(_) => match users.into_iter().find(|e| e.name == user_part) {
            Some(e) => Ok(e),
            None => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("cannot find user {user_part} in the container"),
                    format!("/etc/passwd in the image has no user {user_part}"),
                    "use a numeric uid, or a user that exists in the image",
                ));
            }
        },
    };
    let (uid, passwd_gid, name, home) = match entry {
        Ok(e) => (e.uid, Some(e.gid), Some(e.name), e.home.to_string()),
        Err(uid) => (
            uid,
            None,
            None,
            if uid == 0 {
                "/root".to_string()
            } else {
                "/".to_string()
            },
        ),
    };
    let mut gid = passwd_gid.unwrap_or(0);

    let groups_table: Vec<(&str, u32, Vec<&str>)> = group.lines().filter_map(parse_group).collect();
    if let Some(g) = group_part {
        gid = match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => groups_table
                .iter()
                .find(|(n, _, _)| *n == g)
                .map(|(_, id, _)| *id)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!("cannot find group {g} in the container"),
                        format!("/etc/group in the image has no group {g}"),
                        "use a numeric gid, or a group that exists in the image",
                    )
                })?,
        };
    }

    // Supplementary groups: the groups that list the user, plus the user's passwd group.
    let mut groups: Vec<u32> = match name {
        Some(n) => groups_table
            .iter()
            .filter(|(_, _, members)| members.contains(&n))
            .map(|(_, id, _)| *id)
            .collect(),
        None => Vec::new(),
    };
    groups.extend(passwd_gid);
    groups.sort_unstable();
    groups.dedup();
    Ok(Credentials {
        uid,
        gid,
        groups,
        home,
    })
}

fn parse_passwd(line: &str) -> Option<PasswdEntry<'_>> {
    let f: Vec<&str> = line.split(':').collect();
    if f.len() < 6 {
        return None;
    }
    Some(PasswdEntry {
        name: f[0],
        uid: f[2].parse().ok()?,
        gid: f[3].parse().ok()?,
        home: f[5],
    })
}

fn parse_group(line: &str) -> Option<(&str, u32, Vec<&str>)> {
    let f: Vec<&str> = line.split(':').collect();
    if f.len() < 3 {
        return None;
    }
    let members = f
        .get(3)
        .map(|m| m.split(',').filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    Some((f[0], f[2].parse().ok()?, members))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\napp:x:101:102::/home/app:/bin/sh\n";
    const GROUP: &str = "root:x:0:root\nwheel:x:10:root,app\nstaff:x:50:\napp:x:102:\n";

    fn r(user: Option<&str>) -> Credentials {
        resolve_user(user, PASSWD, GROUP).unwrap()
    }

    #[test]
    fn resolve_user_formats() {
        assert_eq!(
            r(None),
            Credentials {
                uid: 0,
                gid: 0,
                groups: vec![0, 10],
                home: "/root".into()
            }
        );
        assert_eq!(r(Some("")).uid, 0);
        assert_eq!(
            r(Some("app")),
            Credentials {
                uid: 101,
                gid: 102,
                groups: vec![10, 102],
                home: "/home/app".into()
            }
        );
        assert_eq!(r(Some("101")).gid, 102);
        assert_eq!(r(Some("app:staff")).gid, 50);
        assert_eq!(r(Some("app:7")).gid, 7);
        assert_eq!(r(Some("101:staff")).gid, 50);
        assert_eq!(
            r(Some("101:7")),
            Credentials {
                uid: 101,
                gid: 7,
                groups: vec![10, 102],
                home: "/home/app".into()
            }
        );
    }

    #[test]
    fn numeric_user_without_an_entry_gets_gid_0() {
        assert_eq!(
            r(Some("4242")),
            Credentials {
                uid: 4242,
                gid: 0,
                groups: vec![],
                home: "/".into()
            }
        );
    }

    #[test]
    fn unknown_names_are_errors() {
        let e = resolve_user(Some("ghost"), PASSWD, GROUP).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert_eq!(e.what, "cannot find user ghost in the container");
        let e = resolve_user(Some("app:nogroup"), PASSWD, GROUP).unwrap_err();
        assert_eq!(e.what, "cannot find group nogroup in the container");
    }

    #[test]
    fn missing_files_still_allow_numeric_users() {
        assert_eq!(
            resolve_user(Some("1000:1000"), "", "").unwrap(),
            Credentials {
                uid: 1000,
                gid: 1000,
                groups: vec![],
                home: "/".into()
            }
        );
        assert_eq!(
            resolve_user(None, "", "").unwrap(),
            Credentials {
                uid: 0,
                gid: 0,
                groups: vec![],
                home: "/root".into()
            }
        );
    }
}
