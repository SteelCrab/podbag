//! The `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` a container gets (design
//! 6.4.3 step 4). The shim writes them into the container directory; the container
//! mounts them over the image's files.

use crate::network::NetworkMode;
use std::net::Ipv4Addr;

/// The container's own line in `/etc/hosts`.
#[derive(Debug, Clone, Copy)]
pub enum Own<'a> {
    /// A bridge container: its name, hostname and address.
    Address(&'a str, &'a str, Ipv4Addr),
    /// The host's network: the host's file, plus the hostname.
    Host(&'a str),
    /// No network: loopback only, plus the hostname.
    Loopback(&'a str),
}

const LOCALHOST: &str = "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n";

/// `/etc/hosts`. `peers` are the other containers on the same network (by name); the
/// container itself is skipped among them.
pub fn hosts_file(own: Own<'_>, peers: &[(String, Ipv4Addr)], host_hosts: &str) -> String {
    match own {
        Own::Host(hostname) => {
            let mut text = host_hosts.to_string();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("127.0.1.1\t{hostname}\n"));
            text
        }
        Own::Loopback(hostname) => format!("{LOCALHOST}127.0.1.1\t{hostname}\n"),
        Own::Address(name, hostname, ip) => {
            let mut text = LOCALHOST.to_string();
            if name == hostname {
                text.push_str(&format!("{ip}\t{hostname}\n"));
            } else {
                text.push_str(&format!("{ip}\t{hostname} {name}\n"));
            }
            for (peer, peer_ip) in peers.iter().filter(|(p, _)| p != name) {
                text.push_str(&format!("{peer_ip}\t{peer}\n"));
            }
            text
        }
    }
}

/// One `address<TAB>name` line for each container of a user network: the part of
/// `/etc/hosts` that all of them share. Made once for a refresh of all their files.
pub struct PeerBlock {
    text: String,
    /// Where each peer's line ends in `text`, in the order of the peers.
    ends: Vec<usize>,
}

impl PeerBlock {
    pub fn new(peers: &[(String, Ipv4Addr)]) -> PeerBlock {
        use std::fmt::Write;
        let mut text = String::with_capacity(peers.len() * 32);
        let mut ends = Vec::with_capacity(peers.len());
        for (name, ip) in peers {
            let _ = writeln!(text, "{ip}\t{name}");
            ends.push(text.len());
        }
        PeerBlock { text, ends }
    }

    /// `/etc/hosts` of peer number `index` (its place in the list given to `new`): the
    /// localhost lines, its own line with `hostname`, then the other peers. The same text
    /// as [`hosts_file`] gives for it.
    pub fn hosts_of(&self, index: usize, name: &str, hostname: &str, ip: Ipv4Addr) -> String {
        let start = index
            .checked_sub(1)
            .and_then(|before| self.ends.get(before).copied())
            .unwrap_or(0);
        let end = self.ends.get(index).copied().unwrap_or(start);
        let mut text = String::with_capacity(LOCALHOST.len() + 64 + self.text.len());
        text.push_str(LOCALHOST);
        if name == hostname {
            text.push_str(&format!("{ip}\t{hostname}\n"));
        } else {
            text.push_str(&format!("{ip}\t{hostname} {name}\n"));
        }
        text.push_str(self.text.get(..start).unwrap_or_default());
        text.push_str(self.text.get(end..).unwrap_or_default());
        text
    }
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

    #[test]
    fn hosts_for_a_bridge_container_lists_its_peers() {
        let peers = vec![
            ("db".to_string(), Ipv4Addr::new(10, 78, 1, 2)),
            ("web".to_string(), Ipv4Addr::new(10, 78, 1, 3)),
        ];
        let text = hosts_file(
            Own::Address("web", "abc123", Ipv4Addr::new(10, 78, 1, 3)),
            &peers,
            "",
        );
        assert!(text.starts_with(LOCAL), "{text}");
        assert!(text.contains("10.78.1.3\tabc123 web\n"), "{text}");
        assert!(text.contains("10.78.1.2\tdb\n"), "{text}");
        assert_eq!(
            text.matches("10.78.1.3").count(),
            1,
            "own entry once: {text}"
        );
    }

    #[test]
    fn the_block_gives_the_same_file_as_the_peer_list() {
        let peers = vec![
            ("db".to_string(), Ipv4Addr::new(10, 78, 1, 2)),
            ("web".to_string(), Ipv4Addr::new(10, 78, 1, 3)),
            ("web2".to_string(), Ipv4Addr::new(10, 78, 1, 13)),
        ];
        let block = PeerBlock::new(&peers);
        for (index, (name, ip)) in peers.iter().enumerate() {
            // A hostname of its own, and the name as the hostname.
            for hostname in ["abc123", name.as_str()] {
                assert_eq!(
                    block.hosts_of(index, name, hostname, *ip),
                    hosts_file(Own::Address(name, hostname, *ip), &peers, ""),
                    "{name} as {hostname}"
                );
            }
        }
        // A place that is not in the list leaves every peer in (and does not panic).
        let ip = Ipv4Addr::new(10, 78, 1, 9);
        assert_eq!(
            block.hosts_of(7, "new", "new", ip),
            hosts_file(Own::Address("new", "new", ip), &peers, "")
        );
        let none = PeerBlock::new(&[]);
        assert_eq!(
            none.hosts_of(0, "a", "a", ip),
            hosts_file(Own::Address("a", "a", ip), &[], "")
        );
    }

    #[test]
    fn the_own_line_comes_first() {
        // Two containers with one hostname: each resolves it to itself.
        let peers = vec![
            ("a".to_string(), Ipv4Addr::new(10, 78, 1, 2)),
            ("b".to_string(), Ipv4Addr::new(10, 78, 1, 3)),
        ];
        let text = PeerBlock::new(&peers).hosts_of(1, "b", "same", Ipv4Addr::new(10, 78, 1, 3));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.get(2), Some(&"10.78.1.3\tsame b"), "{text}");
        assert_eq!(lines.get(3), Some(&"10.78.1.2\ta"), "{text}");
        assert_eq!(lines.len(), 4, "{text}");
    }

    #[test]
    fn hosts_for_host_and_none() {
        let host = "127.0.0.1 localhost\n192.168.5.15 lima\n";
        let text = hosts_file(Own::Host("box"), &[], host);
        assert!(text.starts_with(host), "{text}");
        assert!(text.ends_with("127.0.1.1\tbox\n"), "{text}");
        let text = hosts_file(Own::Loopback("box"), &[], host);
        assert_eq!(text, format!("{LOCAL}127.0.1.1\tbox\n"));
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
