//! PID 1 of the macOS VM: `/init` in its initramfs is openpod itself.
//!
//! The kernel runs it as `/init __vm-init` (the command line ends in `-- __vm-init`): only
//! then is openpod an init. It mounts the kernel's file systems, takes the clock from the
//! Mac (the kernel command line's `openpod.time=`), writes the DNS servers the kernel's
//! DHCP found, mounts the data disk and the Mac's `/Users`, starts the agent
//! (`__vm-agent`, again whenever it ends), and then reaps orphans until it is told to power
//! off (SIGTERM, or the agent's `__vm-poweroff`). When a mount fails, the agent answers
//! every command with why (see [`super::agent::BROKEN`]).

use crate::{Error, Result};
use rustix::mount::{MountFlags, mount};
use std::path::Path;

/// Where openpod keeps its data in the VM: the data disk.
pub const DATA_DIR: &str = "/var/lib/openpod";

pub fn vm_init_main() -> ! {
    // Containers need pivot_root, which the initramfs's own root refuses: the first
    // run moves everything to a tmpfs, makes that the root, and runs /init again.
    if std::env::var_os("OPENPOD_INIT_STAGE").is_none() {
        let e = leave_rootfs();
        eprintln!("openpod: cannot leave the initramfs root: {e}");
    }
    let problems = setup();
    if !problems.is_empty() {
        let why: Vec<String> = problems.iter().map(|e| e.to_string()).collect();
        for w in &why {
            eprintln!("openpod: {w}");
        }
        // The agent answers every command with this: data must not land in memory.
        if let Err(e) = std::fs::write(super::agent::BROKEN, why.join("\n")) {
            eprintln!("openpod: cannot write {}: {e}", super::agent::BROKEN);
        }
    }
    raise_limits();
    let signals = [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGPWR];
    // Before the agent starts: its end must not go unnoticed. Children start with an
    // empty signal mask.
    crate::signals::block(&signals);
    reap_until_poweroff(&signals, start_agent())
}

/// The agent; openpod is /init here, and its helpers find it through OPENPOD_BIN.
fn start_agent() -> Option<libc::pid_t> {
    match std::process::Command::new("/init")
        .arg("__vm-agent")
        .env("OPENPOD_BIN", "/init")
        .spawn()
    {
        Ok(child) => Some(child.id() as libc::pid_t),
        Err(e) => {
            eprintln!("openpod: cannot start the VM agent: {e}");
            None
        }
    }
}

/// What containers inherit, as from Docker Desktop's VM: many open files, and room for
/// programs that map much memory (Elasticsearch).
fn raise_limits() {
    let files = libc::rlimit {
        rlim_cur: 1 << 20,
        rlim_max: 1 << 20,
    };
    // SAFETY: setrlimit with a valid rlimit.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &files) } != 0 {
        eprintln!(
            "openpod: cannot raise the open file limit: {}",
            std::io::Error::last_os_error()
        );
    }
    if let Err(e) = std::fs::write("/proc/sys/vm/max_map_count", "262144") {
        eprintln!("openpod: cannot set vm.max_map_count: {e}");
    }
}

/// Copy the initramfs's files to a tmpfs, free them, make the tmpfs the root and run
/// /init there (as `switch_root` does). Returns only on failure.
fn leave_rootfs() -> std::io::Error {
    let step = || -> std::io::Result<std::convert::Infallible> {
        let new = Path::new("/newroot");
        std::fs::create_dir_all(new)?;
        mount(
            "tmpfs",
            new,
            "tmpfs",
            MountFlags::empty(),
            Some(c"mode=0755"),
        )?;
        for entry in std::fs::read_dir("/")? {
            let entry = entry?;
            if entry.path() != new {
                copy_tree(&entry.path(), &new.join(entry.file_name()))?;
            }
        }
        // The old copies would hold memory for nothing.
        for entry in std::fs::read_dir("/")? {
            let path = entry?.path();
            if path != new {
                let _ = if path.is_dir() && !path.is_symlink() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
            }
        }
        std::env::set_current_dir(new)?;
        rustix::mount::mount_move(".", "/")?;
        rustix::process::chroot(".")?;
        std::env::set_current_dir("/")?;
        use std::os::unix::process::CommandExt;
        Err(std::process::Command::new("/init")
            .arg(super::proto::INIT_ARG)
            .env("OPENPOD_INIT_STAGE", "2")
            .exec())
    };
    match step() {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

/// Copy a file, directory, link or device node, with its mode.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(from)?;
    let kind = meta.file_type();
    if kind.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(meta.mode() & 0o7777))
    } else if kind.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
    } else if kind.is_char_device() || kind.is_block_device() {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            to,
            if kind.is_char_device() {
                rustix::fs::FileType::CharacterDevice
            } else {
                rustix::fs::FileType::BlockDevice
            },
            rustix::fs::Mode::from_raw_mode(meta.mode() & 0o7777),
            meta.rdev(),
        )
        .map_err(Into::into)
    } else {
        std::fs::copy(from, to).map(drop)
    }
}

