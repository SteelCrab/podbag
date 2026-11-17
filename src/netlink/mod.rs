//! netlink messages, built here on any OS and sent on Linux: links, addresses and routes
//! (rtnetlink) and the nftables rules openpod needs. No `ip` or `nft` program is used.

pub mod nft;
pub mod route;
#[cfg(target_os = "linux")]
mod socket;
#[cfg(target_os = "linux")]
pub use socket::Socket;

pub const NLM_F_REQUEST: u16 = 0x1;
pub const NLM_F_ACK: u16 = 0x4;
pub const NLM_F_EXCL: u16 = 0x200;
pub const NLM_F_CREATE: u16 = 0x400;
pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
/// Marks a nested attribute (nftables requires it; rtnetlink ignores it).
pub const NLA_F_NESTED: u16 = 0x8000;

/// One netlink message: a header, a fixed family header, then attributes, each 4-byte
/// aligned. Numbers are in the CPU's byte order, as netlink wants.
#[derive(Debug, Clone)]
pub struct Message {
    buf: Vec<u8>,
}

fn pad(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

impl Message {
    pub fn new(ty: u16, flags: u16, header: &[u8]) -> Message {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&0u32.to_ne_bytes()); // length, set by finish
        buf.extend_from_slice(&ty.to_ne_bytes());
        buf.extend_from_slice(&flags.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes()); // sequence, set by finish
        buf.extend_from_slice(&0u32.to_ne_bytes()); // port ID: the kernel fills it
        buf.extend_from_slice(header);
        pad(&mut buf);
        Message { buf }
    }

    pub fn attr(&mut self, ty: u16, payload: &[u8]) -> &mut Message {
        let len = (4 + payload.len()) as u16;
        self.buf.extend_from_slice(&len.to_ne_bytes());
        self.buf.extend_from_slice(&ty.to_ne_bytes());
        self.buf.extend_from_slice(payload);
        pad(&mut self.buf);
        self
    }

    /// A NUL-terminated string attribute.
    pub fn attr_str(&mut self, ty: u16, s: &str) -> &mut Message {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        self.attr(ty, &bytes)
    }

    pub fn attr_u32(&mut self, ty: u16, v: u32) -> &mut Message {
        self.attr(ty, &v.to_ne_bytes())
    }

    /// An attribute that holds the attributes `f` adds.
    pub fn nested(&mut self, ty: u16, f: impl FnOnce(&mut Message)) -> &mut Message {
        let start = self.buf.len();
        self.buf.extend_from_slice(&0u16.to_ne_bytes());
        self.buf.extend_from_slice(&ty.to_ne_bytes());
        f(self);
        let len = (self.buf.len() - start) as u16;
        self.buf[start..start + 2].copy_from_slice(&len.to_ne_bytes());
        self
    }

    /// The bytes to send, with the total length and sequence number filled in.
    pub fn finish(mut self, seq: u32) -> Vec<u8> {
        let len = self.buf.len() as u32;
        self.buf[0..4].copy_from_slice(&len.to_ne_bytes());
        self.buf[8..12].copy_from_slice(&seq.to_ne_bytes());
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_lengths_and_alignment() {
        let mut m = Message::new(16, NLM_F_REQUEST | NLM_F_ACK, &[0u8; 16]);
        m.attr_str(3, "eth0");
        m.nested(18, |m| {
            m.attr_str(1, "veth");
        });
        let bytes = m.finish(7);
        assert_eq!(bytes.len() % 4, 0);
        assert_eq!(
            u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize,
            bytes.len()
        );
        assert_eq!(u16::from_ne_bytes(bytes[4..6].try_into().unwrap()), 16);
        assert_eq!(u16::from_ne_bytes(bytes[6..8].try_into().unwrap()), 5);
        assert_eq!(u32::from_ne_bytes(bytes[8..12].try_into().unwrap()), 7);
        // IFLA_IFNAME "eth0\0": 4 + 5 = 9, padded to 12.
        assert_eq!(&bytes[32..34], &9u16.to_ne_bytes());
        assert_eq!(&bytes[34..36], &3u16.to_ne_bytes());
        assert_eq!(&bytes[36..41], b"eth0\0");
        assert_eq!(&bytes[41..44], &[0, 0, 0]);
        // The nest holds one attribute of 4 + 5 = 9 (padded 12): 4 + 12 = 16.
        assert_eq!(&bytes[44..46], &16u16.to_ne_bytes());
        assert_eq!(&bytes[46..48], &18u16.to_ne_bytes());
        assert_eq!(&bytes[48..50], &9u16.to_ne_bytes());
        assert_eq!(&bytes[52..57], b"veth\0");
        assert_eq!(bytes.len(), 60);
    }
}

/// Run by hand as root in a throwaway network namespace (`dev/netlink-check.sh`): the
/// kernel accepts the messages, and `ip`/`nft` read back what was made.
#[cfg(all(test, target_os = "linux"))]
mod kernel_tests {
    use super::*;

    fn run(cmd: &str) -> String {
        let out = std::process::Command::new("sh")
            .args(["-c", cmd])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    #[ignore = "needs root in a new network namespace"]
    fn kernel_accepts_links_addresses_and_routes() {
        let mut s = Socket::route().unwrap();
        s.request(route::new_bridge("opdtest0")).unwrap();
        let bridge = s.link_index("opdtest0").unwrap().expect("bridge index");
        s.request(route::add_address(bridge, [10, 99, 0, 1], 16))
            .unwrap();
        s.request(route::set_up(bridge)).unwrap();
        let ns = std::fs::File::open("/proc/self/ns/net").unwrap();
        use std::os::fd::AsRawFd;
        s.request(route::new_veth("opdv0", "opdv1", ns.as_raw_fd()))
            .unwrap();
        let veth = s.link_index("opdv0").unwrap().unwrap();
        s.request(route::set_master(veth, bridge)).unwrap();
        s.request(route::set_up(veth)).unwrap();
        let peer = s.link_index("opdv1").unwrap().unwrap();
        s.request(route::add_address(peer, [10, 99, 0, 2], 16))
            .unwrap();
        s.request(route::set_up(peer)).unwrap();
        let lo = s.link_index("lo").unwrap().unwrap();
        s.request(route::set_up(lo)).unwrap();
        assert_eq!(s.link_index("nope0").unwrap(), None);
        let links = run("ip -o link show");
        assert!(
            links.contains("opdv0@opdv1") && links.contains("master opdtest0"),
            "{links}"
        );
        assert!(run("ip -o -4 addr show opdtest0").contains("10.99.0.1/16"));
    }

    #[test]
    #[ignore = "needs root in a new network namespace"]
    fn kernel_accepts_the_nat_chain() {
        let mut s = Socket::open(libc::NETLINK_NETFILTER).unwrap();
        let (bytes, last) = nft::masquerade_batch(s.next_seq(), [10, 77, 1, 0], 24, "opd-1");
        s.request_raw(&bytes, last).unwrap();
        // Repeating keeps one rule.
        let (bytes, last) = nft::masquerade_batch(s.next_seq() + 10, [10, 77, 1, 0], 24, "opd-1");
        s.request_raw(&bytes, last).unwrap();
        let rules = run("nft list table ip openpod");
        assert!(
            rules.contains("ip saddr 10.77.1.0/24 oifname != \"opd-1\" masquerade"),
            "{rules}"
        );
        assert_eq!(rules.matches("masquerade").count(), 1, "{rules}");
        assert!(
            rules.contains("type nat hook postrouting priority srcnat"),
            "{rules}"
        );
        assert!(
            rules.contains("type filter hook forward priority filter"),
            "{rules}"
        );
        let inbound = "oifname \"opd-1\" iifname != \"opd-1\" ct state";
        assert!(rules.contains(inbound) && rules.contains("drop"), "{rules}");
        assert!(
            rules.contains("iifname \"opd-1\" ip daddr 169.254.1.3 drop"),
            "{rules}"
        );
        let (bytes, last) = nft::remove_masquerade_batch(s.next_seq() + 20, "opd-1");
        s.request_raw(&bytes, last).unwrap();
        let left = run("nft list table ip openpod");
        assert!(
            !left.contains("post-opd-1") && !left.contains("fwd-opd-1"),
            "{left}"
        );
    }
}
