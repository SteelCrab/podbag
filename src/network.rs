//! Networks: the `--network` and `-p` arguments, and the network store
//! (`<data>/networks/<name>.json`) with each network's address table. The logic does not
//! depend on the OS; devices and rules are made in `linux::net`.

use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

/// The network a container gets without `--network`.
pub const DEFAULT: &str = "bridge";
const DEFAULT_BRIDGE: &str = "openpod0";
const DEFAULT_SUBNET: (Ipv4Addr, u8) = (Ipv4Addr::new(10, 77, 0, 0), 16);
/// User networks get `10.78.<n>.0/24`: outside the default network, so routes never overlap.
const AUTO_BASE: [u8; 2] = [10, 78];

/// How a container is connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NetworkMode {
    /// A bridge network: the default one or one made with `network create`.
    Bridge { network: String },
    /// The host's network (no network namespace).
    Host,
    /// Loopback only.
    None,
}

impl Default for NetworkMode {
    fn default() -> Self {
        NetworkMode::Bridge {
            network: DEFAULT.to_string(),
        }
    }
}

impl NetworkMode {
    /// What containers made before networks existed used: the host's network.
    pub fn legacy() -> Self {
        NetworkMode::Host
    }
}

fn invalid(what: String, why: impl Into<String>, next: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, what, why, next)
}

/// A `--network` value: `bridge`, `host`, `none` or a network name.
pub fn parse_network_arg(s: &str) -> Result<NetworkMode> {
    match s {
        "bridge" | "default" => Ok(NetworkMode::default()),
        "host" => Ok(NetworkMode::Host),
        "none" => Ok(NetworkMode::None),
        _ if s.contains(':') => Err(invalid(
            format!("network mode {s:?} is not supported"),
            "openpod connects to bridge, host, none or a network name",
            "use one of those",
        )),
        name => {
            check_name(name)?;
            Ok(NetworkMode::Bridge {
                network: name.to_string(),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
}

/// A published port (`-p`). Host port 0: the kernel picks one when the container starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    pub host_ip: Ipv4Addr,
    pub host_port: u16,
    pub container_port: u16,
    pub proto: Proto,
}

/// A `-p` value: `[IP:][HOST_PORT:]CONTAINER_PORT[/tcp|udp]`.
pub fn parse_publish_arg(s: &str) -> Result<PortMapping> {
    let bad = |why: &str| {
        invalid(
            format!("invalid port mapping {s:?}"),
            why,
            "write it like 8080:80, 127.0.0.1:8080:80 or 53:53/udp",
        )
    };
    let (spec, proto) = match s.rsplit_once('/') {
        Some((spec, "tcp")) => (spec, Proto::Tcp),
        Some((spec, "udp")) => (spec, Proto::Udp),
        Some(_) => return Err(bad("the protocol must be tcp or udp")),
        None => (s, Proto::Tcp),
    };
    let parts: Vec<&str> = spec.split(':').collect();
    let (ip, host, container) = match parts.as_slice() {
        [c] => (None, "", *c),
        [h, c] => (None, *h, *c),
        [ip, h, c] => (Some(*ip), *h, *c),
        _ => return Err(bad("it has too many parts")),
    };
    let port = |p: &str| p.parse::<u16>().ok().filter(|&n| n > 0);
    let container_port =
        port(container).ok_or_else(|| bad("the container port must be 1-65535"))?;
    let host_port = match host {
        "" => 0,
        h => port(h)
            .ok_or_else(|| bad("the host port must be 1-65535 (ranges are not supported)"))?,
    };
    let host_ip = match ip {
        None => Ipv4Addr::UNSPECIFIED,
        Some(ip) => ip
            .parse()
            .map_err(|_| bad("the address is not an IPv4 address"))?,
    };
    Ok(PortMapping {
        host_ip,
        host_port,
        container_port,
        proto,
    })
}

/// A network name: the same rule as volume names, and not a mode name.
pub fn check_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid = bytes.len() >= 2
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if valid {
        return Ok(());
    }
    Err(invalid(
        format!("invalid network name {name:?}"),
        "a network name starts with a letter or digit and has only letters, digits, _, . and -",
        "choose a name like app or backend",
    ))
}

/// A bridge network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    pub name: String,
    /// The first 12 hex digits of the name's SHA-256, as `network ls` shows.
    pub id: String,
    /// `10.78.1.0/24`.
    pub subnet: String,
    pub gateway: String,
    /// The bridge device.
    pub bridge: String,
    pub created: u64,
}

impl Network {
    /// Create a network; without `subnet`, the next free `10.78.<n>.0/24`.
    pub fn create(name: &str, subnet: Option<&str>) -> Result<Network> {
        create_in(&crate::paths::dirs()?, name, subnet, &host_routes())
    }

    pub fn get(name: &str) -> Result<Network> {
        get_in(&crate::paths::dirs()?, name)
    }

    /// Every network, by name (the default `bridge` included).
    pub fn list() -> Result<Vec<Network>> {
        list_in(&crate::paths::dirs()?)
    }

    /// Remove a network that no container uses. As root, its bridge device and NAT chain
    /// go too; rootless, they live in the network session and end with it.
    pub fn remove(name: &str) -> Result<()> {
        let network = remove_in(&crate::paths::dirs()?, name)?;
        #[cfg(target_os = "linux")]
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            crate::linux::net::remove_bridge(&network)?;
        }
        let _ = network;
        Ok(())
    }

    /// Remove every network no container uses (not the default one). Returns them.
    pub fn prune() -> Result<Vec<Network>> {
        let removed = prune_in(&crate::paths::dirs()?)?;
        #[cfg(target_os = "linux")]
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            for network in &removed {
                crate::linux::net::remove_bridge(network)?;
            }
        }
        Ok(removed)
    }

    /// The full ID the Docker API shows (64 hex digits; [`Network::id`] is its start).
    pub fn docker_id(&self) -> String {
        docker_id_of(&self.name)
    }

    /// The subnet's address and prefix length.
    pub fn cidr(&self) -> (Ipv4Addr, u8) {
        parse_cidr(&self.subnet).unwrap_or(DEFAULT_SUBNET)
    }
}