fn fail(what: &str, e: impl Into<std::io::Error>) -> Error {
    Error::io(
        format!("cannot set up the VM: {what}"),
        &e.into(),
        "see the VM's console log (`openpod vm logs`)",
    )
}

/// Mount `source` on `target`, made first when missing.
fn mount_at(
    source: &str,
    target: &str,
    fstype: &str,
    flags: MountFlags,
    data: Option<&std::ffi::CStr>,
) -> Result<()> {
    std::fs::create_dir_all(target).map_err(|e| fail(target, e))?;
    mount(source, target, fstype, flags, data).map_err(|e| fail(target, e))
}

/// Every step, each whatever the others did; returns what failed. A failed mount leaves
/// the VM unusable (data would land in memory, or containers would not start).
fn setup() -> Vec<Error> {
    let mut problems = Vec::new();
    let mut need = |r: Result<()>| {
        if let Err(e) = r {
            problems.push(e);
        }
    };
    let quiet = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    need(mount_at("proc", "/proc", "proc", quiet, None));
    need(mount_at("sysfs", "/sys", "sysfs", quiet, None));
    // devtmpfs may be mounted already (CONFIG_DEVTMPFS_MOUNT does not apply to an
    // initramfs root, but a second mount is harmless to try).
    let _ = mount("devtmpfs", "/dev", "devtmpfs", MountFlags::NOSUID, None);
    need(
        mount_at(
            "devpts",
            "/dev/pts",
            "devpts",
            MountFlags::NOSUID | MountFlags::NOEXEC,
            Some(c"newinstance,ptmxmode=0666,mode=0620"),
        )
        .and_then(|()| {
            let _ = std::fs::remove_file("/dev/ptmx");
            std::os::unix::fs::symlink("pts/ptmx", "/dev/ptmx").map_err(|e| fail("/dev/ptmx", e))
        }),
    );
    let private = MountFlags::NOSUID | MountFlags::NODEV;
    need(mount_at("tmpfs", "/dev/shm", "tmpfs", private, None));
    need(mount_at(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        quiet,
        None,
    ));
    need(mount_at(
        "tmpfs",
        "/run",
        "tmpfs",
        private,
        Some(c"mode=0755"),
    ));
    need(mount_at(
        "tmpfs",
        "/tmp",
        "tmpfs",
        private,
        Some(c"mode=1777"),
    ));
    // Helpful, not needed: what fails is only reported.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if let Some(time) = clock_from(&cmdline) {
        set_clock(time);
    }
    let _ = rustix::system::sethostname(b"openpod");
    loopback_up();
    let pnp = std::fs::read_to_string("/proc/net/pnp").unwrap_or_default();
    if let Err(e) = std::fs::write("/etc/resolv.conf", resolv_conf(&pnp)) {
        eprintln!("openpod: cannot write /etc/resolv.conf: {e}");
    }
    // The empty inode tables are left as they are: zeroing them would fill the Mac's
    // sparse disk file.
    need(
        std::fs::create_dir_all(DATA_DIR)
            .map_err(|e| fail(DATA_DIR, e))
            .and_then(|()| {
                mount(
                    "/dev/vda",
                    DATA_DIR,
                    "ext4",
                    MountFlags::NOATIME,
                    Some(c"noinit_itable,discard"),
                )
                .map_err(|e| {
                    Error::io(
                        "cannot mount the VM's data disk",
                        &e.into(),
                        "the disk image may be damaged: move ~/.openpod/disk.img away to \
                         start with an empty one (its images, containers and volumes are lost)",
                    )
                })
            }),
    );
    need(mount_at(
        "users",
        "/Users",
        "virtiofs",
        MountFlags::NODEV,
        None,
    ));
    problems
}

