//! A service's networks. On one node, an `overlay` network is a podbag bridge network
//! of the same name, marked in the store; `ingress` is one too.
//!
//! Root mode gives each service a virtual address on each of its networks: a member of
//! the network's address table named after the service (so every attached container's
//! hosts file has it), an address of the bridge, and DNAT rules that spread its
//! connections, and those to its published ports, over the tasks. Rootless, the service
//! name is an alias of every task, and one task publishes the ports.

use super::body::Attach;
use super::store::{Overlay, ServiceRecord, Store};
use super::tasks::Task;
use crate::network::Network;
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// The network published ports come in on, and tasks without networks join.
pub const INGRESS: &str = "ingress";

/// The network table key of a service's virtual address (never a container ID).
fn vip_key(service_id: &str) -> String {
    format!("vip-{service_id}")
}

/// Whether the service has a virtual address: root mode and the `vip` endpoint mode.
pub fn has_vip(service: &ServiceRecord) -> bool {
    !super::rootless() && service.spec["EndpointSpec"]["Mode"].as_str() != Some("dnsrr")
}

/// A network by name or by (a prefix of) its Docker ID.
pub fn find(target: &str) -> Result<Network> {
    let all = Network::list()?;
    if let Some(n) = all.iter().find(|n| n.name == target) {
        return Ok(n.clone());
    }
    let matching: Vec<&Network> = all
        .iter()
        .filter(|n| target.len() >= 6 && n.docker_id().starts_with(target))
        .collect();
    match matching.as_slice() {
        [n] => Ok((*n).clone()),
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("network {target} not found"),
            "no network has this name or ID",
            "create it first with `docker network create -d overlay`",
        )),
    }
}

/// The networks a service's tasks join, with their names there: the attachment's
/// aliases, `tasks.<service>`, and the service name itself when it has no virtual
/// address. A service without networks joins `ingress`.
pub fn attachments(service: &ServiceRecord) -> Result<Vec<Attach>> {
    let template = &service.spec["TaskTemplate"];
    let listed = template["Networks"]
        .as_array()
        .or_else(|| service.spec["Networks"].as_array())
        .cloned()
        .unwrap_or_default();
    let listed: Vec<Value> = if listed.is_empty() {
        vec![serde_json::json!({"Target": INGRESS})]
    } else {
        listed
    };
    let name = service.name();
    let mut out: Vec<Attach> = Vec::with_capacity(listed.len());
    for a in &listed {
        let target = a["Target"].as_str().unwrap_or_default();
        let network = find(target)?;
        let mut aliases: Vec<String> = a["Aliases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if !has_vip(service) {
            aliases.push(name.to_string());
        }
        aliases.push(format!("tasks.{name}"));
        let mut seen = std::collections::BTreeSet::new();
        aliases.retain(|a| seen.insert(a.clone()));
        if out.iter().any(|o| o.network == network.name) {
            continue;
        }
        out.push(Attach {
            network: network.name,
            aliases,
        });
    }
    Ok(out)
}

/// Docker's driver name for a podbag network: `overlay` for the marked ones.
pub fn driver_of(dirs: &Dirs, name: &str) -> &'static str {
    match Store::new(dirs).networks() {
        Ok(marks) if marks.contains_key(name) => "overlay",
        _ => "bridge",
    }
}

/// Show a marked network as an overlay network: its driver, scope and flags.
pub fn dress(dirs: &Dirs, network: &mut Value) {
    let Some(name) = network["Name"].as_str().map(String::from) else {
        return;
    };
    let Ok(marks) = Store::new(dirs).networks() else {
        return;
    };
    if let Some(mark) = marks.get(&name) {
        network["Driver"] = Value::from("overlay");
        network["Scope"] = Value::from("swarm");
        network["Attachable"] = Value::from(mark.attachable);
        network["Ingress"] = Value::from(mark.ingress);
    }
}

/// Mark a network as an overlay network.
pub fn mark(store: &Store, name: &str, overlay: Overlay) -> Result<()> {
    let mut marks = store.networks()?;
    marks.insert(name.to_string(), overlay);
    store.write_networks(&marks)
}

