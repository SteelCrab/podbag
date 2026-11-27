//! The `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` a container gets. The shim
//! writes them into the container directory; the container mounts them over the image's
//! files.

use crate::network::NetworkMode;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};

/// What a container's `/etc/hosts` is made of.
#[derive(Debug, Clone)]
pub enum Own<'a> {
    /// A bridge container: its whole file, made from the network records
    /// (`network::hosts_text`).
    Bridge(String),
    /// The host's network: the host's file, the extra lines, then the hostname.
    Host(&'a str, &'a [HostEntry]),
    /// No network: loopback only, the extra lines, then the hostname.
    Loopback(&'a str, &'a [HostEntry]),
}

const LOCALHOST: &str = "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n";

/// `/etc/hosts`. `host_hosts` is the host's file, for the host's network.
pub fn hosts_file(own: &Own<'_>, host_hosts: &str) -> String {
    match own {
        Own::Bridge(text) => text.clone(),
        Own::Host(hostname, extra) => {
            let mut text = host_hosts.to_string();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            push_extra(&mut text, extra);
            text.push_str(&format!("127.0.1.1\t{hostname}\n"));
            text
        }
        Own::Loopback(hostname, extra) => {
            let mut text = LOCALHOST.to_string();
            push_extra(&mut text, extra);
            text.push_str(&format!("127.0.1.1\t{hostname}\n"));
            text
        }
    }
}

/// One `address<TAB>name` line for each extra entry, in the order given.
fn push_extra(text: &mut String, extra: &[HostEntry]) {
    use std::fmt::Write;
    for e in extra {
        let _ = writeln!(text, "{}\t{}", e.ip, e.name);
    }
}

/// The address of `--add-host NAME:ADDRESS` that means the host itself.
pub const HOST_GATEWAY: &str = "host-gateway";

/// The address of an extra `/etc/hosts` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAddress {
    Ip(IpAddr),
    /// `host-gateway`: the host, as the container reaches it. Found at each start.
    HostGateway,
}

/// An extra `/etc/hosts` line as asked for (`--add-host`, Docker's `ExtraHosts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraHost {
    pub name: String,
    pub address: HostAddress,
}

fn invalid(what: String, why: impl Into<String>, next: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, what, why, next)
}

impl ExtraHost {
    /// `NAME:ADDRESS` or `NAME=ADDRESS`, as Docker takes them. ADDRESS is an IPv4 or IPv6
    /// address (brackets allowed: `[::1]`) or `host-gateway`.
    pub fn parse(text: &str) -> Result<ExtraHost> {
        let bad = |why: &str| {
            invalid(
                format!("invalid extra host {text:?}"),
                why,
                "write it as NAME:IP, like db:10.0.0.5 or db=::1",
            )
        };
        let (name, address) = text
            .split_once('=')
            .or_else(|| text.split_once(':'))
            .ok_or_else(|| bad("it has no ':' or '=' between the name and the address"))?;
        check_host_name(name).map_err(&bad)?;
        let address = if address == HOST_GATEWAY {
            HostAddress::HostGateway
        } else {
            let bare = address
                .strip_prefix('[')
                .and_then(|a| a.strip_suffix(']'))
                .unwrap_or(address);
            HostAddress::Ip(
                bare.parse()
                    .map_err(|_| bad("the address is not an IPv4 or IPv6 address"))?,
            )
        };
        Ok(ExtraHost {
            name: name.to_string(),
            address,
        })
    }

    /// Check the name: it must fit on a hosts line.
    pub fn check(&self) -> Result<()> {
        check_host_name(&self.name).map_err(|why| {
            invalid(
                format!("invalid extra host name {:?}", self.name),
                why,
                "give a host name, like db or host.docker.internal",
            )
        })
    }

    /// Docker's form: `NAME:ADDRESS`.
    pub fn docker(&self) -> String {
        match self.address {
            HostAddress::Ip(ip) => format!("{}:{ip}", self.name),
            HostAddress::HostGateway => format!("{}:{HOST_GATEWAY}", self.name),
        }
    }
}

/// Why `name` cannot be the name of an extra hosts line, if it cannot.
fn check_host_name(name: &str) -> std::result::Result<(), &'static str> {
    if name.is_empty() {
        Err("the name is empty")
    } else if name.contains(|c: char| c == ':' || c == '#' || c.is_whitespace() || c.is_control()) {
        Err("a name has no ':', '#' or spaces")
    } else {
        Ok(())
    }
}

