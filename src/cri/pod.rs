//! A pod's namespaces without a pause container: the network, IPC and UTS namespaces are
//! made by a thread of their own and kept by bind mounts of their files; a shared PID
//! namespace is held by a tiny init (`podbag __pause`). The pod's /dev/shm is a tmpfs.

use std::path::{Path, PathBuf};

use crate::{Error, ErrorKind, Result};

/// What a pod holds, as paths: the namespace files and its /dev/shm.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held {
    pub net: Option<PathBuf>,
    pub ipc: Option<PathBuf>,
    pub uts: Option<PathBuf>,
    pub shm: Option<PathBuf>,
}

/// Which namespaces the pod gets of its own (the others are the host's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted {
    pub net: bool,
    pub ipc: bool,
    pub uts: bool,
}

/// The kernel settings that belong to a namespace the pod has of its own; `/` and `.`
/// both separate the parts of a name, as Kubernetes allows.
pub fn sysctl_path(name: &str, wanted: Wanted) -> Option<String> {
    let path = if name.contains('/') {
        name.to_string()
    } else {
        name.replace('.', "/")
    };
    if path.split('/').any(|p| p.is_empty() || p == "..") {
        return None;
    }
    let ipc = path.starts_with("kernel/shm")
        || path.starts_with("kernel/msg")
        || path == "kernel/sem"
        || path.starts_with("fs/mqueue/");
    let uts = path == "kernel/hostname" || path == "kernel/domainname";
    let own =
        (path.starts_with("net/") && wanted.net) || (ipc && wanted.ipc) || (uts && wanted.uts);
    own.then(|| format!("/proc/sys/{path}"))
}

/// Make the pod's namespaces in `dir` (created): a thread leaves the process's network,
/// IPC and UTS namespaces for new ones, sets them up and binds their files.
pub fn hold(
    dir: &Path,
    wanted: Wanted,
    hostname: &str,
    sysctls: &[(String, String)],
) -> Result<Held> {
    std::fs::create_dir_all(dir).map_err(|e| io(format!("cannot create {}", dir.display()), e))?;
    let mut held;
    let dir_owned = dir.to_path_buf();
    let hostname = hostname.to_string();
    let sysctls = sysctls.to_vec();
    let result = std::thread::spawn(move || -> Result<Held> {
        let dir = dir_owned;
        let mut flags = 0;
        if wanted.net {
            flags |= libc::CLONE_NEWNET;
        }
        if wanted.ipc {
            flags |= libc::CLONE_NEWIPC;
        }
        if wanted.uts {
            flags |= libc::CLONE_NEWUTS;
        }
        let mut held = Held::default();
        if flags == 0 {
            return Ok(held);
        }
        // SAFETY: unshare with namespace flags changes only this thread, which ends below.
        if unsafe { libc::unshare(flags) } != 0 {
            return Err(io(
                "cannot make the pod's namespaces".into(),
                std::io::Error::last_os_error(),
            ));
        }
        if wanted.uts && !hostname.is_empty() {
            rustix::system::sethostname(hostname.as_bytes())
                .map_err(|e| io(format!("cannot set the pod hostname {hostname}"), e.into()))?;
        }
        if wanted.net {
            loopback_up().map_err(|e| io("cannot bring up the pod's loopback".into(), e))?;
        }
        for (name, value) in &sysctls {
            let Some(path) = sysctl_path(name, wanted) else {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("cannot set the kernel setting {name}"),
                    "it does not belong to a namespace of the pod",
                    "set only namespaced settings (net.*, kernel.shm*, …)",
                ));
            };
            std::fs::write(&path, value)
                .map_err(|e| io(format!("cannot set {name} to {value}"), e))?;
        }
        for (want, kind, slot) in [
            (wanted.net, "net", &mut held.net),
            (wanted.ipc, "ipc", &mut held.ipc),
            (wanted.uts, "uts", &mut held.uts),
        ] {
            if want {
                let target = dir.join(kind);
                bind_ns(&format!("/proc/thread-self/ns/{kind}"), &target)?;
                *slot = Some(target);
            }
        }
        Ok(held)
    })
    .join()
    .unwrap_or_else(|_| {
        Err(Error::new(
            ErrorKind::Internal,
            "cannot make the pod's namespaces",
            "the thread that made them failed",
            "report this bug",
        ))
    });
    match result {
        Ok(h) => held = h,
        Err(e) => {
            release(dir);
            return Err(e);
        }
    }
    if wanted.ipc {
        let shm = dir.join("shm");
        if let Err(e) = mount_shm(&shm) {
            release(dir);
            return Err(e);
        }
        held.shm = Some(shm);
    }
    Ok(held)
}

/// Bind a namespace file onto `target` (an empty file made for it).
pub fn bind_ns(source: &str, target: &Path) -> Result<()> {
    std::fs::File::create(target)
        .map_err(|e| io(format!("cannot create {}", target.display()), e))?;
    rustix::mount::mount_bind(source, target).map_err(|e| {
        io(
            format!("cannot keep the namespace at {}", target.display()),
            e.into(),
        )
    })
}