fn loopback_up() {
    let up = || -> std::io::Result<()> {
        let mut s = crate::netlink::Socket::route()?;
        if let Some(lo) = s.link_index("lo")? {
            s.request(crate::netlink::route::set_up(lo))?;
        }
        Ok(())
    };
    if let Err(e) = up() {
        eprintln!("openpod: cannot bring up lo: {e}");
    }
}

/// `openpod.time=<seconds>.<nanoseconds>` from the kernel command line.
pub fn clock_from(cmdline: &str) -> Option<(u64, u32)> {
    let value = cmdline
        .split_whitespace()
        .find_map(|w| w.strip_prefix("openpod.time="))?;
    let (secs, nanos) = value.split_once('.').unwrap_or((value, "0"));
    Some((
        secs.parse().ok()?,
        nanos.parse().ok().filter(|n| *n < 1_000_000_000)?,
    ))
}

/// Set the clock to the Mac's.
pub fn set_clock((secs, nanos): (u64, u32)) {
    let ts = libc::timespec {
        tv_sec: secs as _,
        tv_nsec: nanos as _,
    };
    // SAFETY: a valid timespec; failure (no privilege) only leaves the clock as it was.
    unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
}

/// `/etc/resolv.conf` from the kernel's `/proc/net/pnp` (what its DHCP client found).
pub fn resolv_conf(pnp: &str) -> String {
    let mut out = String::new();
    for line in pnp.lines() {
        if let Some(server) = line.strip_prefix("nameserver ") {
            if server.trim() != "0.0.0.0" {
                out.push_str(&format!("nameserver {}\n", server.trim()));
            }
        } else if let Some(domain) = line.strip_prefix("domain ") {
            out.push_str(&format!("search {}\n", domain.trim()));
        }
    }
    if !out.contains("nameserver") {
        // NAT's resolver on Virtualization.framework's network.
        out.push_str("nameserver 192.168.64.1\n");
    }
    out
}

/// PID 1's work after setup: reap every orphan, start the agent again when it ends, and
/// power off on SIGTERM. `signals` are blocked.
fn reap_until_poweroff(signals: &[i32], mut agent: Option<libc::pid_t>) -> ! {
    loop {
        // SAFETY: a sigset on the stack, filled with the signals blocked by the caller.
        let sig = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for &s in signals {
                libc::sigaddset(&mut set, s);
            }
            let mut sig = 0;
            libc::sigwait(&set, &mut sig);
            sig
        };
        if sig != libc::SIGCHLD {
            power_off();
        }
        loop {
            let mut status = 0;
            // SAFETY: waitpid with WNOHANG reaps any ended child.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if Some(pid) == agent {
                eprintln!("openpod: the VM agent ended (status {status:#x}); starting it again");
                std::thread::sleep(std::time::Duration::from_secs(1));
                agent = start_agent();
            }
        }
    }
}

/// End every process, flush the disks, and turn the VM off.
pub fn power_off() -> ! {
    // SAFETY: signals to every process but this one; then reaping.
    unsafe {
        libc::kill(-1, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        // SAFETY: as above.
        let r = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if r == -1 {
            break; // No children left.
        }
        if r == 0 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    // SAFETY: as above.
    unsafe {
        libc::kill(-1, libc::SIGKILL);
        while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
        libc::sync();
    }
    let _ = rustix::mount::unmount(DATA_DIR, rustix::mount::UnmountFlags::empty());
    // SAFETY: reboot with the power-off command; it does not return on success.
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    eprintln!(
        "openpod: cannot power off: {}",
        std::io::Error::last_os_error()
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_comes_from_the_command_line() {
        let cmdline = "console=hvc0 ip=dhcp openpod.time=1700000000.250000000 rw";
        assert_eq!(clock_from(cmdline), Some((1_700_000_000, 250_000_000)));
        assert_eq!(clock_from("openpod.time=12"), Some((12, 0)));
        assert_eq!(clock_from("console=hvc0"), None);
        assert_eq!(clock_from("openpod.time=x.1"), None);
    }

    #[test]
    fn dns_comes_from_the_kernels_dhcp() {
        let pnp = "#PROTO: DHCP\ndomain localdomain\nnameserver 192.168.64.1\nnameserver 0.0.0.0\nbootserver 192.168.64.1\n";
        assert_eq!(
            resolv_conf(pnp),
            "search localdomain\nnameserver 192.168.64.1\n"
        );
        assert_eq!(resolv_conf(""), "nameserver 192.168.64.1\n");
    }
}