/// A network with its address table.
#[derive(Serialize, Deserialize)]
struct Record {
    network: Network,
    #[serde(default)]
    containers: std::collections::BTreeMap<String, Member>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Member {
    ip: Ipv4Addr,
    name: String,
    /// The container's hostname, for its own line in `/etc/hosts`. Records written
    /// before it was stored here have none: the container's config has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
}

/// The host's IPv4 routes (destination and prefix), except the default route, so a new
/// network does not hide part of the host's network.
fn host_routes() -> Vec<(Ipv4Addr, u8)> {
    std::fs::read_to_string("/proc/net/route")
        .map(|text| parse_routes(&text))
        .unwrap_or_default()
}

/// Read `/proc/net/route`.
fn parse_routes(text: &str) -> Vec<(Ipv4Addr, u8)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // openpod's own bridges: their subnets are networks, checked as such.
            let iface = *f.first()?;
            if iface == DEFAULT_BRIDGE || iface.starts_with("opd-") {
                return None;
            }
            let dest = u32::from_str_radix(f.get(1)?, 16).ok()?;
            let mask = u32::from_str_radix(f.get(7)?, 16).ok()?;
            // The address bytes, in network order, printed as a little-endian number
            // (both CPUs openpod builds for are little-endian).
            (mask != 0).then(|| (Ipv4Addr::from(dest.to_le_bytes()), mask.count_ones() as u8))
        })
        .collect()
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

/// `a.b.c.d/n` with no host bits set and a prefix from 8 to 30.
pub fn parse_cidr(s: &str) -> Result<(Ipv4Addr, u8)> {
    let bad = |why: &str| {
        invalid(
            format!("invalid subnet {s:?}"),
            why,
            "write it like 172.30.0.0/16",
        )
    };
    let (addr, prefix) = s.split_once('/').ok_or_else(|| bad("it has no /prefix"))?;
    let addr: Ipv4Addr = addr.parse().map_err(|_| bad("the address is not IPv4"))?;
    let prefix: u8 = prefix
        .parse()
        .ok()
        .filter(|p| (8..=30).contains(p))
        .ok_or_else(|| bad("the prefix must be from 8 to 30"))?;
    if u32::from(addr) & !mask(prefix) != 0 {
        return Err(bad("the address has host bits set"));
    }
    Ok((addr, prefix))
}

fn overlaps(a: (Ipv4Addr, u8), b: (Ipv4Addr, u8)) -> bool {
    let m = mask(a.1.min(b.1));
    u32::from(a.0) & m == u32::from(b.0) & m
}