fn mount_shm(target: &Path) -> Result<()> {
    use rustix::mount::MountFlags;
    std::fs::create_dir_all(target)
        .map_err(|e| io(format!("cannot create {}", target.display()), e))?;
    rustix::mount::mount(
        "shm",
        target,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        Some(c"mode=1777,size=65536k"),
    )
    .map_err(|e| {
        io(
            format!("cannot mount the pod's /dev/shm at {}", target.display()),
            e.into(),
        )
    })
}

/// Let go of everything a pod holds in `dir`, and the folder. What is gone already is
/// skipped.
pub fn release(dir: &Path) {
    for name in ["net", "ipc", "uts", "pid", "shm"] {
        let path = dir.join(name);
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        let _ = rustix::mount::unmount(&path, rustix::mount::UnmountFlags::DETACH);
        let _ = std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir(&path));
    }
    let _ = std::fs::remove_dir(dir);
}

/// Release only the network namespace (a stopped pod has no network).
pub fn release_net(dir: &Path) {
    let path = dir.join("net");
    if std::fs::symlink_metadata(&path).is_ok() {
        let _ = rustix::mount::unmount(&path, rustix::mount::UnmountFlags::DETACH);
        let _ = std::fs::remove_file(&path);
    }
}

fn loopback_up() -> std::io::Result<()> {
    let mut s = crate::netlink::Socket::route()?;
    if let Some(lo) = s.link_index("lo")? {
        s.request(crate::netlink::route::set_up(lo))?;
    }
    Ok(())
}

fn io(what: String, e: std::io::Error) -> Error {
    Error::io(what, &e, "check that `podbag cri serve` runs as root")
}

/// Start the init that holds a pod's PID namespace, and keep the namespace at `dir/pid`.
/// Returns its PID.
pub fn hold_pid(dir: &Path) -> Result<u32> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    let exe =
        std::env::current_exe().map_err(|e| io("cannot find the podbag program".into(), e))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__pause")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // SAFETY: unshare is async-signal-safe; it runs in the child before exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::unshare(libc::CLONE_NEWPID) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| io("cannot start the pod's PID namespace".into(), e))?;
    let mut out = String::new();
    let _ = child.stdout.take().map(|mut s| s.read_to_string(&mut out));
    let _ = child.wait();
    let pid: u32 = out.trim().parse().map_err(|_| {
        Error::new(
            ErrorKind::Runtime,
            "cannot start the pod's PID namespace",
            "its init did not report its PID",
            "report this bug",
        )
    })?;
    if let Err(e) = bind_ns(&format!("/proc/{pid}/ns/pid"), &dir.join("pid")) {
        stop_pid(pid);
        return Err(e);
    }
    Ok(pid)
}

/// End a pod's init (only when that PID is still a podbag init).
pub fn stop_pid(pid: u32) {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if cmdline.split(|&b| b == 0).any(|a| a == b"__pause") {
        // SAFETY: kill has no memory arguments.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// `podbag __pause`: fork the init of the new PID namespace (this process went into it
/// for its children), print its PID, and leave. The init reaps what ends below it and
/// waits for SIGKILL.
pub fn pause_main() -> i32 {
    // SAFETY: fork in a single-threaded process.
    match unsafe { libc::fork() } {
        0 => {
            extern "C" fn nothing(_: libc::c_int) {}
            // SAFETY: setsid, dup2 and signal handlers on our own process.
            unsafe {
                libc::setsid();
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    for fd in 0..3 {
                        libc::dup2(null, fd);
                    }
                }
                libc::signal(libc::SIGCHLD, nothing as *const () as libc::sighandler_t);
            }
            loop {
                // SAFETY: reaping our children; pause until the next signal.
                unsafe {
                    while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
                    libc::pause();
                }
            }
        }
        pid if pid > 0 => {
            println!("{pid}");
            0
        }
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_settings_of_the_pods_own_namespaces_are_set() {
        let all = Wanted {
            net: true,
            ipc: true,
            uts: true,
        };
        let host_net = Wanted { net: false, ..all };
        assert_eq!(
            sysctl_path("net.ipv4.ip_unprivileged_port_start", all).as_deref(),
            Some("/proc/sys/net/ipv4/ip_unprivileged_port_start")
        );
        assert_eq!(sysctl_path("net.ipv4.ip_forward", host_net), None);
        assert_eq!(
            sysctl_path("kernel/shm_rmid_forced", host_net).as_deref(),
            Some("/proc/sys/kernel/shm_rmid_forced")
        );
        assert_eq!(
            sysctl_path("fs.mqueue.msg_max", all).as_deref(),
            Some("/proc/sys/fs/mqueue/msg_max")
        );
        assert_eq!(sysctl_path("kernel.panic", all), None);
        assert_eq!(sysctl_path("net/../../etc", all), None);
        assert_eq!(sysctl_path("net..x", all), None);
    }
}