/// Check a container's extra hosts and DNS settings before it is made: names that fit,
/// and a `host-gateway` that podbag can find for `mode`.
pub fn check_settings(
    mode: &NetworkMode,
    rootless: bool,
    extra: &[ExtraHost],
    dns: &DnsSettings,
) -> Result<()> {
    extra.iter().try_for_each(ExtraHost::check)?;
    dns.check()?;
    if extra.iter().any(|e| e.address == HostAddress::HostGateway) {
        host_gateway(mode, rootless, Place::detect())?;
    }
    Ok(())
}

/// An extra `/etc/hosts` line with its address found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntry {
    pub name: String,
    pub ip: IpAddr,
}

/// What `podbag` runs on, for where `host-gateway` leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// A Linux system: the host is this system.
    Linux,
    /// The macOS VM: the host is the Mac.
    MacVm,
    /// WSL: the host is Windows.
    Wsl,
}

impl Place {
    /// Where this process runs.
    pub fn detect() -> Place {
        let cmdline = std::fs::read("/proc/1/cmdline").unwrap_or_default();
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        Place::from_facts(&cmdline, &release)
    }

    /// From PID 1's command line (`/proc/1/cmdline`) and the kernel release.
    fn from_facts(cmdline: &[u8], release: &str) -> Place {
        let mut args = cmdline.split(|&b| b == 0);
        if args.nth(1) == Some(crate::vm::proto::INIT_ARG.as_bytes()) {
            Place::MacVm
        } else if release.to_ascii_lowercase().contains("microsoft") {
            Place::Wsl
        } else {
            Place::Linux
        }
    }
}

/// Where `host-gateway` leads for a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostGateway {
    /// The gateway of its first network: the host's side of the bridge.
    Bridge,
    /// The loopback address: it shares the host's network.
    Loopback,
    /// The VM's default gateway: the Mac.
    VmRoute,
}

/// Where `host-gateway` leads for a container on `mode`, or why podbag cannot tell.
pub fn host_gateway(mode: &NetworkMode, rootless: bool, place: Place) -> Result<HostGateway> {
    let refuse = |why: &str| {
        Err(invalid(
            format!("cannot use {HOST_GATEWAY} in --add-host"),
            why,
            "give the host's address instead: --add-host NAME:IP",
        ))
    };
    match (mode, place) {
        (NetworkMode::None, _) => refuse("the container has no network (--network none)"),
        (_, Place::Wsl) => refuse("podbag cannot tell the Windows host's address in WSL"),
        (_, Place::MacVm) => Ok(HostGateway::VmRoute),
        (NetworkMode::Host, Place::Linux) => Ok(HostGateway::Loopback),
        (NetworkMode::Bridge { .. }, Place::Linux) if rootless => refuse(
            "a rootless bridge network has no address that leads to the host \
             (pasta keeps the host's addresses away from containers)",
        ),
        (NetworkMode::Bridge { .. }, Place::Linux) => Ok(HostGateway::Bridge),
    }
}

/// The gateway of the default route in `/proc/net/route`.
pub fn default_gateway(routes: &str) -> Option<Ipv4Addr> {
    routes.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let dest = u32::from_str_radix(f.get(1)?, 16).ok()?;
        let gateway = u32::from_str_radix(f.get(2)?, 16).ok()?;
        let mask = u32::from_str_radix(f.get(7)?, 16).ok()?;
        // Network-order bytes printed as a little-endian number (see `network::parse_routes`).
        (dest == 0 && mask == 0 && gateway != 0).then(|| Ipv4Addr::from(gateway.to_le_bytes()))
    })
}

/// The extra lines with their addresses: `host-gateway` becomes the address `gateway`
/// gives (asked only when an entry needs it).
pub fn resolve_extra_hosts(
    extra: &[ExtraHost],
    gateway: impl FnOnce() -> Result<IpAddr>,
) -> Result<Vec<HostEntry>> {
    let needed = extra.iter().any(|e| e.address == HostAddress::HostGateway);
    let gateway = needed.then(gateway).transpose()?;
    Ok(extra
        .iter()
        .filter_map(|e| {
            let ip = match e.address {
                HostAddress::Ip(ip) => ip,
                HostAddress::HostGateway => gateway?,
            };
            Some(HostEntry {
                name: e.name.clone(),
                ip,
            })
        })
        .collect())
}

