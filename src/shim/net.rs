//! The shim's network work: the container's generated `/etc` files, and (with a bridge)
//! its address, link and published ports.

use crate::netfiles::{self, HostEntry, HostGateway, Own};
use crate::network::{Addresses, NetworkMode};
use crate::paths::Dirs;
use crate::spec::ContainerSpec;
use crate::volume::{Mount, MountSource};
use crate::{Error, Result};
use std::net::{IpAddr, Ipv4Addr};

/// The files `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf`, written into the
/// container directory, as mounts that go before the user's (a user mount on the same
/// path, or on a directory above it, replaces the generated file).
pub(super) fn files(
    spec: &ContainerSpec,
    dirs: &Dirs,
    rootless: bool,
    own: Own<'_>,
) -> Result<Vec<Mount>> {
    let dir = dirs.container_dir(&spec.id);
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    // `rootless` comes from the caller: after joining the session's user namespace this
    // process's user ID reads 0.
    let systemd = std::fs::read_to_string("/run/systemd/resolve/resolv.conf").ok();
    let wanted = [
        ("hostname", format!("{}\n", spec.hostname)),
        ("hosts", netfiles::hosts_file(&own, &read("/etc/hosts"))),
        (
            "resolv.conf",
            netfiles::resolv_conf(
                &spec.network,
                rootless,
                &read("/etc/resolv.conf"),
                systemd.as_deref(),
                &spec.dns,
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
                "check that the podbag data directory is writable",
            )
        })?;
        mounts.push(Mount {
            source: MountSource::Bind {
                path: path.to_string_lossy().into_owned(),
                propagation: Default::default(),
                shallow_ro: false,
            },
            target,
            read_only: false,
            copy_up: false,
        });
    }
    Ok(mounts)
}

/// The container's networks and its address on each, its primary network first (none
/// with the host's network or none). On an error, the addresses given so far stay
/// recorded: [`detach`] gives them back.
pub(super) fn bridge_addresses(
    dirs: &Dirs,
    spec: &ContainerSpec,
    extra_hosts: &[HostEntry],
) -> Result<Vec<(crate::network::Network, Addresses)>> {
    let networks = spec.bridge_networks();
    // Written down only for a container on several networks: its hosts file names the
    // containers of each.
    let all: Vec<String> = if networks.len() > 1 {
        networks.iter().map(|n| n.to_string()).collect()
    } else {
        Vec::new()
    };
    let mut out = Vec::with_capacity(networks.len());
    for (index, network) in networks.iter().enumerate() {
        let net = crate::network::get_in(dirs, network)?;
        let ip = crate::network::join(
            dirs,
            network,
            &spec.id,
            &crate::network::Endpoint {
                name: &spec.name,
                hostname: &spec.hostname,
                aliases: spec.aliases_on(index),
                networks: &all,
                extra_hosts,
                fixed: spec
                    .fixed_addresses
                    .get(*network)
                    .copied()
                    .unwrap_or_default(),
            },
        )?;
        out.push((net, ip));
    }
    Ok(out)
}

/// The container's extra `/etc/hosts` lines with their addresses: `host-gateway` is
/// found now, as the host is reached from this start's network.
pub(super) fn extra_hosts(
    dirs: &Dirs,
    spec: &ContainerSpec,
    rootless: bool,
) -> Result<Vec<HostEntry>> {
    netfiles::resolve_extra_hosts(&spec.extra_hosts, || {
        let unknown = |why: String| {
            Error::new(
                crate::ErrorKind::Runtime,
                format!("cannot find the address of {}", netfiles::HOST_GATEWAY),
                why,
                "give the host's address instead: --add-host NAME:IP",
            )
        };
        match netfiles::host_gateway(&spec.network, rootless, netfiles::Place::detect())? {
            HostGateway::Loopback => Ok(Ipv4Addr::LOCALHOST.into()),
            HostGateway::Bridge => {
                let first = spec.bridge_networks().first().copied().unwrap_or_default();
                let net = crate::network::get_in(dirs, first)?;
                net.gateway
                    .parse::<Ipv4Addr>()
                    .map(IpAddr::from)
                    .map_err(|_| unknown(format!("network {first} has no gateway address")))
            }
            HostGateway::VmRoute => std::fs::read_to_string("/proc/net/route")
                .ok()
                .as_deref()
                .and_then(netfiles::default_gateway)
                .map(IpAddr::from)
                .ok_or_else(|| unknown("the VM has no default route to the Mac".to_string())),
        }
    })
}

