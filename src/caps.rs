//! Linux capabilities by name, and the set a container keeps (Docker's defaults,
//! changed by `--cap-drop` and then `--cap-add`). The logic does not depend on the OS.

use crate::{Error, ErrorKind, Result};

/// Capability names in number order (linux/capability.h), without `CAP_`.
const NAMES: [&str; 41] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "KILL",
    "SETGID",
    "SETUID",
    "SETPCAP",
    "LINUX_IMMUTABLE",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_ADMIN",
    "NET_RAW",
    "IPC_LOCK",
    "IPC_OWNER",
    "SYS_MODULE",
    "SYS_RAWIO",
    "SYS_CHROOT",
    "SYS_PTRACE",
    "SYS_PACCT",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_NICE",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "MKNOD",
    "LEASE",
    "AUDIT_WRITE",
    "AUDIT_CONTROL",
    "SETFCAP",
    "MAC_OVERRIDE",
    "MAC_ADMIN",
    "SYSLOG",
    "WAKE_ALARM",
    "BLOCK_SUSPEND",
    "AUDIT_READ",
    "PERFMON",
    "BPF",
    "CHECKPOINT_RESTORE",
];

/// The highest capability number openpod knows (CHECKPOINT_RESTORE).
pub const LAST: u32 = NAMES.len() as u32 - 1;

/// Docker's default set: CHOWN, DAC_OVERRIDE, FSETID, FOWNER, MKNOD, NET_RAW, SETGID,
/// SETUID, SETFCAP, SETPCAP, NET_BIND_SERVICE, SYS_CHROOT, KILL, AUDIT_WRITE.
pub const DEFAULT: u64 = 0xa80425fb;

/// The number of a capability name: any case, with or without `CAP_`.
pub fn number(name: &str) -> Result<u32> {
    let upper = name.trim().to_ascii_uppercase();
    let bare = upper.strip_prefix("CAP_").unwrap_or(&upper);
    NAMES
        .iter()
        .position(|n| *n == bare)
        .map(|i| i as u32)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("unknown capability {name:?}"),
                "it is not a Linux capability name",
                "use a name like NET_ADMIN or CAP_NET_ADMIN, or ALL",
            )
        })
}

/// The capabilities a container keeps: Docker's defaults, minus `drop`, plus `add`.
/// `ALL` in `drop` starts from none; `ALL` in `add` adds every capability.
pub fn effective(add: &[String], drop: &[String]) -> Result<u64> {
    let all = (1u64 << (LAST + 1)) - 1;
    let is_all = |n: &String| n.trim().eq_ignore_ascii_case("ALL");
    let mut set = DEFAULT;
    for name in drop {
        set &= if is_all(name) {
            0
        } else {
            !(1u64 << number(name)?)
        };
    }
    for name in add {
        set |= if is_all(name) {
            all
        } else {
            1u64 << number(name)?
        };
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_caps_are_dockers_14() {
        assert_eq!(effective(&[], &[]).unwrap(), 0xa80425fb);
        assert_eq!(DEFAULT.count_ones(), 14);
    }

    #[test]
    fn cap_names_accept_prefix_and_case() {
        assert_eq!(number("net_raw").unwrap(), 13);
        assert_eq!(number("CAP_NET_RAW").unwrap(), 13);
        assert_eq!(number("cap_sys_admin").unwrap(), 21);
        assert_eq!(number("CHECKPOINT_RESTORE").unwrap(), LAST);
    }

    #[test]
    fn drop_all_then_add() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(effective(&v(&["NET_RAW"]), &v(&["ALL"])).unwrap(), 1 << 13);
        assert_eq!(
            effective(&v(&["ALL"]), &[]).unwrap(),
            (1u64 << (LAST + 1)) - 1
        );
        assert_eq!(effective(&[], &v(&["CHOWN"])).unwrap(), 0xa80425fb & !1);
        assert_eq!(
            effective(&v(&["SYS_ADMIN"]), &[]).unwrap(),
            0xa80425fb | 1 << 21
        );
    }

    #[test]
    fn unknown_cap_is_rejected() {
        assert_eq!(number("NOPE").unwrap_err().kind, ErrorKind::InvalidInput);
        assert_eq!(
            effective(&["NOPE".to_string()], &[]).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
    }
}