/// DNS settings for a container's resolv.conf (`--dns`, `--dns-search`, `--dns-option`).
/// Each one given replaces what the container would get otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsSettings {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<IpAddr>,
    /// Search domains. `.` alone means none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

impl DnsSettings {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty() && self.search.is_empty() && self.options.is_empty()
    }

    /// Check the search domains and options (the servers are addresses already).
    pub fn check(&self) -> Result<()> {
        self.search.iter().try_for_each(|d| check_dns_search(d))?;
        self.options.iter().try_for_each(|o| check_dns_option(o))
    }
}

/// A `--dns` value: an IPv4 or IPv6 address.
pub fn parse_dns_server(text: &str) -> Result<IpAddr> {
    text.trim().parse().map_err(|_| {
        invalid(
            format!("invalid DNS server {text:?}"),
            "a DNS server is an IPv4 or IPv6 address",
            "give an address, like 1.1.1.1",
        )
    })
}

/// A `--dns-search` value: a domain name, or `.` for no search domain.
pub fn check_dns_search(domain: &str) -> Result<()> {
    if domain == "." {
        return Ok(());
    }
    let name = domain.strip_suffix('.').unwrap_or(domain);
    let label = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if !name.is_empty() && name.len() <= 253 && name.split('.').all(label) {
        Ok(())
    } else {
        Err(invalid(
            format!("invalid DNS search domain {domain:?}"),
            "a domain has parts of letters, digits and '-', joined by '.'",
            "give a domain like example.com, or . for none",
        ))
    }
}

/// A `--dns-option` value: one word, like `ndots:2`.
pub fn check_dns_option(option: &str) -> Result<()> {
    if !option.is_empty() && !option.contains(|c: char| c.is_whitespace() || c.is_control()) {
        Ok(())
    } else {
        Err(invalid(
            format!("invalid DNS option {option:?}"),
            "an option is one word without spaces",
            "give an option like ndots:2 or edns0",
        ))
    }
}

/// One container of a user network as the others see it: its address and names.
pub struct Peer<'a> {
    pub ip: Ipv4Addr,
    pub ip6: Option<std::net::Ipv6Addr>,
    pub name: &'a str,
    /// Further names on this network (`--network-alias`).
    pub aliases: &'a [String],
}

/// `names`, space-separated, each once, at the end of `text` (where the line's names
/// begin).
fn push_names<'a>(text: &mut String, names: impl Iterator<Item = &'a str>) {
    let start = text.len();
    for name in names {
        if text[start..].split(' ').any(|written| written == name) {
            continue;
        }
        if text.len() > start {
            text.push(' ');
        }
        text.push_str(name);
    }
}

/// One `address<TAB>name aliases…` line for each container of a user network: the part
/// of `/etc/hosts` that all of them share. Made once for a refresh of all their files.
pub struct PeerBlock {
    text: String,
    /// Where each peer's line ends in `text`, in the order of the peers.
    ends: Vec<usize>,
}

