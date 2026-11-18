//! The `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` a container gets (design
//! 6.4.3 step 4). The shim writes them into the container directory; the container
//! mounts them over the image's files.

use crate::network::NetworkMode;
use std::net::Ipv4Addr;

/// What a container's `/etc/hosts` is made of.
#[derive(Debug, Clone)]
pub enum Own<'a> {
    /// A bridge container: its whole file, made from the network records
    /// (`network::hosts_text`).
    Bridge(String),
    /// The host's network: the host's file, plus the hostname.
    Host(&'a str),
    /// No network: loopback only, plus the hostname.
    Loopback(&'a str),
}

const LOCALHOST: &str = "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n";

/// `/etc/hosts`. `host_hosts` is the host's file, for the host's network.
pub fn hosts_file(own: &Own<'_>, host_hosts: &str) -> String {
    match own {
        Own::Bridge(text) => text.clone(),
        Own::Host(hostname) => {
            let mut text = host_hosts.to_string();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("127.0.1.1\t{hostname}\n"));
            text
        }
        Own::Loopback(hostname) => format!("{LOCALHOST}127.0.1.1\t{hostname}\n"),
    }
}

/// One container of a user network as the others see it: its address and names.
pub struct Peer<'a> {
    pub ip: Ipv4Addr,
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
            let _ = write!(text, "{}\t", p.ip);
            push_names(
                &mut text,
                std::iter::once(p.name).chain(p.aliases.iter().map(String::as_str)),
            );
            text.push('\n');
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

/// `/etc/hosts` of a container on bridge networks: the localhost lines, a line of its own
/// on each network (`own`: its address there and its aliases there, its primary network
/// first: the hostname resolves to that one), then the other containers of each of its
/// user networks (`lists`: the network's block and the container's own place in it).
pub fn bridge_hosts(
    hostname: &str,
    name: &str,
    own: &[(Ipv4Addr, &[String])],
    lists: &[(&PeerBlock, Option<usize>)],
) -> String {
    use std::fmt::Write;
    let shared: usize = lists.iter().map(|(block, _)| block.text.len()).sum();
    let mut text = String::with_capacity(LOCALHOST.len() + 64 * own.len() + shared);
    text.push_str(LOCALHOST);
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

/// Name servers in a resolv.conf that a container can reach (not loopback).
fn servers(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !is_loopback(s))
        .collect()
}

/// `/etc/resolv.conf`. `host` is the host's file; `systemd` is
/// `/run/systemd/resolve/resolv.conf`, the upstream servers behind the 127.0.0.53 stub.
pub fn resolv_conf(
    mode: &NetworkMode,
    rootless: bool,
    host: &str,
    systemd: Option<&str>,
) -> String {
    let others: String = host
        .lines()
        .filter(|l| !l.trim_start().starts_with("nameserver"))
        .map(|l| format!("{l}\n"))
        .collect();
    let list = match mode {
        NetworkMode::Host | NetworkMode::None => return host.to_string(),
        NetworkMode::Bridge { .. } if rootless => vec![PASTA_DNS.to_string()],
        NetworkMode::Bridge { .. } => {
            let mut list = servers(host);
            if list.is_empty() {
                list = systemd.map(servers).unwrap_or_default();
            }
            if list.is_empty() {
                // Docker's last resort.
                list = vec!["8.8.8.8".to_string(), "8.8.4.4".to_string()];
            }
            list
        }
    };
    let mut text: String = list.iter().map(|s| format!("nameserver {s}\n")).collect();
    text.push_str(&others);
    text
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
            bridge_hosts("db", "db", &[(own, &[])], &[(&block, Some(0))]),
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
            bridge_hosts("abc123", "web", &[(own, &aliases)], &[(&block, Some(0))]),
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
            (Ipv4Addr::new(10, 1, 0, 2), &[][..]),
            (Ipv4Addr::new(10, 2, 0, 3), &w[..]),
        ];
        assert_eq!(
            bridge_hosts("web", "web", &own, &[(&front, Some(0)), (&back, Some(1))]),
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
            bridge_hosts("new", "new", &[(ip, &[])], &[(&block, Some(7))]),
            all
        );
        assert_eq!(
            bridge_hosts("new", "new", &[(ip, &[])], &[(&block, None)]),
            all
        );
        let none = PeerBlock::new(&[]);
        assert_eq!(
            bridge_hosts("a", "a", &[(ip, &[])], &[(&none, Some(0))]),
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
            &[(own, &[])],
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
        let text = hosts_file(&Own::Host("box"), host);
        assert!(text.starts_with(host), "{text}");
        assert!(text.ends_with("127.0.1.1\tbox\n"), "{text}");
        let text = hosts_file(&Own::Loopback("box"), host);
        assert_eq!(text, format!("{LOCAL}127.0.1.1\tbox\n"));
        // A bridge container's file is made from the network records.
        let made = "made\n".to_string();
        assert_eq!(hosts_file(&Own::Bridge(made.clone()), host), made);
    }

    #[test]
    fn resolv_conf_by_mode() {
        let host = "nameserver 127.0.0.53\noptions edns0 trust-ad\nsearch lan\n";
        let upstream = "nameserver 192.168.5.3\nnameserver 1.1.1.1\n";
        let bridge = NetworkMode::default();
        assert_eq!(
            resolv_conf(&bridge, true, host, Some(upstream)),
            "nameserver 169.254.1.1\noptions edns0 trust-ad\nsearch lan\n"
        );
        assert_eq!(
            resolv_conf(&bridge, false, host, Some(upstream)),
            "nameserver 192.168.5.3\nnameserver 1.1.1.1\noptions edns0 trust-ad\nsearch lan\n"
        );
        assert_eq!(
            resolv_conf(&bridge, false, "nameserver 127.0.0.53\n", None),
            "nameserver 8.8.8.8\nnameserver 8.8.4.4\n"
        );
        assert_eq!(resolv_conf(&NetworkMode::Host, true, host, None), host);
        assert_eq!(
            resolv_conf(&NetworkMode::None, false, "nameserver 10.0.0.1\n", None),
            "nameserver 10.0.0.1\n"
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
