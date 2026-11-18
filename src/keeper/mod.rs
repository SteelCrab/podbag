//! The keeper: one process per runtime directory that starts containers for clients and
//! watches the detached ones, so that a container needs no process of its own (design 6.15).

#[cfg(target_os = "linux")]
pub mod cgroups;
#[cfg(target_os = "linux")]
pub mod client;
#[cfg(target_os = "linux")]
pub mod handover;
#[cfg(target_os = "linux")]
mod host;
pub mod proto;

#[cfg(target_os = "linux")]
pub use host::keeper_main;

use std::sync::atomic::{AtomicBool, Ordering};

/// The keeper's socket and lock in the runtime directory.
pub const SOCKET: &str = "keeper.sock";
pub const LOCK: &str = "keeper.lock";

/// Set in a starter: a fork of the keeper that runs the shim code for one container.
static UNDER_KEEPER: AtomicBool = AtomicBool::new(false);

/// Whether this process is a starter that the keeper forked.
pub fn under_keeper() -> bool {
    UNDER_KEEPER.load(Ordering::Relaxed)
}

/// Whether containers start through the keeper: the `keeper` setting, or `OPENPOD_KEEPER`.
/// Unreadable settings count as "no": the shim path reports them.
pub fn enabled(dirs: &crate::paths::Dirs) -> bool {
    crate::config::load(dirs).is_ok_and(|c| c.keeper)
}

/// Variables a starter or a finisher needs besides the `OPENPOD_` ones: where the user's
/// directories and programs are, and how to reach the user's service manager.
#[cfg(target_os = "linux")]
const KEPT: [&str; 8] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// This process's settings for openpod: what a starter or a finisher runs with. The rest
/// of the environment (tokens, credentials) stays with its owner.
#[cfg(target_os = "linux")]
pub(crate) fn settings() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| k.starts_with("OPENPOD_") || KEPT.contains(&k.as_str()))
        .collect()
}

/// The user of the process at the other end of `conn`.
#[cfg(target_os = "linux")]
fn peer_uid(conn: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    // SAFETY: an all-zero ucred is valid; getsockopt writes at most `len` bytes into it.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the pointers name a ucred and its size.
    let rc = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut cred).cast(),
            &mut len,
        )
    };
    (rc == 0 && len as usize == std::mem::size_of::<libc::ucred>()).then_some(cred.uid)
}

/// Whether the other end of `conn` is a process of this user. The keeper and its clients
/// both ask: file modes alone do not say who listens at a path, or who connects.
#[cfg(target_os = "linux")]
pub(crate) fn peer_is_us(conn: &std::os::unix::net::UnixStream) -> bool {
    // SAFETY: geteuid has no preconditions.
    peer_uid(conn) == Some(unsafe { libc::geteuid() })
}

/// Whether `dir` may hold the keeper's socket: it belongs to this user and nobody else
/// can put files there. A directory that does not exist yet is made private by the keeper.
#[cfg(target_os = "linux")]
pub(crate) fn private_dir(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(dir) {
        // SAFETY: geteuid has no preconditions.
        Ok(m) => m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// What tells one build of openpod from another: the program file. The keeper serves only
/// clients whose helper program is the file it runs from itself: after an upgrade, new
/// containers must not be started by the old program's code.
#[cfg(target_os = "linux")]
pub(crate) fn build_of(program: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(program).ok()?;
    Some(format!(
        "{}:{:x}:{:x}:{:x}:{}.{}",
        env!("CARGO_PKG_VERSION"),
        m.dev(),
        m.ino(),
        m.size(),
        m.mtime(),
        m.mtime_nsec()
    ))
}

/// Call `f` with a path to `name` in `dir` that fits in a socket address (107 bytes): a
/// long directory is reached through an open descriptor.
#[cfg(target_os = "linux")]
fn with_socket_path<T>(
    dir: &std::path::Path,
    name: &str,
    f: impl FnOnce(&std::path::Path) -> std::io::Result<T>,
) -> std::io::Result<T> {
    use std::os::fd::AsRawFd;
    let plain = dir.join(name);
    if plain.as_os_str().len() <= 107 {
        return f(&plain);
    }
    let handle = std::fs::File::open(dir)?;
    let short = std::path::PathBuf::from(format!("/proc/self/fd/{}/{name}", handle.as_raw_fd()));
    f(&short)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn a_peer_of_this_user_is_us() {
        let (a, b) = UnixStream::pair().unwrap();
        assert!(peer_is_us(&a) && peer_is_us(&b));
    }

    #[test]
    #[ignore = "needs root to become another user: dev/vm.sh root-lib"]
    fn a_peer_of_another_user_is_not_us_as_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        }
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the child makes only system calls (no allocation) and leaves with _exit.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: as above; the address is a zeroed sockaddr_un with the path copied in.
            unsafe {
                if libc::setgid(65534) != 0 || libc::setuid(65534) != 0 {
                    libc::_exit(2);
                }
                let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                let mut addr: libc::sockaddr_un = std::mem::zeroed();
                addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
                let bytes = c_path.as_bytes_with_nul();
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr().cast(),
                    addr.sun_path.as_mut_ptr(),
                    bytes.len().min(addr.sun_path.len() - 1),
                );
                let rc = libc::connect(
                    fd,
                    std::ptr::from_ref(&addr).cast(),
                    std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
                );
                if rc != 0 {
                    libc::_exit(3);
                }
                libc::sleep(2);
                libc::_exit(0);
            }
        }
        let (conn, _) = listener.accept().unwrap();
        let ours = peer_is_us(&conn);
        let uid = peer_uid(&conn);
        // SAFETY: our own child.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        assert_eq!(uid, Some(65534));
        assert!(!ours);
    }

    #[test]
    fn a_directory_others_can_write_is_not_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |m: u32| {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(m)).unwrap();
        };
        mode(0o700);
        assert!(private_dir(dir.path()));
        mode(0o755);
        assert!(private_dir(dir.path()));
        mode(0o777);
        assert!(!private_dir(dir.path()));
        mode(0o770);
        assert!(!private_dir(dir.path()));
        mode(0o700);
        // Made by the keeper when it is missing; a file is no directory.
        assert!(private_dir(&dir.path().join("missing")));
        std::fs::write(dir.path().join("file"), b"").unwrap();
        assert!(!private_dir(&dir.path().join("file")));
    }

    #[test]
    fn two_files_are_two_builds() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"program").unwrap();
        std::fs::copy(&a, &b).unwrap();
        assert_eq!(build_of(&a), build_of(&a));
        assert_ne!(build_of(&a), build_of(&b));
        assert_eq!(build_of(&dir.path().join("none")), None);
    }

    #[test]
    fn settings_leave_the_rest_of_the_environment_out() {
        // Read-only on the environment: PATH is set wherever tests run.
        let s = settings();
        assert!(s.iter().any(|(k, _)| k == "PATH"));
        assert!(
            s.iter()
                .all(|(k, _)| k.starts_with("OPENPOD_") || KEPT.contains(&k.as_str()))
        );
    }
}
