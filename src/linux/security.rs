//! Security settings applied in the container process (and in commands `exec` starts)
//! after it entered the container's root and before it becomes the container user.

use super::init::os_error;
use crate::spec::Security;
use crate::{Error, ErrorKind, Result};

/// A container's processes get an OOM score adjustment of 0, as with Docker, whatever the
/// process that started them has: in the macOS VM the agent keeps -1000 so that an OOM kill
/// never takes it, and a child inherits the value. Raising it needs no privilege; a failure
/// leaves the inherited value, which is 0 outside the VM.
fn reset_oom_score() {
    let path = "/proc/self/oom_score_adj";
    if std::fs::read_to_string(path).is_ok_and(|v| v.trim() != "0") {
        let _ = std::fs::write(path, "0");
    }
}

/// Shrink the capability bounding set to the container's set, turn on no_new_privs when
/// asked, and install the seccomp filter unless it is off. Runs as root of the container,
/// which holds CAP_SETPCAP and CAP_SYS_ADMIN (seccomp needs one of that or no_new_privs).
pub(super) fn apply(sec: &Security) -> Result<()> {
    reset_oom_score();
    // Privileged: every capability the kernel has, whatever else was asked.
    let keep = if sec.privileged {
        u64::MAX
    } else {
        crate::caps::effective(&sec.cap_add, &sec.cap_drop)?
    };
    for cap in 0..64u32 {
        if keep & (1u64 << cap) != 0 {
            continue;
        }
        // SAFETY: prctl with integer arguments has no memory preconditions.
        if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) } != 0 {
            let e = std::io::Error::last_os_error();
            // Numbers above the kernel's last capability do not exist: nothing to drop.
            if e.raw_os_error() == Some(libc::EINVAL) {
                continue;
            }
            return Err(os_error(
                "cannot drop the container's capabilities",
                e,
                "report this bug",
            ));
        }
    }
    clear_inherited_capabilities()?;
    if sec.no_new_privileges {
        // SAFETY: as above.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(os_error(
                "cannot set no_new_privs",
                std::io::Error::last_os_error(),
                "report this bug",
            ));
        }
    }
    if sec.seccomp && !sec.privileged {
        install_seccomp(keep)?;
    }
    Ok(())
}

/// Empty the ambient and inheritable sets. The bounding set does not limit them, and for
/// root they become permitted at execve: a caller's inheritable CAP_SYS_ADMIN (pam_cap,
/// AmbientCapabilities=, capsh) would otherwise reach the container. A new user namespace
/// starts with both empty; root mode inherits the caller's.
fn clear_inherited_capabilities() -> Result<()> {
    let fail = |what: &str| os_error(what, std::io::Error::last_os_error(), "report this bug");
    // SAFETY: prctl with integer arguments has no memory preconditions.
    let rc = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0,
            0,
            0,
        )
    };
    if rc != 0 {
        return Err(fail("cannot clear the ambient capabilities"));
    }
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const VERSION_3: u32 = 0x2008_0522;
    let mut header = Header {
        version: VERSION_3,
        pid: 0,
    };
    let mut data = [Data::default(); 2];
    // SAFETY: header and data are the sizes capget and capset expect for version 3.
    unsafe {
        if libc::syscall(
            libc::SYS_capget,
            &mut header as *mut Header,
            data.as_mut_ptr(),
        ) != 0
        {
            return Err(fail("cannot read the capabilities"));
        }
        for d in &mut data {
            d.inheritable = 0;
        }
        if libc::syscall(libc::SYS_capset, &mut header as *mut Header, data.as_ptr()) != 0 {
            return Err(fail("cannot clear the inheritable capabilities"));
        }
    }
    Ok(())
}

fn install_seccomp(caps: u64) -> Result<()> {
    let arch = crate::seccomp::native().ok_or_else(|| {
        Error::new(
            ErrorKind::Unsupported,
            "cannot apply the seccomp filter",
            "podbag has no filter for this CPU type",
            "run with --security-opt seccomp=unconfined",
        )
    })?;
    let program = crate::seccomp::deny_filter(arch, caps);
    /// `struct sock_fprog`.
    #[repr(C)]
    struct Fprog {
        len: libc::c_ushort,
        filter: *const crate::seccomp::Insn,
    }
    let fprog = Fprog {
        len: program.len() as libc::c_ushort,
        filter: program.as_ptr(),
    };
    const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
    // SAFETY: fprog points to `program`, which outlives the call; the kernel copies it.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            0 as libc::c_ulong,
            &fprog as *const Fprog,
        )
    };
    if rc != 0 {
        return Err(os_error(
            "cannot apply the seccomp filter",
            std::io::Error::last_os_error(),
            "check that the kernel has seccomp, or run with --security-opt seccomp=unconfined",
        ));
    }
    Ok(())
}
