//! Connect containers to bridge networks, through netlink (no `ip` or `nft` programs).
//!
//! The bridge lives in the network namespace of the calling thread when `prepare_bridge`
//! runs (root mode: the host's). The container's side is set up from a short-lived thread
//! that joins the container's network namespace (`setns` works per thread).

use crate::netlink::{Socket, nft, route};
use crate::network::{Addresses, Network};
use crate::{Error, ErrorKind, Result};
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;

fn net_error(network: &str, what: &str, e: std::io::Error) -> Error {
    Error::new(
        ErrorKind::Runtime,
        format!("cannot connect the container to network {network}"),
        format!("{what}: {e}"),
        "check that the network's subnet does not clash with the host's networks",
    )
}

/// Ignore "already exists": another container may have made it a moment ago.
fn tolerate_exists(r: std::io::Result<()>) -> std::io::Result<()> {
    match r {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        r => r,
    }
}

/// Make sure the network's bridge exists with its gateway address and is up, that IPv4
/// forwarding is on, and that its masquerade rule is in place. Safe to repeat, and cheap
/// when everything is there already (every container's start comes through here).
pub fn prepare_bridge(net: &Network) -> Result<u32> {
    let fail = |what: &str, e: std::io::Error| net_error(&net.name, what, e);
    let (subnet, prefix) = net.cidr();
    let gateway: Ipv4Addr = net.gateway.parse().unwrap_or(subnet);
    let mut s = Socket::route().map_err(|e| fail("netlink", e))?;
    let index = match s
        .link_index(&net.bridge)
        .map_err(|e| fail("find bridge", e))?
    {
        Some(i) => i,
        None => {
            tolerate_exists(s.request(route::new_bridge(&net.bridge)))
                .map_err(|e| fail("create bridge", e))?;
            s.link_index(&net.bridge)
                .map_err(|e| fail("find bridge", e))?
                .ok_or_else(|| fail("find bridge", std::io::Error::other("it vanished")))?
        }
    };
    tolerate_exists(s.request(route::add_address(index, gateway.octets(), prefix)))
        .map_err(|e| fail("give the bridge its address", e))?;
    if let Some(subnet) = net.ipv6 {
        tolerate_exists(s.request(route::add_address6(index, subnet.gateway(), subnet.prefix)))
            .map_err(|e| fail("give the bridge its IPv6 address", e))?;
    }
    s.request(route::set_up(index))
        .map_err(|e| fail("bring the bridge up", e))?;
    let forward = "/proc/sys/net/ipv4/ip_forward";
    if std::fs::read_to_string(forward)
        .map(|v| v.trim() != "1")
        .unwrap_or(true)
    {
        std::fs::write(forward, "1").map_err(|e| fail("turn on IPv4 forwarding", e))?;
    }
    let mut f = Socket::open(libc::NETLINK_NETFILTER).map_err(|e| fail("netlink", e))?;
    // A transaction makes the kernel wait for every CPU (several milliseconds), so the
    // rules are written only when they are not all there: for a network's first
    // container, after something removed or emptied the chains (a firewall reload), and
    // after an upgrade that changed the rules. Reading them is not a transaction.
    let in_place = nft::rule_queries(&net.bridge)
        .into_iter()
        .all(|(query, expected)| {
            f.dump(query)
                .is_ok_and(|rules| nft::rules_in_place(&rules, expected, subnet.octets(), prefix))
        });
    if !in_place {
        let first = f.next_seq();
        let (bytes, last) = nft::masquerade_batch(first, subnet.octets(), prefix, &net.bridge);
        f.request_raw(&bytes, last)
            .map_err(|e| fail("add the masquerade rule", e))?;
    }
    Ok(index)
}

/// The host side of a container's veth pair: `op` and the first 10 hex digits of its ID.
/// The host side of a container's veth pair for its interface number `interface` (`eth0`
/// has no number, as before containers had further networks).
pub fn veth_name(container_id: &str, interface: usize) -> String {
    let base = format!("op{}", &container_id[..10.min(container_id.len())]);
    if interface == 0 {
        base
    } else {
        format!("{base}{interface}")
    }
}