/// The 64-hex ID of a network name (also for `host` and `none`, which have no record).
pub fn docker_id_of(name: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(name.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn prune_in(dirs: &Dirs) -> Result<Vec<Network>> {
    let mut removed = Vec::new();
    for network in list_in(dirs)? {
        if network.name == DEFAULT {
            continue;
        }
        // Checks its users again under the network lock.
        match remove_in(dirs, &network.name) {
            Ok(n) => removed.push(n),
            Err(e) if matches!(e.kind, ErrorKind::Conflict | ErrorKind::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

/// The containers in `network`: (ID, name, address), by address.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn endpoints(dirs: &Dirs, network: &str) -> Result<Vec<(String, String, Ipv4Addr)>> {
    let _lock = lock(dirs)?;
    let record = load(dirs, network)?;
    let mut out: Vec<(String, String, Ipv4Addr)> = record
        .containers
        .into_iter()
        .map(|(id, m)| (id, m.name, m.ip))
        .collect();
    out.sort_by_key(|(_, _, ip)| u32::from(*ip));
    Ok(out)
}

fn sha_id(name: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(name.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn networks_dir(dirs: &Dirs) -> Result<std::path::PathBuf> {
    let dir = dirs.data.join("networks");
    let fail = |e: std::io::Error| store_error(&dir, e);
    let mut builder = crate::store::perm::private_dir_builder();
    builder.recursive(true).create(&dir).map_err(fail)?;
    Ok(dir)
}

fn store_error(path: &std::path::Path, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot use {}", path.display()),
        &e,
        "check the permissions of the openpod data directory",
    )
}

/// One lock for every network file: creating, removing, and changing address tables.
fn lock(dirs: &Dirs) -> Result<std::fs::File> {
    let path = networks_dir(dirs)?.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| store_error(&path, e))?;
    file.lock().map_err(|e| store_error(&path, e))?;
    Ok(file)
}

fn record_path(dirs: &Dirs, name: &str) -> std::path::PathBuf {
    dirs.data.join("networks").join(format!("{name}.json"))
}

fn read_record(dirs: &Dirs, name: &str) -> Result<Option<Record>> {
    let path = record_path(dirs, name);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!("cannot read network {name}"),
                format!("{} is damaged: {e}", path.display()),
                "remove the network with `openpod network rm`",
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(store_error(&path, e)),
    }
}

fn write_record(dirs: &Dirs, record: &Record) -> Result<()> {
    let path = record_path(dirs, &record.network.name);
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{n}.tmp", std::process::id()));
    let json = serde_json::to_vec_pretty(record).unwrap_or_default();
    std::fs::write(&tmp, json)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| store_error(&path, e))
}

fn not_found(name: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("no such network: {name}"),
        "it does not exist",
        "run `openpod network ls`, or create it with `openpod network create`",
    )
}

fn default_record(dirs: &Dirs, host_routes: &[(Ipv4Addr, u8)]) -> Result<Record> {
    let subnet = crate::config::load(dirs)?.default_subnet;
    let (addr, prefix) = parse_cidr(&subnet)?;
    if let Some((a, p)) = host_routes.iter().find(|r| overlaps(**r, (addr, prefix))) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot set up the default network",
            format!("its subnet {subnet} overlaps the host route {a}/{p}"),
            "set default_subnet in config.toml (or OPENPOD_DEFAULT_SUBNET) to a free subnet",
        ));
    }
    Ok(Record {
        network: Network {
            name: DEFAULT.to_string(),
            id: sha_id(DEFAULT),
            subnet: format!("{addr}/{prefix}"),
            gateway: Ipv4Addr::from(u32::from(addr) + 1).to_string(),
            // openpod0 for the usual subnet; another subnet gets a bridge of its own, so
            // data directories with different default subnets can share a root host.
            bridge: if (addr, prefix) == DEFAULT_SUBNET {
                DEFAULT_BRIDGE.to_string()
            } else {
                format!("opd-{}", &sha_id(&subnet)[..8])
            },
            created: crate::state::now(),
        },
        containers: Default::default(),
    })
}

/// Read a network's record; the default network is made on first use, checked against
/// the host's routes.
fn load(dirs: &Dirs, name: &str) -> Result<Record> {
    let routes = if name == DEFAULT {
        host_routes()
    } else {
        Vec::new()
    };
    load_with(dirs, name, &routes)
}

fn load_with(dirs: &Dirs, name: &str, host_routes: &[(Ipv4Addr, u8)]) -> Result<Record> {
    match read_record(dirs, name)? {
        Some(r) => Ok(r),
        None if name == DEFAULT => {
            let r = default_record(dirs, host_routes)?;
            write_record(dirs, &r)?;
            Ok(r)
        }
        None => Err(not_found(name)),
    }
}

/// The default network, made on first use.
#[cfg(test)]
pub(crate) fn ensure_default(dirs: &Dirs) -> Result<Network> {
    let _lock = lock(dirs)?;
    load(dirs, DEFAULT).map(|r| r.network)
}

fn all_records(dirs: &Dirs) -> Result<Vec<Record>> {
    let dir = networks_dir(dirs)?;
    let entries = std::fs::read_dir(&dir).map_err(|e| store_error(&dir, e))?;
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if let Some(name) = file.strip_suffix(".json").filter(|n| !n.starts_with('.')) {
            if let Some(r) = read_record(dirs, name)? {
                records.push(r);
            }
        }
    }
    records.sort_by(|a, b| a.network.name.cmp(&b.network.name));
    Ok(records)
}