/// What the container's `/etc/hosts` is made of: with addresses, the names of its
/// networks' containers (the network records hold its extra lines too).
pub(super) fn own<'a>(
    dirs: &Dirs,
    spec: &'a ContainerSpec,
    addresses: &[(crate::network::Network, Addresses)],
    extra_hosts: &'a [HostEntry],
) -> Result<Own<'a>> {
    if addresses.is_empty() {
        return Ok(own_without_address(spec, extra_hosts));
    }
    let networks: Vec<String> = spec
        .bridge_networks()
        .iter()
        .map(|n| n.to_string())
        .collect();
    crate::network::hosts_text(dirs, &spec.id, &networks).map(Own::Bridge)
}

/// Rootless bridge containers live in the network session: start it if needed, count
/// this container, and join the session's user namespace (this process must still be
/// single-threaded). Returns the session holder's PID.
pub(super) fn join_session(dirs: &Dirs, spec: &ContainerSpec) -> Result<u32> {
    let pid = crate::linux::session::acquire(dirs, &spec.id)?;
    // Inside the session's user namespace this process's user ID reads 0, which would
    // make helpers it starts pick root's directories: fix them to the ones in use.
    for (var, dir) in [
        ("PODBAG_DATA_DIR", &dirs.data),
        ("PODBAG_RUNTIME_DIR", &dirs.runtime),
        ("PODBAG_CONFIG_DIR", &dirs.config),
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

/// Connect the new container `pid` to each of its networks (runs while it waits): `eth0`
/// on the first, with the default route, `eth1` on the next, and so on. With a session,
/// the bridges live in the session's network namespace.
pub(super) fn attach(
    spec: &ContainerSpec,
    addresses: &[(crate::network::Network, Addresses)],
    session: Option<u32>,
    pid: u32,
) -> Result<()> {
    let Some((first, _)) = addresses.first() else {
        return Ok(());
    };
    let connect = || -> Result<()> {
        for (interface, (net, ip)) in addresses.iter().enumerate() {
            let index = crate::linux::net::prepare_bridge(net)?;
            crate::linux::net::connect(net, index, &spec.id, pid, *ip, interface)?;
        }
        Ok(())
    };
    match session {
        None => connect(),
        Some(holder) => {
            crate::linux::net::in_netns(holder, || -> std::io::Result<Result<()>> { Ok(connect()) })
                .map_err(|e| {
                    Error::io(
                        format!("cannot connect the container to network {}", first.name),
                        &e,
                        "check the rootless network session (pasta)",
                    )
                })?
        }
    }
}

/// A container on user networks has started: tell the others its names.
pub(super) fn announce(dirs: &Dirs, spec: &ContainerSpec) {
    for network in spec.bridge_networks() {
        crate::network::refresh_hosts(dirs, network);
    }
}

/// Give back what a container held on its networks: its addresses (the others forget its
/// names), and its place in the rootless session.
pub(super) fn detach(dirs: &Dirs, spec: &ContainerSpec, session: bool) {
    for network in spec.bridge_networks() {
        if let Err(e) = crate::network::release(dirs, network, &spec.id) {
            eprintln!("podbag: {e}");
        }
        crate::network::refresh_hosts(dirs, network);
    }
    if session {
        crate::linux::session::release(dirs, &spec.id);
    }
}

/// The container's own `/etc/hosts` lines for modes without a bridge address.
pub(super) fn own_without_address<'a>(
    spec: &'a ContainerSpec,
    extra_hosts: &'a [HostEntry],
) -> Own<'a> {
    match spec.network {
        NetworkMode::None => Own::Loopback(&spec.hostname, extra_hosts),
        _ => Own::Host(&spec.hostname, extra_hosts),
    }
}