/// Forget a removed network's mark.
pub fn forget(dirs: &Dirs, name: &str) {
    let store = Store::new(dirs);
    // Without a swarm there are no marks (and no directory to make for them).
    if !store.dir().exists() {
        return;
    }
    let Ok(_lock) = store.lock() else { return };
    if let Ok(mut marks) = store.networks()
        && marks.remove(name).is_some()
    {
        let _ = store.write_networks(&marks);
    }
}

/// The service's virtual addresses as `/services` shows them: (network ID, addr/prefix).
pub fn vips_json(service: &ServiceRecord) -> Vec<(String, String)> {
    service
        .vips
        .iter()
        .filter_map(|(name, ip)| {
            let n = Network::get(name).ok()?;
            Some((n.docker_id(), format!("{ip}/{}", n.cidr().1)))
        })
        .collect()
}

/// What was last written for each chain: rules are written again only when this changes
/// (a transaction costs the kernel several milliseconds).
#[cfg(target_os = "linux")]
static WRITTEN: std::sync::Mutex<BTreeMap<String, crate::netlink::service_nat::ServiceNat>> =
    std::sync::Mutex::new(BTreeMap::new());

/// The chain name part of a service's network number `index`.
#[cfg(target_os = "linux")]
fn chain_id(service_id: &str, index: usize) -> String {
    format!("{}{index}", &service_id[..12.min(service_id.len())])
}

