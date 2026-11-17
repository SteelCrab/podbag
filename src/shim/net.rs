//! The shim's network work: the container's generated `/etc` files, and (with a bridge)
//! its address, link and published ports.

use crate::netfiles::{self, Own};
use crate::network::NetworkMode;
use crate::paths::Dirs;
use crate::spec::ContainerSpec;
use crate::volume::{Mount, MountSource};
use crate::{Error, Result};

/// The files `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf`, written into the
/// container directory, as mounts that go before the user's (a user mount on the same
/// path, or on a directory above it, replaces the generated file).
pub(super) fn files(
    spec: &ContainerSpec,
    dirs: &Dirs,
    rootless: bool,
    own: Own<'_>,
    peers: &[(String, std::net::Ipv4Addr)],
) -> Result<Vec<Mount>> {
    let dir = dirs.container_dir(&spec.id);
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    // `rootless` comes from the caller: after joining the session's user namespace this
    // process's user ID reads 0.
    let systemd = std::fs::read_to_string("/run/systemd/resolve/resolv.conf").ok();
    let wanted = [
        ("hostname", format!("{}\n", spec.hostname)),
        (
            "hosts",
            netfiles::hosts_file(own, peers, &read("/etc/hosts")),
        ),
        (
            "resolv.conf",
            netfiles::resolv_conf(
                &spec.network,
                rootless,
                &read("/etc/resolv.conf"),
                systemd.as_deref(),
            ),
        ),
    ];
    let covered = |target: &str| {
        spec.mounts.iter().any(|m| {
            target == m.target
                || target.starts_with(&format!("{}/", m.target.trim_end_matches('/')))
        })
    };
    let mut mounts = Vec::new();
    for (name, content) in wanted {
        let target = format!("/etc/{name}");
        if covered(&target) {
            continue;
        }
        let path = dir.join(name);
        std::fs::write(&path, content).map_err(|e| {
            Error::io(
                format!("cannot write {}", path.display()),
                &e,
                "check that the openpod data directory is writable",
            )
        })?;
        mounts.push(Mount {
            source: MountSource::Bind {
                path: path.to_string_lossy().into_owned(),
            },
            target,
            read_only: false,
            copy_up: false,
        });
    }
    Ok(mounts)
}

/// The container's network and address, for a bridge container.
pub(super) fn bridge_address(
    dirs: &Dirs,
    spec: &ContainerSpec,
    network: &str,
) -> Result<(crate::network::Network, std::net::Ipv4Addr)> {
    let net = crate::network::get_in(dirs, network)?;
    let ip = crate::network::allocate(dirs, network, &spec.id, &spec.name)?;
    Ok((net, ip))
}

/// Rootless bridge containers live in the network session: start it if needed, count
/// this container, and join the session's user namespace (this process must still be
/// single-threaded). Returns the session holder's PID.
pub(super) fn join_session(dirs: &Dirs, spec: &ContainerSpec) -> Result<u32> {
    let pid = crate::linux::session::acquire(dirs, &spec.id)?;
    // Inside the session's user namespace this process's user ID reads 0, which would
    // make helpers it starts pick root's directories: fix them to the ones in use.
    for (var, dir) in [
        ("OPENPOD_DATA_DIR", &dirs.data),
        ("OPENPOD_RUNTIME_DIR", &dirs.runtime),
        ("OPENPOD_CONFIG_DIR", &dirs.config),
    ] {
        // SAFETY: the shim is single-threaded here (it is about to join a user namespace,
        // which requires that).
        unsafe { std::env::set_var(var, dir) };
    }
    if let Err(e) = crate::linux::session::join_userns(pid) {
        crate::linux::session::release(dirs, &spec.id);
        return Err(e);
    }
    Ok(pid)
}

/// Connect the new container `pid` as its network mode says (runs while it waits). With a
/// session, the bridge lives in the session's network namespace.
pub(super) fn attach(
    spec: &ContainerSpec,
    bridge: Option<&(crate::network::Network, std::net::Ipv4Addr)>,
    session: Option<u32>,
    pid: u32,
) -> Result<()> {
    let Some((net, ip)) = bridge else {
        return Ok(());
    };
    let connect = || -> Result<()> {
        let index = crate::linux::net::prepare_bridge(net)?;
        crate::linux::net::connect(net, index, &spec.id, pid, *ip)
    };
    match session {
        None => connect(),
        Some(holder) => {
            crate::linux::net::in_netns(holder, || -> std::io::Result<Result<()>> { Ok(connect()) })
                .map_err(|e| {
                    Error::io(
                        format!("cannot connect the container to network {}", net.name),
                        &e,
                        "check the rootless network session (pasta)",
                    )
                })?
        }
    }
}

/// The other containers on a user network, by name (the default network has no names).
pub(super) fn peers(dirs: &Dirs, spec: &ContainerSpec) -> Vec<(String, std::net::Ipv4Addr)> {
    match &spec.network {
        NetworkMode::Bridge { network } if network != crate::network::DEFAULT => {
            crate::network::members(dirs, network).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// A container on a user network has started: tell the others its name.
pub(super) fn announce(dirs: &Dirs, spec: &ContainerSpec) {
    if let NetworkMode::Bridge { network } = &spec.network {
        crate::network::refresh_hosts(dirs, network);
    }
}

/// Give back what a container held on its network: its address (the others forget its
/// name), and its place in the rootless session.
pub(super) fn detach(dirs: &Dirs, spec: &ContainerSpec, session: bool) {
    if let NetworkMode::Bridge { network } = &spec.network {
        if let Err(e) = crate::network::release(dirs, network, &spec.id) {
            eprintln!("openpod: {e}");
        }
        crate::network::refresh_hosts(dirs, network);
    }
    if session {
        crate::linux::session::release(dirs, &spec.id);
    }
}

/// The container's own `/etc/hosts` line for modes without a bridge address.
pub(super) fn own_without_address(spec: &ContainerSpec) -> Own<'_> {
    match spec.network {
        NetworkMode::None => Own::Loopback(&spec.hostname),
        _ => Own::Host(&spec.hostname),
    }
}
