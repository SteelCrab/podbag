//! Linux container runtime: namespaces, ID mapping, root filesystem, exec.
//!
//! Everything here runs inside the shim helper process, never in the library caller.

pub mod archive;
pub mod exec;
mod init;
mod mounts;
pub mod net;
pub mod pty;
mod rootfs;
mod security;
pub mod session;
mod spawn;
pub mod userns;

pub(crate) use rootfs::overlay_options;
pub use spawn::{ContainerStdio, Parent, UserNs, spawn, wait};

use crate::{Error, ErrorKind};

/// Explain a rootless setup failure that AppArmor causes.
///
/// On Ubuntu 24.04+, a program without an AppArmor profile that creates a user namespace
/// is moved to the `unprivileged_userns` profile, which denies the capabilities that
/// container setup needs. `os_error` is the failed system call's error, or `None` when a
/// helper tool such as newuidmap failed. `pid` names the process whose label to check;
/// `None` means this process.
pub(crate) fn userns_denial(
    rootless: bool,
    os_error: Option<&std::io::Error>,
    pid: Option<u32>,
) -> Option<Error> {
    let errno = os_error.map(|e| e.raw_os_error().unwrap_or(0));
    if !rootless || !is_apparmor_userns_denial(rootless, errno, &apparmor_label(pid)) {
        return None;
    }
    Some(Error::new(
        ErrorKind::MissingRequirement,
        "cannot create an unprivileged user namespace",
        "AppArmor restricts user namespaces for programs without a profile \
         (kernel.apparmor_restrict_unprivileged_userns=1)",
        "install an AppArmor profile for podbag that allows userns (see README, \"AppArmor\"), then run again",
    ))
}

/// True when the failure is a permission error (or a failed helper tool) in rootless mode
/// and the process runs under AppArmor's `unprivileged_userns` profile.
pub(crate) fn is_apparmor_userns_denial(rootless: bool, errno: Option<i32>, label: &str) -> bool {
    let permission = errno.is_none_or(|e| e == libc::EPERM || e == libc::EACCES);
    rootless && permission && label.split_whitespace().next() == Some("unprivileged_userns")
}

/// The AppArmor label of a process, or "" when AppArmor is not in use.
pub(crate) fn apparmor_label(pid: Option<u32>) -> String {
    let base = pid.map_or_else(|| "/proc/self".to_string(), |p| format!("/proc/{p}"));
    ["attr/apparmor/current", "attr/current"]
        .iter()
        .find_map(|f| std::fs::read_to_string(format!("{base}/{f}")).ok())
        .map(|s| s.trim_end_matches(['\0', '\n']).to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DENIED: &str = "unprivileged_userns (enforce)";

    #[test]
    fn denial_needs_the_unprivileged_userns_label() {
        assert!(is_apparmor_userns_denial(true, Some(libc::EPERM), DENIED));
        assert!(is_apparmor_userns_denial(true, Some(libc::EACCES), DENIED));
        assert!(is_apparmor_userns_denial(true, None, DENIED));
    }

    #[test]
    fn an_installed_profile_is_not_blamed() {
        // With the podbag profile, the process runs under that profile instead.
        assert!(!is_apparmor_userns_denial(
            true,
            Some(libc::EPERM),
            "podbag-dev (unconfined)"
        ));
        assert!(!is_apparmor_userns_denial(
            true,
            Some(libc::EPERM),
            "unconfined"
        ));
        assert!(!is_apparmor_userns_denial(true, None, ""));
    }

    #[test]
    fn other_errors_and_root_mode_are_not_blamed() {
        assert!(!is_apparmor_userns_denial(true, Some(libc::ENOENT), DENIED));
        assert!(!is_apparmor_userns_denial(false, Some(libc::EPERM), DENIED));
    }
}