/// Give the service its virtual addresses and write its NAT rules for the running
/// `tasks` (root mode). Returns whether the record changed (new addresses). Networks
/// whose bridge does not exist yet (no task has started on them) wait for a later call.
pub fn apply(dirs: &Dirs, service: &mut ServiceRecord, tasks: &[&Task]) -> Result<bool> {
    if super::rootless() {
        return Ok(false);
    }
    let attachments = attachments(service)?;
    let mut changed = false;
    if has_vip(service) {
        for a in &attachments {
            let name = service.name().to_string();
            let ip = crate::network::join(
                dirs,
                &a.network,
                &vip_key(&service.id),
                &crate::network::Endpoint {
                    name: &name,
                    hostname: &name,
                    aliases: &[],
                    networks: &[],
                },
            )?;
            let ip = ip.to_string();
            if service.vips.get(&a.network) != Some(&ip) {
                service.vips.insert(a.network.clone(), ip);
                crate::network::refresh_hosts(dirs, &a.network);
                changed = true;
            }
        }
    }
    #[cfg(target_os = "linux")]
    for (index, a) in attachments.iter().enumerate() {
        write_rules(dirs, service, index, a, tasks)?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = tasks;
    Ok(changed)
}

#[cfg(target_os = "linux")]
fn write_rules(
    dirs: &Dirs,
    service: &ServiceRecord,
    index: usize,
    a: &Attach,
    tasks: &[&Task],
) -> Result<()> {
    use crate::netlink::service_nat::{Proto, Published, ServiceNat, apply_batch};
    use crate::netlink::{Socket, route};
    let network = Network::get(&a.network)?;
    let fail = |what: &str, e: std::io::Error| {
        Error::new(
            ErrorKind::Runtime,
            format!(
                "cannot route service {} on network {}",
                service.name(),
                a.network
            ),
            format!("{what}: {e}"),
            "check that nftables works on this host (`podbag check`)",
        )
    };
    let mut s = Socket::route().map_err(|e| fail("netlink", e))?;
    let Some(bridge) = s
        .link_index(&network.bridge)
        .map_err(|e| fail("find the bridge", e))?
    else {
        return Ok(());
    };
    let vip: Option<std::net::Ipv4Addr> =
        service.vips.get(&a.network).and_then(|ip| ip.parse().ok());
    if let Some(vip) = vip {
        match s.request(route::add_address(bridge, vip.octets(), 32)) {
            Err(e) if e.raw_os_error() != Some(libc::EEXIST) => {
                return Err(fail("give the bridge the service's address", e));
            }
            _ => {}
        }
    }
    let mut addrs: Vec<[u8; 4]> = tasks
        .iter()
        .filter(|t| t.running())
        .filter_map(|t| {
            crate::network::with_address_of(dirs, &a.network, &t.container_id)
                .ok()?
                .1
                .map(|ip| ip.octets())
        })
        .collect();
    addrs.sort();
    let ports = if index == 0 {
        service
            .ports
            .iter()
            .map(|p| Published {
                proto: if p.protocol == "udp" {
                    Proto::Udp
                } else {
                    Proto::Tcp
                },
                port: p.published,
                target: p.target,
            })
            .collect()
    } else {
        Vec::new()
    };
    let (subnet, prefix) = network.cidr();
    let nat = ServiceNat {
        chain_id: chain_id(&service.id, index),
        bridge: network.bridge.clone(),
        subnet: (subnet.octets(), prefix),
        ports,
        vip: vip.map(|v| v.octets()),
        tasks: addrs,
    };
    let mut written = WRITTEN.lock().unwrap_or_else(|p| p.into_inner());
    if written.get(&nat.chain_id) == Some(&nat) {
        return Ok(());
    }
    if !nat.ports.is_empty() {
        // Connections from this host's loopback to a published port leave through the
        // bridge (and are masqueraded there).
        let knob = format!("/proc/sys/net/ipv4/conf/{}/route_localnet", network.bridge);
        let _ = std::fs::write(knob, "1");
    }
    let mut f = Socket::open(libc::NETLINK_NETFILTER).map_err(|e| fail("netlink", e))?;
    let first = f.next_seq();
    let (bytes, last) = apply_batch(first, &nat);
    f.request_raw(&bytes, last)
        .map_err(|e| fail("write the NAT rules", e))?;
    written.insert(nat.chain_id.clone(), nat);
    Ok(())
}

/// Take away a service's virtual addresses and NAT rules.
pub fn remove(dirs: &Dirs, service: &ServiceRecord) {
    for (network, ip) in &service.vips {
        let _ = crate::network::release(dirs, network, &vip_key(&service.id));
        crate::network::refresh_hosts(dirs, network);
        #[cfg(target_os = "linux")]
        if let (Ok(n), Ok(ip)) = (Network::get(network), ip.parse::<std::net::Ipv4Addr>())
            && let Ok(mut s) = crate::netlink::Socket::route()
            && let Ok(Some(index)) = s.link_index(&n.bridge)
        {
            let _ = s.request(crate::netlink::route::del_address(index, ip.octets(), 32));
        }
        #[cfg(not(target_os = "linux"))]
        let _ = ip;
    }
    #[cfg(target_os = "linux")]
    if !super::rootless() {
        let mut written = WRITTEN.lock().unwrap_or_else(|p| p.into_inner());
        let networks = attachments(service)
            .map_or(1, |a| a.len())
            .max(service.vips.len());
        for index in 0..networks.max(1) {
            let id = chain_id(&service.id, index);
            written.remove(&id);
            if let Ok(mut f) = crate::netlink::Socket::open(libc::NETLINK_NETFILTER) {
                let first = f.next_seq();
                let (bytes, last) = crate::netlink::service_nat::remove_batch(first, &id);
                let _ = f.request_raw(&bytes, last);
            }
        }
    }
}

/// Make the `ingress` network if there is none. Returns whether it was made.
pub fn ensure_ingress(store: &Store) -> Result<bool> {
    let made = match Network::get(INGRESS) {
        Ok(_) => false,
        Err(e) if e.kind == ErrorKind::NotFound => {
            Network::create(INGRESS, None)?;
            true
        }
        Err(e) => return Err(e),
    };
    mark(
        store,
        INGRESS,
        Overlay {
            attachable: false,
            ingress: true,
        },
    )?;
    Ok(made)
}

/// Remove the overlay networks (when the swarm goes): the ones no container uses.
pub fn remove_overlays(store: &Store, keep_ingress: bool) {
    let marks: BTreeMap<String, Overlay> = store.networks().unwrap_or_default();
    for name in marks.keys() {
        if name == INGRESS && keep_ingress {
            continue;
        }
        let _ = Network::remove(name);
    }
}