/// Run `f` in a thread that joined the network namespace of process `pid`.
pub fn in_netns<T: Send, E: From<std::io::Error> + Send>(
    pid: u32,
    f: impl FnOnce() -> std::result::Result<T, E> + Send,
) -> std::result::Result<T, E> {
    let ns = std::fs::File::open(format!("/proc/{pid}/ns/net"))?;
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                // SAFETY: setns on a namespace descriptor we opened; it affects this thread.
                if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                f()
            })
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("network setup thread panicked").into()))
    })
}

/// Connect process `pid` (a new container, in its own network namespace) to the network:
/// a veth pair on the bridge `bridge_index`, `eth<interface>` inside with `ip`, and for
/// `eth0` the default route.
pub fn connect(
    net: &Network,
    bridge_index: u32,
    container_id: &str,
    pid: u32,
    addresses: Addresses,
    interface: usize,
) -> Result<()> {
    let fail = |what: &str, e: std::io::Error| net_error(&net.name, what, e);
    let (_, prefix) = net.cidr();
    let gateway: Ipv4Addr = net.gateway.parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
    let host_side = veth_name(container_id, interface);
    let inside = format!("eth{interface}");
    let ns = std::fs::File::open(format!("/proc/{pid}/ns/net"))
        .map_err(|e| fail("open the container's network namespace", e))?;
    let mut s = Socket::route().map_err(|e| fail("netlink", e))?;
    let made = match s.request(route::new_veth(&host_side, &inside, ns.as_raw_fd())) {
        // Left by this container's last run: the kernel takes a network namespace (and
        // the veth pair with it) down a moment after its last process. Remove the old
        // device and make the pair again.
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            if let Ok(Some(old)) = s.link_index(&host_side) {
                let _ = s.request(route::del_link(old));
            }
            s.request(route::new_veth(&host_side, &inside, ns.as_raw_fd()))
        }
        made => made,
    };
    made.map_err(|e| fail("create the veth pair", e))?;
    let index = s
        .link_index(&host_side)
        .map_err(|e| fail("find the veth", e))?
        .ok_or_else(|| fail("find the veth", std::io::Error::other("it vanished")))?;
    s.request(route::set_master(index, bridge_index))
        .map_err(|e| fail("attach the veth to the bridge", e))?;
    s.request(route::set_up(index))
        .map_err(|e| fail("bring the veth up", e))?;
    in_netns(pid, || -> std::io::Result<()> {
        let mut s = Socket::route()?;
        if let Some(lo) = s.link_index("lo")? {
            s.request(route::set_up(lo))?;
        }
        let link = s
            .link_index(&inside)?
            .ok_or_else(|| std::io::Error::other(format!("{inside} is missing")))?;
        s.request(route::add_address(link, addresses.ipv4.octets(), prefix))?;
        if let (Some(address), Some(subnet)) = (addresses.ipv6, net.ipv6) {
            s.request(route::add_address6(link, address, subnet.prefix))?;
        }
        s.request(route::set_up(link))?;
        if interface == 0 {
            s.request(route::add_default_route(gateway.octets()))?;
        }
        Ok(())
    })
    .map_err(|e| fail(&format!("set up {inside} in the container"), e))
}

/// Remove a network's bridge device and its NAT chain from the calling thread's network
/// namespace (root mode: the host's). Missing pieces are not an error.
pub fn remove_bridge(net: &Network) -> Result<()> {
    let fail = |what: &str, e: std::io::Error| {
        Error::new(
            ErrorKind::Runtime,
            format!("cannot remove network {}", net.name),
            format!("{what}: {e}"),
            "remove the bridge by hand with `ip link del`",
        )
    };
    let mut s = Socket::route().map_err(|e| fail("netlink", e))?;
    if let Some(index) = s
        .link_index(&net.bridge)
        .map_err(|e| fail("find bridge", e))?
    {
        s.request(route::del_link(index))
            .map_err(|e| fail("delete bridge", e))?;
    }
    let mut f = Socket::open(libc::NETLINK_NETFILTER).map_err(|e| fail("netlink", e))?;
    let first = f.next_seq();
    let (bytes, last) = nft::remove_masquerade_batch(first, &net.bridge);
    match f.request_raw(&bytes, last) {
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
        r => r.map(drop).map_err(|e| fail("delete the NAT chain", e)),
    }
}