impl PeerBlock {
    pub fn new(peers: &[Peer<'_>]) -> PeerBlock {
        use std::fmt::Write;
        let mut text = String::with_capacity(peers.len() * 32);
        let mut ends = Vec::with_capacity(peers.len());
        for p in peers {
            for ip in std::iter::once(IpAddr::from(p.ip)).chain(p.ip6.map(IpAddr::from)) {
                let _ = write!(text, "{ip}\t");
                push_names(
                    &mut text,
                    std::iter::once(p.name).chain(p.aliases.iter().map(String::as_str)),
                );
                text.push('\n');
            }
            ends.push(text.len());
        }
        PeerBlock { text, ends }
    }

    /// The block without the line of the peer at `place` (its place in the list given to
    /// `new`), in two pieces. A place that is not in the list leaves every line in.
    fn without(&self, place: Option<usize>) -> (&str, &str) {
        let Some(end) = place.and_then(|i| self.ends.get(i).copied()) else {
            return (&self.text, "");
        };
        let start = place
            .and_then(|i| i.checked_sub(1))
            .and_then(|before| self.ends.get(before).copied())
            .unwrap_or(0);
        (
            self.text.get(..start).unwrap_or_default(),
            self.text.get(end..).unwrap_or_default(),
        )
    }
}

/// `/etc/hosts` of a container on bridge networks: the localhost lines, the extra lines
/// (`--add-host`, before its own as with Docker), a line of its own on each network
/// (`own`: its address there and its aliases there, its primary network first: the
/// hostname resolves to that one), then the other containers of each of its user networks
/// (`lists`: the network's block and the container's own place in it).
pub fn bridge_hosts(
    hostname: &str,
    name: &str,
    extra: &[HostEntry],
    own: &[(IpAddr, &[String])],
    lists: &[(&PeerBlock, Option<usize>)],
) -> String {
    use std::fmt::Write;
    let shared: usize = lists.iter().map(|(block, _)| block.text.len()).sum();
    let mut text = String::with_capacity(LOCALHOST.len() + 64 * (own.len() + extra.len()) + shared);
    text.push_str(LOCALHOST);
    push_extra(&mut text, extra);
    for (ip, aliases) in own {
        let _ = write!(text, "{ip}\t");
        push_names(
            &mut text,
            [hostname, name]
                .into_iter()
                .chain(aliases.iter().map(String::as_str)),
        );
        text.push('\n');
    }
    for (block, place) in lists {
        let (before, after) = block.without(*place);
        text.push_str(before);
        text.push_str(after);
    }
    text
}

/// The DNS server pasta answers on in a rootless network (`--dns-forward`).
pub const PASTA_DNS: &str = "169.254.1.1";

fn is_loopback(server: &str) -> bool {
    server.starts_with("127.") || server == "::1"
}

/// The name servers of a resolv.conf.
pub(crate) fn nameservers(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "nameserver")
            .then(|| fields.next())
            .flatten()
    })
}

/// Name servers in a resolv.conf that a container can reach (not loopback).
fn servers(text: &str) -> Vec<String> {
    nameservers(text)
        .filter(|s| !is_loopback(s))
        .map(String::from)
        .collect()
}

/// `/etc/resolv.conf`. `host` is the host's file; `systemd` is
/// `/run/systemd/resolve/resolv.conf`, the upstream servers behind the 127.0.0.53 stub.
/// Each setting in `dns` replaces its part of the file, as with Docker.
pub fn resolv_conf(
    mode: &NetworkMode,
    rootless: bool,
    host: &str,
    systemd: Option<&str>,
    dns: &DnsSettings,
) -> String {
    use std::fmt::Write;
    let shared = matches!(mode, NetworkMode::Host | NetworkMode::None);
    if shared && dns.is_empty() {
        return host.to_string();
    }
    let list: Vec<String> = if !dns.servers.is_empty() {
        dns.servers.iter().map(IpAddr::to_string).collect()
    } else if shared {
        // The host's network: its own servers, the local stub too.
        nameservers(host).map(String::from).collect()
    } else {
        default_servers(rootless, host, systemd)
    };
    let replaced = |line: &str| match line.split_whitespace().next() {
        Some("nameserver") => true,
        Some("search" | "domain") => !dns.search.is_empty(),
        Some("options") => !dns.options.is_empty(),
        _ => false,
    };
    let mut text: String = list.iter().map(|s| format!("nameserver {s}\n")).collect();
    for line in host.lines().filter(|l| !replaced(l)) {
        let _ = writeln!(text, "{line}");
    }
    if !dns.search.is_empty() && dns.search != ["."] {
        let _ = writeln!(text, "search {}", dns.search.join(" "));
    }
    if !dns.options.is_empty() {
        let _ = writeln!(text, "options {}", dns.options.join(" "));
    }
    text
}

/// The name servers a bridge container gets without `--dns`: pasta's forwarder when
/// rootless; else the host's servers it can reach, those behind systemd-resolved's stub,
/// or Docker's last resort.
fn default_servers(rootless: bool, host: &str, systemd: Option<&str>) -> Vec<String> {
    if rootless {
        return vec![PASTA_DNS.to_string()];
    }
    let mut list = servers(host);
    if list.is_empty() {
        list = systemd.map(servers).unwrap_or_default();
    }
    if list.is_empty() {
        list = vec!["8.8.8.8".to_string(), "8.8.4.4".to_string()];
    }
    list
}