/// Create a network. Without `subnet`, the next free `10.78.<n>.0/24` that overlaps neither
/// another network nor one of `host_routes`.
pub(crate) fn create_in(
    dirs: &Dirs,
    name: &str,
    subnet: Option<&str>,
    host_routes: &[(Ipv4Addr, u8)],
) -> Result<Network> {
    check_name(name)?;
    if matches!(name, "bridge" | "host" | "none" | "default") {
        return Err(invalid(
            format!("cannot create network {name}"),
            format!("{name} is a predefined network mode"),
            "choose another name",
        ));
    }
    let _lock = lock(dirs)?;
    if read_record(dirs, name)?.is_some() {
        return Err(Error::new(
            ErrorKind::Conflict,
            format!("cannot create network {name}"),
            "a network with that name exists",
            "choose another name, or remove it with `openpod network rm`",
        ));
    }
    // The default network counts even before its first use.
    load_with(dirs, DEFAULT, host_routes)?;
    let others: Vec<Network> = all_records(dirs)?.into_iter().map(|r| r.network).collect();
    let conflict = |cidr: (Ipv4Addr, u8)| -> Option<String> {
        if let Some(n) = others.iter().find(|n| overlaps(n.cidr(), cidr)) {
            return Some(format!("it overlaps network {} ({})", n.name, n.subnet));
        }
        host_routes
            .iter()
            .find(|r| overlaps(**r, cidr))
            .map(|(a, p)| format!("it overlaps the host route {a}/{p}"))
    };
    let cidr = match subnet {
        Some(s) => {
            let cidr = parse_cidr(s)?;
            if let Some(why) = conflict(cidr) {
                return Err(invalid(
                    format!("cannot create network {name} with subnet {s}"),
                    why,
                    "choose a subnet that no network or host route uses",
                ));
            }
            cidr
        }
        None => (0..=255u8)
            .map(|n| (Ipv4Addr::new(AUTO_BASE[0], AUTO_BASE[1], n, 0), 24))
            .find(|c| conflict(*c).is_none())
            .ok_or_else(|| {
                invalid(
                    format!("cannot create network {name}"),
                    "every 10.78.<n>.0/24 subnet is taken",
                    "remove unused networks, or give --subnet",
                )
            })?,
    };
    let id = sha_id(name);
    let network = Network {
        name: name.to_string(),
        // Named after the subnet: rootless, a removed network's bridge stays in the session,
        // and a new network on the same subnet must reuse it, not add a second route.
        bridge: format!("opd-{}", &sha_id(&format!("{}/{}", cidr.0, cidr.1))[..8]),
        id,
        subnet: format!("{}/{}", cidr.0, cidr.1),
        gateway: Ipv4Addr::from(u32::from(cidr.0) + 1).to_string(),
        created: crate::state::now(),
    };
    write_record(
        dirs,
        &Record {
            network: network.clone(),
            containers: Default::default(),
        },
    )?;
    Ok(network)
}

pub(crate) fn get_in(dirs: &Dirs, name: &str) -> Result<Network> {
    if name != DEFAULT {
        check_name(name)?;
    }
    let _lock = lock(dirs)?;
    load(dirs, name).map(|r| r.network)
}

/// Every network, by name; the default network is always there.
pub(crate) fn list_in(dirs: &Dirs) -> Result<Vec<Network>> {
    let _lock = lock(dirs)?;
    load(dirs, DEFAULT)?;
    Ok(all_records(dirs)?.into_iter().map(|r| r.network).collect())
}

/// Delete a network's record. Refused for the predefined networks and while a container
/// that still exists has an address in it.
pub(crate) fn remove_in(dirs: &Dirs, name: &str) -> Result<Network> {
    if matches!(name, "bridge" | "host" | "none" | "default") {
        return Err(invalid(
            format!("cannot remove network {name}"),
            "it is a predefined network",
            "remove the networks you created only",
        ));
    }
    check_name(name)?;
    let _lock = lock(dirs)?;
    let record = read_record(dirs, name)?.ok_or_else(|| not_found(name))?;
    let users: Vec<String> = record
        .containers
        .iter()
        .filter(|(id, _)| crate::state::read_config(dirs, id).is_ok())
        .map(|(_, m)| m.name.clone())
        .collect();
    if !users.is_empty() {
        return Err(Error::new(
            ErrorKind::Conflict,
            format!("cannot remove network {name}"),
            format!("it is used by container {}", users.join(", ")),
            "remove those containers first (`openpod rm`)",
        ));
    }
    let path = record_path(dirs, name);
    std::fs::remove_file(&path).map_err(|e| store_error(&path, e))?;
    Ok(record.network)
}

/// An address for container `id` in `network`: its old one, or the lowest free one after
/// the gateway.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn allocate(
    dirs: &Dirs,
    network: &str,
    id: &str,
    name: &str,
    hostname: &str,
) -> Result<Ipv4Addr> {
    let _lock = lock(dirs)?;
    let mut record = load(dirs, network)?;
    if let Some(m) = record.containers.get_mut(id) {
        let ip = m.ip;
        // A member from an earlier version's record learns its hostname at its next start.
        if m.hostname.as_deref() != Some(hostname) {
            m.hostname = Some(hostname.to_string());
            write_record(dirs, &record)?;
        }
        return Ok(ip);
    }
    let (addr, prefix) = record.network.cidr();
    let first = u32::from(addr) + 2;
    let broadcast = u32::from(addr) | !mask(prefix);
    let used: std::collections::BTreeSet<u32> = record
        .containers
        .values()
        .map(|m| u32::from(m.ip))
        .collect();
    let ip = (first..broadcast)
        .find(|a| !used.contains(a))
        .map(Ipv4Addr::from)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Runtime,
                format!("cannot give container {name} an address"),
                format!("network {network} has no free address left"),
                "remove containers from it, or use a network with a larger subnet",
            )
        })?;
    record.containers.insert(
        id.to_string(),
        Member {
            ip,
            name: name.to_string(),
            hostname: Some(hostname.to_string()),
        },
    );
    write_record(dirs, &record)?;
    Ok(ip)
}

/// Give back container `id`'s address. A missing network or entry is not an error.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn release(dirs: &Dirs, network: &str, id: &str) -> Result<()> {
    let _lock = lock(dirs)?;
    let Some(mut record) = read_record(dirs, network)? else {
        return Ok(());
    };
    if record.containers.remove(id).is_some() {
        write_record(dirs, &record)?;
    }
    Ok(())
}

/// Rewrite the `/etc/hosts` of every container on a user network so it names all the
/// network's containers (the default network has no names, as with Docker). Under the
/// network lock: a start and a stop at the same moment cannot leave a stale list behind.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn refresh_hosts(dirs: &Dirs, network: &str) {
    if network == DEFAULT {
        return;
    }
    let Ok(_lock) = lock(dirs) else { return };
    let Ok(Some(record)) = read_record(dirs, network) else {
        return;
    };
    let peers: Vec<(String, Ipv4Addr)> = record
        .containers
        .values()
        .map(|m| (m.name.clone(), m.ip))
        .collect();
    // The names are the same in every member's file: made once, not once for each.
    let block = crate::netfiles::PeerBlock::new(&peers);
    for (index, (id, m)) in record.containers.iter().enumerate() {
        // A record of an earlier version has no hostnames: the member's config has it.
        let from_config;
        let hostname = match &m.hostname {
            Some(hostname) => hostname.as_str(),
            None => match crate::state::read_config(dirs, id) {
                Ok(spec) => {
                    from_config = spec.hostname;
                    from_config.as_str()
                }
                Err(_) => continue,
            },
        };
        let text = block.hosts_of(index, &m.name, hostname, m.ip);
        // A user mount may replace the file: then there is nothing to write.
        let _ = crate::netfiles::rewrite_in_place(&dirs.container_dir(id).join("hosts"), &text);
    }
}