/// Replace a file's content keeping its inode: a container has it bind-mounted, so a new
/// file under the same name would not reach the container.
pub fn rewrite_in_place(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    // Over the old content, and cut only what is left of a longer one: emptying the file
    // first costs several times more on a disk, and a reader would find it empty.
    let old = file.metadata()?.len();
    file.write_all(content.as_bytes())?;
    if old > content.len() as u64 {
        file.set_len(content.len() as u64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n";

    fn peer<'a>(name: &'a str, ip: [u8; 4], aliases: &'a [String]) -> Peer<'a> {
        Peer {
            ip: Ipv4Addr::from(ip),
            ip6: None,
            name,
            aliases,
        }
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hosts_lines_carry_aliases() {
        // Its name once, each alias once.
        let web = strings(&["front", "web", "front"]);
        let peers = [
            peer("db", [10, 78, 1, 2], &[]),
            peer("web", [10, 78, 1, 3], &web),
        ];
        let block = PeerBlock::new(&peers);
        let own = Ipv4Addr::new(10, 78, 1, 2);
        assert_eq!(
            bridge_hosts("db", "db", &[], &[(own.into(), &[])], &[(&block, Some(0))]),
            format!("{LOCAL}10.78.1.2\tdb\n10.78.1.3\tweb front\n")
        );
    }

    #[test]
    fn the_own_line_carries_the_own_aliases() {
        let aliases = strings(&["api", "abc123", "web", "api"]);
        let peers = [peer("web", [10, 78, 1, 3], &aliases)];
        let block = PeerBlock::new(&peers);
        let own = Ipv4Addr::new(10, 78, 1, 3);
        assert_eq!(
            bridge_hosts(
                "abc123",
                "web",
                &[],
                &[(own.into(), &aliases)],
                &[(&block, Some(0))]
            ),
            format!("{LOCAL}10.78.1.3\tabc123 web api\n")
        );
    }

    #[test]
    fn a_container_on_two_networks_has_a_line_on_each_and_both_lists() {
        let w = strings(&["w"]);
        let front = [
            peer("web", [10, 1, 0, 2], &[]),
            peer("db", [10, 1, 0, 3], &[]),
        ];
        let back = [
            peer("cache", [10, 2, 0, 2], &[]),
            peer("web", [10, 2, 0, 3], &w),
        ];
        let (front, back) = (PeerBlock::new(&front), PeerBlock::new(&back));
        let own = [
            (IpAddr::from([10, 1, 0, 2]), &[][..]),
            (IpAddr::from([10, 2, 0, 3]), &w[..]),
        ];
        assert_eq!(
            bridge_hosts(
                "web",
                "web",
                &[],
                &own,
                &[(&front, Some(0)), (&back, Some(1))]
            ),
            format!("{LOCAL}10.1.0.2\tweb\n10.2.0.3\tweb w\n10.1.0.3\tdb\n10.2.0.2\tcache\n")
        );
    }

    #[test]
    fn a_place_outside_the_block_leaves_every_peer_in() {
        let peers = [peer("db", [10, 78, 1, 2], &[])];
        let block = PeerBlock::new(&peers);
        let ip = Ipv4Addr::new(10, 78, 1, 9);
        let all = format!("{LOCAL}10.78.1.9\tnew\n10.78.1.2\tdb\n");
        assert_eq!(
            bridge_hosts("new", "new", &[], &[(ip.into(), &[])], &[(&block, Some(7))]),
            all
        );
        assert_eq!(
            bridge_hosts("new", "new", &[], &[(ip.into(), &[])], &[(&block, None)]),
            all
        );
        let none = PeerBlock::new(&[]);
        assert_eq!(
            bridge_hosts("a", "a", &[], &[(ip.into(), &[])], &[(&none, Some(0))]),
            format!("{LOCAL}10.78.1.9\ta\n")
        );
    }

    #[test]
    fn the_own_line_comes_first() {
        // Two containers with one hostname: each resolves it to itself.
        let peers = [
            peer("a", [10, 78, 1, 2], &[]),
            peer("b", [10, 78, 1, 3], &[]),
        ];
        let own = Ipv4Addr::new(10, 78, 1, 3);
        let text = bridge_hosts(
            "same",
            "b",
            &[],
            &[(own.into(), &[])],
            &[(&PeerBlock::new(&peers), Some(1))],
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.get(2), Some(&"10.78.1.3\tsame b"), "{text}");
        assert_eq!(lines.get(3), Some(&"10.78.1.2\ta"), "{text}");
        assert_eq!(lines.len(), 4, "{text}");
    }

    #[test]
    fn hosts_for_host_and_none() {
        let host = "127.0.0.1 localhost\n192.168.5.15 lima\n";
        let text = hosts_file(&Own::Host("box", &[]), host);
        assert!(text.starts_with(host), "{text}");
        assert!(text.ends_with("127.0.1.1\tbox\n"), "{text}");
        let text = hosts_file(&Own::Loopback("box", &[]), host);
        assert_eq!(text, format!("{LOCAL}127.0.1.1\tbox\n"));
        // A bridge container's file is made from the network records.
        let made = "made\n".to_string();
        assert_eq!(hosts_file(&Own::Bridge(made.clone()), host), made);
    }

    #[test]
    fn resolv_conf_by_mode() {
        let none = DnsSettings::default();
        let host = "nameserver 127.0.0.53\noptions edns0 trust-ad\nsearch lan\n";
        let upstream = "nameserver 192.168.5.3\nnameserver 1.1.1.1\n";
        let bridge = NetworkMode::default();
        assert_eq!(
            resolv_conf(&bridge, true, host, Some(upstream), &none),
            "nameserver 169.254.1.1\noptions edns0 trust-ad\nsearch lan\n"
        );
        assert_eq!(
            resolv_conf(&bridge, false, host, Some(upstream), &none),
            "nameserver 192.168.5.3\nnameserver 1.1.1.1\noptions edns0 trust-ad\nsearch lan\n"
        );
        assert_eq!(
            resolv_conf(&bridge, false, "nameserver 127.0.0.53\n", None, &none),
            "nameserver 8.8.8.8\nnameserver 8.8.4.4\n"
        );
        assert_eq!(
            resolv_conf(&NetworkMode::Host, true, host, None, &none),
            host
        );
        assert_eq!(
            resolv_conf(
                &NetworkMode::None,
                false,
                "nameserver 10.0.0.1\n",
                None,
                &none
            ),
            "nameserver 10.0.0.1\n"
        );
    }

    fn dns(servers: &[&str], search: &[&str], options: &[&str]) -> DnsSettings {
        DnsSettings {
            servers: servers.iter().map(|s| s.parse().unwrap()).collect(),
            search: strings(search),
            options: strings(options),
        }
    }

    #[test]
    fn given_dns_settings_replace_their_part() {
        let host =
            "# made by hand\nnameserver 127.0.0.53\noptions edns0\nsearch lan\ndomain home\n";
        let bridge = NetworkMode::default();
        // Servers only: the host's search and options stay. Rootless too: pasta's
        // forwarder is replaced.
        for rootless in [false, true] {
            assert_eq!(
                resolv_conf(
                    &bridge,
                    rootless,
                    host,
                    None,
                    &dns(&["9.9.9.9", "2620:fe::fe"], &[], &[])
                ),
                "nameserver 9.9.9.9\nnameserver 2620:fe::fe\n# made by hand\noptions edns0\nsearch lan\ndomain home\n"
            );
        }
        // Search and options only: the default servers stay.
        assert_eq!(
            resolv_conf(
                &bridge,
                true,
                host,
                None,
                &dns(&[], &["a.example", "b.example"], &["ndots:2", "edns0"])
            ),
            "nameserver 169.254.1.1\n# made by hand\nsearch a.example b.example\noptions ndots:2 edns0\n"
        );
        // `.` means no search domain at all.
        assert_eq!(
            resolv_conf(
                &bridge,
                false,
                "nameserver 10.0.0.1\nsearch lan\n",
                None,
                &dns(&[], &["."], &[])
            ),
            "nameserver 10.0.0.1\n"
        );
    }

    #[test]
    fn the_host_network_keeps_the_host_file_but_for_given_settings() {
        let host = "nameserver 127.0.0.53\nsearch lan\n";
        // Without settings: the host's file as it is.
        assert_eq!(
            resolv_conf(
                &NetworkMode::Host,
                false,
                host,
                None,
                &DnsSettings::default()
            ),
            host
        );
        // The host's own stub stays usable on its network: it is kept when only the
        // search domains change.
        assert_eq!(
            resolv_conf(
                &NetworkMode::Host,
                true,
                host,
                None,
                &dns(&[], &["corp"], &[])
            ),
            "nameserver 127.0.0.53\nsearch corp\n"
        );
        assert_eq!(
            resolv_conf(
                &NetworkMode::Host,
                false,
                host,
                None,
                &dns(&["1.1.1.1"], &[], &[])
            ),
            "nameserver 1.1.1.1\nsearch lan\n"
        );
        assert_eq!(
            resolv_conf(
                &NetworkMode::None,
                false,
                host,
                None,
                &dns(&["1.1.1.1"], &[], &[])
            ),
            "nameserver 1.1.1.1\nsearch lan\n"
        );
    }

    fn entry(name: &str, ip: &str) -> HostEntry {
        HostEntry {
            name: name.into(),
            ip: ip.parse().unwrap(),
        }
    }

    #[test]
    fn extra_hosts_come_before_the_own_lines() {
        let extra = [entry("db", "10.1.2.3"), entry("v6", "fd00::1")];
        let peers = [peer("web", [10, 78, 1, 3], &[])];
        let block = PeerBlock::new(&peers);
        let own = Ipv4Addr::new(10, 78, 1, 2);
        assert_eq!(
            bridge_hosts("me", "me", &extra, &[(own.into(), &[])], &[(&block, None)]),
            format!("{LOCAL}10.1.2.3\tdb\nfd00::1\tv6\n10.78.1.2\tme\n10.78.1.3\tweb\n")
        );
        assert_eq!(
            hosts_file(&Own::Loopback("box", &extra), ""),
            format!("{LOCAL}10.1.2.3\tdb\nfd00::1\tv6\n127.0.1.1\tbox\n")
        );
        assert_eq!(
            hosts_file(&Own::Host("box", &extra), "127.0.0.1 localhost"),
            "127.0.0.1 localhost\n10.1.2.3\tdb\nfd00::1\tv6\n127.0.1.1\tbox\n"
        );
    }

    #[test]
    fn extra_hosts_parse_as_docker_takes_them() {
        let ip = |name: &str, ip: &str| ExtraHost {
            name: name.into(),
            address: HostAddress::Ip(ip.parse().unwrap()),
        };
        for (text, want) in [
            ("db:10.1.2.3", ip("db", "10.1.2.3")),
            ("db=10.1.2.3", ip("db", "10.1.2.3")),
            ("v6:::1", ip("v6", "::1")),
            ("v6=::1", ip("v6", "::1")),
            ("v6=[2001:db8::33]", ip("v6", "2001:db8::33")),
            ("v6:[2001:db8::33]", ip("v6", "2001:db8::33")),
            ("v4=[10.0.0.1]", ip("v4", "10.0.0.1")),
            (
                "host.docker.internal:host-gateway",
                ExtraHost {
                    name: "host.docker.internal".into(),
                    address: HostAddress::HostGateway,
                },
            ),
        ] {
            let got = ExtraHost::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(got, want, "{text}");
        }
        assert_eq!(ExtraHost::parse("v6=::1").unwrap().docker(), "v6:::1");
        assert_eq!(
            ExtraHost::parse("gw=host-gateway").unwrap().docker(),
            "gw:host-gateway"
        );
        for bad in [
            "db",
            ":10.0.0.1",
            "=10.0.0.1",
            "db:",
            "db:not-an-ip",
            "db:10.0.0.256",
            "db:[10.0.0.1",
            "a b:10.0.0.1",
            "a#b:10.0.0.1",
            "db:10.0.0.1 x",
        ] {
            let e = ExtraHost::parse(bad).expect_err(bad);
            assert_eq!(e.kind, ErrorKind::InvalidInput, "{bad}");
        }
    }

    #[test]
    fn dns_values_are_checked() {
        assert_eq!(
            parse_dns_server("9.9.9.9").unwrap(),
            "9.9.9.9".parse::<IpAddr>().unwrap()
        );
        assert!(parse_dns_server("2620:fe::fe").is_ok());
        for bad in ["", "dns.example", "1.2.3", "::g"] {
            assert!(parse_dns_server(bad).is_err(), "{bad}");
        }
        for good in [".", "example.com", "example.com.", "a-b.c1", "local"] {
            assert!(check_dns_search(good).is_ok(), "{good}");
        }
        for bad in ["", "..", "a..b", "-a.com", "a-.com", "a b", "a_b.com"] {
            assert!(check_dns_search(bad).is_err(), "{bad}");
        }
        for good in ["ndots:2", "edns0", "timeout:1"] {
            assert!(check_dns_option(good).is_ok(), "{good}");
        }
        for bad in ["", "ndots:2 edns0", "a\nnameserver 1.2.3.4"] {
            assert!(check_dns_option(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_gateway_by_network_and_place() {
        let bridge = NetworkMode::default();
        let ok = |mode: &NetworkMode, rootless: bool, place: Place| {
            host_gateway(mode, rootless, place).ok()
        };
        assert_eq!(ok(&bridge, false, Place::Linux), Some(HostGateway::Bridge));
        assert_eq!(ok(&bridge, true, Place::Linux), None, "rootless: pasta");
        assert_eq!(
            ok(&NetworkMode::Host, true, Place::Linux),
            Some(HostGateway::Loopback)
        );
        assert_eq!(ok(&bridge, false, Place::MacVm), Some(HostGateway::VmRoute));
        assert_eq!(
            ok(&NetworkMode::Host, false, Place::MacVm),
            Some(HostGateway::VmRoute)
        );
        for place in [Place::Linux, Place::MacVm, Place::Wsl] {
            assert_eq!(ok(&NetworkMode::None, false, place), None);
        }
        assert_eq!(ok(&bridge, false, Place::Wsl), None);
        assert_eq!(ok(&NetworkMode::Host, false, Place::Wsl), None);
        let e = host_gateway(&bridge, true, Place::Linux).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert!(e.to_string().contains("--add-host NAME:IP"), "{e}");
    }

    #[test]
    fn the_place_comes_from_pid_1_and_the_kernel() {
        assert_eq!(
            Place::from_facts(b"/init\0__vm-init\0", "6.12.0"),
            Place::MacVm
        );
        assert_eq!(
            Place::from_facts(b"/sbin/init\0", "5.15.167.4-microsoft-standard-WSL2"),
            Place::Wsl
        );
        assert_eq!(
            Place::from_facts(b"/sbin/init\0splash\0", "6.8.0-45-generic"),
            Place::Linux
        );
        assert_eq!(Place::from_facts(b"", ""), Place::Linux);
    }

    #[test]
    fn the_default_route_gives_the_gateway() {
        let routes = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            eth0\t0040A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
            eth0\t00000000\t0140A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n";
        assert_eq!(
            default_gateway(routes),
            Some(Ipv4Addr::new(192, 168, 64, 1))
        );
        assert_eq!(default_gateway("Iface\tDestination\n"), None);
    }

    #[test]
    fn host_gateway_is_asked_once_and_only_when_needed() {
        let parsed = |list: &[&str]| -> Vec<ExtraHost> {
            list.iter().map(|t| ExtraHost::parse(t).unwrap()).collect()
        };
        let never = || -> Result<IpAddr> { panic!("not needed") };
        assert_eq!(
            resolve_extra_hosts(&parsed(&["db:10.0.0.5"]), never).unwrap(),
            [entry("db", "10.0.0.5")]
        );
        let mut asked = 0;
        let got = resolve_extra_hosts(
            &parsed(&["a:host-gateway", "db:10.0.0.5", "b:host-gateway"]),
            || {
                asked += 1;
                Ok("10.77.0.1".parse().unwrap())
            },
        )
        .unwrap();
        assert_eq!(
            got,
            [
                entry("a", "10.77.0.1"),
                entry("db", "10.0.0.5"),
                entry("b", "10.77.0.1")
            ]
        );
        assert_eq!(asked, 1);
        let refused = resolve_extra_hosts(&parsed(&["a:host-gateway"]), || {
            Err(invalid("no".into(), "no", "no"))
        });
        assert!(refused.is_err());
    }

    #[test]
    fn settings_round_trip_and_stay_out_when_empty() {
        let e = ExtraHost::parse("gw:host-gateway").unwrap();
        let back: ExtraHost = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
        let d = dns(&["1.1.1.1"], &["lan"], &["ndots:1"]);
        let back: DnsSettings = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
        assert_eq!(
            serde_json::to_string(&DnsSettings::default()).unwrap(),
            "{}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_keeps_the_inode() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hosts");
        std::fs::write(&path, "a much longer first version\n").unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();
        rewrite_in_place(&path, "short\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short\n");
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), before);
        // Longer again, and the same length.
        rewrite_in_place(&path, "a longer text than before\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a longer text than before\n"
        );
        rewrite_in_place(&path, "A LONGER TEXT THAN BEFORE\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "A LONGER TEXT THAN BEFORE\n"
        );
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), before);
    }
}