/// The containers with an address in `network`: (name, address), by address.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn members(dirs: &Dirs, network: &str) -> Result<Vec<(String, Ipv4Addr)>> {
    let _lock = lock(dirs)?;
    let record = load(dirs, network)?;
    let mut out: Vec<(String, Ipv4Addr)> = record
        .containers
        .into_values()
        .map(|m| (m.name, m.ip))
        .collect();
    out.sort_by_key(|(_, ip)| u32::from(*ip));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn dirs_in(tmp: &Path) -> Dirs {
        Dirs {
            data: tmp.join("data"),
            runtime: tmp.join("run"),
            config: tmp.join("cfg"),
        }
    }

    #[test]
    fn prune_removes_networks_no_container_uses() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "idle", None, &[]).unwrap();
        create_in(&dirs, "busy", None, &[]).unwrap();
        let spec = crate::spec::spec_fixture();
        crate::state::create(&dirs, &spec).unwrap();
        allocate(&dirs, "busy", &spec.id, &spec.name, &spec.hostname).unwrap();
        let gone: Vec<String> = prune_in(&dirs)
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(gone, ["idle"]);
        let left: Vec<String> = list_in(&dirs)
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert!(
            left.contains(&"busy".to_string()) && left.contains(&DEFAULT.to_string()),
            "{left:?}"
        );
    }

    #[test]
    fn docker_ids_extend_the_short_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let n = create_in(&dirs, "app", None, &[]).unwrap();
        assert_eq!(n.docker_id().len(), 64);
        assert!(n.docker_id().starts_with(&n.id));
        assert_eq!(docker_id_of("host").len(), 64);
    }

    #[test]
    fn a_taken_network_name_is_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "app", None, &[]).unwrap();
        let e = create_in(&dirs, "app", None, &[]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Conflict);
    }

    #[test]
    fn endpoints_name_their_containers() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "app", None, &[]).unwrap();
        let spec = crate::spec::spec_fixture();
        let ip = allocate(&dirs, "app", &spec.id, &spec.name, &spec.hostname).unwrap();
        assert_eq!(
            endpoints(&dirs, "app").unwrap(),
            [(spec.id.clone(), spec.name.clone(), ip)]
        );
    }

    #[test]
    fn parses_network_and_publish_args() {
        assert_eq!(parse_network_arg("bridge").unwrap(), NetworkMode::default());
        assert_eq!(parse_network_arg("host").unwrap(), NetworkMode::Host);
        assert_eq!(parse_network_arg("none").unwrap(), NetworkMode::None);
        assert_eq!(
            parse_network_arg("app").unwrap(),
            NetworkMode::Bridge {
                network: "app".into()
            }
        );
        assert!(parse_network_arg("container:x").is_err());
        let p = |s: &str| parse_publish_arg(s).unwrap();
        assert_eq!(
            p("8080:80"),
            PortMapping {
                host_ip: Ipv4Addr::UNSPECIFIED,
                host_port: 8080,
                container_port: 80,
                proto: Proto::Tcp
            }
        );
        assert_eq!(
            p("127.0.0.1:8080:80/udp"),
            PortMapping {
                host_ip: Ipv4Addr::LOCALHOST,
                host_port: 8080,
                container_port: 80,
                proto: Proto::Udp
            }
        );
        assert_eq!(p("80").host_port, 0);
        assert_eq!(p("127.0.0.1::80").host_port, 0);
        for bad in [
            "",
            "x:80",
            "80:0",
            "8080:80/sctp",
            "1.2.3:8:80",
            "8000-8010:80",
            "70000:80",
        ] {
            assert_eq!(
                parse_publish_arg(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn default_network_follows_the_setting() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(
            dirs.config.join("config.toml"),
            "default_subnet = \"10.79.4.0/24\"\n",
        )
        .unwrap();
        let net = ensure_default(&dirs).unwrap();
        assert_eq!(
            (net.subnet.as_str(), net.gateway.as_str()),
            ("10.79.4.0/24", "10.79.4.1")
        );
    }

    #[test]
    fn default_network_exists_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let a = ensure_default(&dirs).unwrap();
        let b = ensure_default(&dirs).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.name, "bridge");
        assert_eq!(a.bridge, "openpod0");
        assert_eq!(a.subnet, "10.77.0.0/16");
        assert_eq!(a.gateway, "10.77.0.1");
    }

    #[test]
    fn user_networks_get_the_next_free_slash24() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let routes = [(Ipv4Addr::new(10, 78, 0, 0), 24)];
        ensure_default(&dirs).unwrap();
        let app = create_in(&dirs, "app", None, &routes).unwrap();
        assert_eq!(app.subnet, "10.78.1.0/24", "10.78.0.0/24 is a host route");
        assert_eq!(app.gateway, "10.78.1.1");
        assert!(app.bridge.starts_with("opd-") && app.bridge.len() <= 15);
        assert_eq!(app.id.len(), 12);
        let db = create_in(&dirs, "db", None, &routes).unwrap();
        assert_eq!(db.subnet, "10.78.2.0/24");
        assert_ne!(db.bridge, app.bridge);
        let own = create_in(&dirs, "own", Some("192.168.50.0/24"), &[]).unwrap();
        assert_eq!(own.gateway, "192.168.50.1");
        assert_eq!(
            create_in(&dirs, "app", None, &[]).unwrap_err().kind,
            ErrorKind::Conflict
        );
        for reserved in ["bridge", "host", "none"] {
            assert!(create_in(&dirs, reserved, None, &[]).is_err(), "{reserved}");
        }
        let names: Vec<String> = list_in(&dirs)
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(names, ["app", "bridge", "db", "own"]);
    }

    #[test]
    fn host_routes_are_read() {
        let text = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                    eth0\t00000000\t0205A8C0\t0003\t0\t0\t200\t00000000\t0\t0\t0\n\
                    eth0\t0005A8C0\t00000000\t0001\t0\t0\t200\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_routes(text), [(Ipv4Addr::new(192, 168, 5, 0), 24)]);
    }

    #[test]
    fn create_checks_the_default_network_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let e = create_in(&dirs, "app", Some("10.77.1.0/24"), &[]).unwrap_err();
        assert!(e.why.contains("bridge"), "{}", e.why);
    }

    #[test]
    fn default_network_refuses_a_host_route_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let routes = [(Ipv4Addr::new(10, 77, 4, 0), 24)];
        let _lock = lock(&dirs).unwrap();
        let e = load_with(&dirs, DEFAULT, &routes)
            .err()
            .expect("an overlap error");
        assert!(e.why.contains("10.77.4.0/24"), "{}", e.why);
        assert!(e.next.contains("default_subnet"), "{}", e.next);
    }

    #[test]
    fn own_bridges_are_not_host_routes() {
        let text = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                    openpod0\t00004D0A\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n\
                    opd-1a2b3c4d\t00004E0A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
                    eth0\t0005A8C0\t00000000\t0001\t0\t0\t200\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_routes(text), [(Ipv4Addr::new(192, 168, 5, 0), 24)]);
    }

    #[test]
    fn bridge_name_follows_the_subnet() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let a = create_in(&dirs, "a1", None, &[]).unwrap();
        remove_in(&dirs, "a1").unwrap();
        let b = create_in(&dirs, "b1", None, &[]).unwrap();
        assert_eq!(a.subnet, b.subnet);
        assert_eq!(a.bridge, b.bridge, "a reused subnet reuses its bridge");
        let c = create_in(&dirs, "c1", None, &[]).unwrap();
        assert_ne!(c.bridge, b.bridge);
    }

    #[test]
    fn overlapping_subnets_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "a1", Some("172.30.0.0/16"), &[]).unwrap();
        let e = create_in(&dirs, "a2", Some("172.30.5.0/24"), &[]).unwrap_err();
        assert!(e.why.contains("a1"), "{}", e.why);
        let host = [(Ipv4Addr::new(192, 168, 0, 0), 16)];
        let e = create_in(&dirs, "a3", Some("192.168.9.0/24"), &host).unwrap_err();
        assert!(e.why.contains("route"), "{}", e.why);
        for bad in [
            "10.0.0.1/24",
            "10.0.0.0",
            "10.0.0.0/33",
            "300.0.0.0/8",
            "10.0.0.0/31",
        ] {
            assert!(create_in(&dirs, "bad", Some(bad), &[]).is_err(), "{bad}");
        }
    }

    #[test]
    fn allocations_skip_used_addresses_and_gateway() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "tiny", Some("10.200.0.0/29"), &[]).unwrap();
        let ip = |id: &str| allocate(&dirs, "tiny", id, id, id).unwrap();
        assert_eq!(ip("a"), Ipv4Addr::new(10, 200, 0, 2));
        assert_eq!(ip("b"), Ipv4Addr::new(10, 200, 0, 3));
        assert_eq!(
            ip("a"),
            Ipv4Addr::new(10, 200, 0, 2),
            "same container, same address"
        );
        release(&dirs, "tiny", "a").unwrap();
        assert_eq!(
            ip("c"),
            Ipv4Addr::new(10, 200, 0, 2),
            "freed address reused"
        );
        for id in ["d", "e", "f"] {
            ip(id);
        }
        // .1 gateway, .2-.6 used, .7 broadcast: full.
        let e = allocate(&dirs, "tiny", "h", "h", "h").unwrap_err();
        assert!(e.why.contains("no free address"), "{}", e.why);
        let members = members(&dirs, "tiny").unwrap();
        assert_eq!(members.len(), 5);
        assert!(members.contains(&("c".to_string(), Ipv4Addr::new(10, 200, 0, 2))));
    }

    #[test]
    fn concurrent_allocations_are_unique() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        ensure_default(&dirs).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let dirs = dirs.clone();
                std::thread::spawn(move || {
                    allocate(&dirs, "bridge", &format!("c{i}"), &format!("c{i}"), "h").unwrap()
                })
            })
            .collect();
        let mut ips: Vec<Ipv4Addr> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        ips.sort();
        ips.dedup();
        assert_eq!(ips.len(), 8);
    }

    #[test]
    fn network_in_use_is_not_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "app", None, &[]).unwrap();
        let mut spec = crate::spec::spec_fixture();
        spec.name = "web".into();
        spec.network = NetworkMode::Bridge {
            network: "app".into(),
        };
        crate::state::create(&dirs, &spec).unwrap();
        allocate(&dirs, "app", &spec.id, "web", &spec.hostname).unwrap();
        let e = remove_in(&dirs, "app").unwrap_err();
        assert!(e.why.contains("web"), "{}", e.why);
        crate::state::remove_container_dir(&dirs, &spec.id).unwrap();
        remove_in(&dirs, "app").unwrap();
        assert_eq!(get_in(&dirs, "app").unwrap_err().kind, ErrorKind::NotFound);
    }

    /// A container `name` (ID: its first letter in hex, 32 times) on network `app`, with an empty
    /// hosts file as its shim would have made, and its address.
    fn member(dirs: &Dirs, name: &str, hostname: &str) -> (String, Ipv4Addr) {
        let mut spec = crate::spec::spec_fixture();
        spec.id = format!("{:02x}", name.bytes().next().unwrap_or(0)).repeat(32);
        spec.name = name.into();
        spec.hostname = hostname.into();
        spec.network = NetworkMode::Bridge {
            network: "app".into(),
        };
        crate::state::create(dirs, &spec).unwrap();
        std::fs::write(dirs.container_dir(&spec.id).join("hosts"), "").unwrap();
        let ip = allocate(dirs, "app", &spec.id, name, hostname).unwrap();
        (spec.id, ip)
    }

    fn hosts_of(dirs: &Dirs, id: &str) -> String {
        std::fs::read_to_string(dirs.container_dir(id).join("hosts")).unwrap_or_default()
    }

    #[test]
    fn refresh_does_not_need_the_members_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "app", None, &[]).unwrap();
        let (web, web_ip) = member(&dirs, "web", "front");
        let (db, db_ip) = member(&dirs, "db", "db");
        // The record alone says what goes into the files.
        for id in [&web, &db] {
            std::fs::remove_file(dirs.container_dir(id).join("config.json")).unwrap();
        }
        refresh_hosts(&dirs, "app");
        let local = "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n";
        assert_eq!(
            hosts_of(&dirs, &web),
            format!("{local}{web_ip}\tfront web\n{db_ip}\tdb\n")
        );
        assert_eq!(
            hosts_of(&dirs, &db),
            format!("{local}{db_ip}\tdb\n{web_ip}\tweb\n")
        );
    }

    #[test]
    fn a_member_of_an_older_record_gets_its_hostname_from_its_config() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "app", None, &[]).unwrap();
        let (web, web_ip) = member(&dirs, "web", "front");
        let (db, db_ip) = member(&dirs, "db", "db");
        // The record as an earlier version wrote it: members without a hostname.
        let path = record_path(&dirs, "app");
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for m in json["containers"].as_object_mut().unwrap().values_mut() {
            m.as_object_mut().unwrap().remove("hostname");
        }
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        refresh_hosts(&dirs, "app");
        assert!(hosts_of(&dirs, &web).contains(&format!("{web_ip}\tfront web\n")));
        assert!(hosts_of(&dirs, &web).contains(&format!("{db_ip}\tdb\n")));
        assert!(hosts_of(&dirs, &db).contains(&format!("{web_ip}\tweb\n")));
        // Its next start writes the hostname down, and keeps the address.
        assert_eq!(
            allocate(&dirs, "app", &web, "web", "front").unwrap(),
            web_ip
        );
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["containers"][&web]["hostname"], "front");
    }

    #[test]
    fn a_member_without_a_hosts_file_does_not_stop_the_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "app", None, &[]).unwrap();
        let (a, a_ip) = member(&dirs, "a", "a");
        let (b, _) = member(&dirs, "b", "b");
        let (c, c_ip) = member(&dirs, "c", "c");
        // One has a mount of its own over /etc/hosts (no file), one has no directory.
        std::fs::remove_file(dirs.container_dir(&a).join("hosts")).unwrap();
        std::fs::remove_dir_all(dirs.container_dir(&b)).unwrap();
        refresh_hosts(&dirs, "app");
        assert!(!dirs.container_dir(&a).join("hosts").exists());
        assert!(hosts_of(&dirs, &c).contains(&format!("{c_ip}\tc\n")));
        assert!(hosts_of(&dirs, &c).contains(&format!("{a_ip}\ta\n")));
    }

    #[test]
    fn hosts_follow_the_record_after_starts_and_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "app", None, &[]).unwrap();
        let (a, a_ip) = member(&dirs, "a", "a");
        let (b, b_ip) = member(&dirs, "b", "b");
        refresh_hosts(&dirs, "app");
        let (c, c_ip) = member(&dirs, "c", "c");
        refresh_hosts(&dirs, "app");
        let all = format!("{a_ip}\ta\n{b_ip}\tb\n{c_ip}\tc\n");
        for id in [&a, &b, &c] {
            let text = hosts_of(&dirs, id);
            for line in all.lines() {
                assert_eq!(text.matches(line).count(), 1, "{line} in {text}");
            }
        }
        // One leaves: the others forget its name.
        release(&dirs, "app", &b).unwrap();
        refresh_hosts(&dirs, "app");
        for id in [&a, &c] {
            let text = hosts_of(&dirs, id);
            assert!(!text.contains("\tb\n"), "{text}");
            assert!(
                text.contains(&format!("{a_ip}\ta\n")) && text.contains(&format!("{c_ip}\tc\n"))
            );
        }
        // The default network has no names.
        refresh_hosts(&dirs, DEFAULT);
    }

    /// Not a test: how long the network work of one start takes with 250 members.
    /// `cargo test --lib bench_250_members -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement"]
    fn bench_250_members() {
        let base = std::env::var("BENCH_DIR").unwrap_or_else(|_| "/var/tmp".into());
        let tmp = tempfile::tempdir_in(base).unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, "app", Some("10.94.0.0/22"), &[]).unwrap();
        let mut spec = crate::spec::spec_fixture();
        spec.network = NetworkMode::Bridge {
            network: "app".into(),
        };
        for i in 0..250 {
            spec.id = format!("{i:064x}");
            spec.name = format!("c{i}");
            spec.hostname = spec.name.clone();
            crate::state::create(&dirs, &spec).unwrap();
            std::fs::write(dirs.container_dir(&spec.id).join("hosts"), "").unwrap();
            allocate(&dirs, "app", &spec.id, &spec.name, &spec.hostname).unwrap();
        }
        refresh_hosts(&dirs, "app");
        let time = |what: &str, f: &mut dyn FnMut()| {
            let mut best = std::time::Duration::MAX;
            for _ in 0..7 {
                let t = std::time::Instant::now();
                f();
                best = best.min(t.elapsed());
            }
            eprintln!("{what}: {:.2} ms", best.as_secs_f64() * 1000.0);
        };
        time("refresh_hosts", &mut || refresh_hosts(&dirs, "app"));
        time("members", &mut || drop(members(&dirs, "app").unwrap()));
        let mut n = 1000;
        time("allocate + release", &mut || {
            n += 1;
            let id = format!("{n:064x}");
            allocate(&dirs, "app", &id, "x", "x").unwrap();
            release(&dirs, "app", &id).unwrap();
        });
        time("get_in", &mut || drop(get_in(&dirs, "app").unwrap()));
    }

    #[test]
    fn default_network_cannot_be_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        ensure_default(&dirs).unwrap();
        assert_eq!(
            remove_in(&dirs, "bridge").unwrap_err().kind,
            ErrorKind::InvalidInput
        );
    }
}
