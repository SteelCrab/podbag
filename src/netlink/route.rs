//! rtnetlink: bridges, veth pairs, addresses, link state and routes.

use super::{Message, NLM_F_ACK, NLM_F_CREATE, NLM_F_EXCL, NLM_F_REQUEST};

pub const RTM_NEWLINK: u16 = 16;
pub const RTM_DELLINK: u16 = 17;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_NEWROUTE: u16 = 24;
pub const IFLA_IFNAME: u16 = 3;
pub const IFLA_MASTER: u16 = 10;
pub const IFLA_LINKINFO: u16 = 18;
pub const IFLA_NET_NS_FD: u16 = 28;
pub const IFLA_INFO_KIND: u16 = 1;
pub const IFLA_INFO_DATA: u16 = 2;
pub const VETH_INFO_PEER: u16 = 1;
pub const IFA_ADDRESS: u16 = 1;
pub const IFA_LOCAL: u16 = 2;
pub const RTA_GATEWAY: u16 = 5;
pub const IFF_UP: u32 = 1;
const AF_INET: u8 = 2;
const CREATE: u16 = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;

/// `struct ifinfomsg` (16 bytes) for link `index` (0: none), with `flags` under `change`.
fn ifinfomsg(index: u32, flags: u32, change: u32) -> [u8; 16] {
    let mut h = [0u8; 16];
    h[4..8].copy_from_slice(&(index as i32).to_ne_bytes());
    h[8..12].copy_from_slice(&flags.to_ne_bytes());
    h[12..16].copy_from_slice(&change.to_ne_bytes());
    h
}

/// Create a bridge.
pub fn new_bridge(name: &str) -> Message {
    let mut m = Message::new(RTM_NEWLINK, CREATE, &ifinfomsg(0, 0, 0));
    m.attr_str(IFLA_IFNAME, name);
    m.nested(IFLA_LINKINFO, |m| {
        m.attr_str(IFLA_INFO_KIND, "bridge");
    });
    m
}

/// Create a veth pair: `name` here, `peer` in the network namespace `peer_netns_fd`.
pub fn new_veth(name: &str, peer: &str, peer_netns_fd: i32) -> Message {
    let mut m = Message::new(RTM_NEWLINK, CREATE, &ifinfomsg(0, 0, 0));
    m.attr_str(IFLA_IFNAME, name);
    m.nested(IFLA_LINKINFO, |m| {
        m.attr_str(IFLA_INFO_KIND, "veth");
        m.nested(IFLA_INFO_DATA, |m| {
            m.nested(VETH_INFO_PEER, |m| {
                m.buf.extend_from_slice(&ifinfomsg(0, 0, 0));
                m.attr_str(IFLA_IFNAME, peer);
                m.attr_u32(IFLA_NET_NS_FD, peer_netns_fd as u32);
            });
        });
    });
    m
}

/// Delete a link.
pub fn del_link(index: u32) -> Message {
    Message::new(
        RTM_DELLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        &ifinfomsg(index, 0, 0),
    )
}

/// Bring a link up.
pub fn set_up(index: u32) -> Message {
    Message::new(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        &ifinfomsg(index, IFF_UP, IFF_UP),
    )
}

/// Attach a link to a bridge.
pub fn set_master(index: u32, master: u32) -> Message {
    let mut m = Message::new(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        &ifinfomsg(index, 0, 0),
    );
    m.attr_u32(IFLA_MASTER, master);
    m
}

/// Give link `index` the IPv4 address `addr/prefix`.
pub fn add_address(index: u32, addr: [u8; 4], prefix: u8) -> Message {
    let mut h = [0u8; 8];
    h[0] = AF_INET;
    h[1] = prefix;
    h[4..8].copy_from_slice(&index.to_ne_bytes());
    let mut m = Message::new(RTM_NEWADDR, CREATE, &h);
    m.attr(IFA_LOCAL, &addr);
    m.attr(IFA_ADDRESS, &addr);
    m
}

/// The default IPv4 route through `gateway`.
pub fn add_default_route(gateway: [u8; 4]) -> Message {
    // family, dst_len, src_len, tos, table main, protocol boot, scope universe, unicast.
    let mut h = [0u8; 12];
    h[..8].copy_from_slice(&[AF_INET, 0, 0, 0, 254, 3, 0, 1]);
    let mut m = Message::new(RTM_NEWROUTE, CREATE, &h);
    m.attr(RTA_GATEWAY, &gateway);
    m
}

/// Ask for the link called `name` (the answer carries its index).
pub fn get_link(name: &str) -> Message {
    let mut m = Message::new(RTM_GETLINK, NLM_F_REQUEST, &ifinfomsg(0, 0, 0));
    m.attr_str(IFLA_IFNAME, name);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_ne_bytes(b[at..at + 2].try_into().unwrap())
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn new_bridge_message() {
        let b = new_bridge("openpod0").finish(1);
        assert_eq!(u16_at(&b, 4), RTM_NEWLINK);
        assert_eq!(
            u16_at(&b, 6),
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL
        );
        assert_eq!(&b[16..32], &[0u8; 16], "ifinfomsg");
        // IFLA_IFNAME "openpod0\0" = 4 + 9 = 13 → 16 bytes.
        assert_eq!((u16_at(&b, 32), u16_at(&b, 34)), (13, IFLA_IFNAME));
        assert_eq!(&b[36..45], b"openpod0\0");
        // IFLA_LINKINFO { IFLA_INFO_KIND "bridge\0" (4 + 7 = 11 → 12) } = 16.
        assert_eq!((u16_at(&b, 48), u16_at(&b, 50)), (16, IFLA_LINKINFO));
        assert_eq!((u16_at(&b, 52), u16_at(&b, 54)), (11, IFLA_INFO_KIND));
        assert_eq!(&b[56..63], b"bridge\0");
        assert_eq!(b.len(), 64);
    }

    #[test]
    fn new_veth_puts_the_peer_in_a_namespace() {
        let b = new_veth("opd3a", "eth0", 9).finish(1);
        // IFLA_IFNAME "opd3a\0" = 10 → 12, at 32..44.
        assert_eq!(&b[36..42], b"opd3a\0");
        // IFLA_LINKINFO at 44: KIND "veth\0" (9 → 12), then INFO_DATA { VETH_INFO_PEER {
        // ifinfomsg(16), IFLA_IFNAME "eth0\0" (9 → 12), IFLA_NET_NS_FD u32 (8) } }.
        assert_eq!(u16_at(&b, 46), IFLA_LINKINFO);
        assert_eq!(u16_at(&b, 50), IFLA_INFO_KIND);
        assert_eq!(&b[52..57], b"veth\0");
        assert_eq!(u16_at(&b, 62), IFLA_INFO_DATA);
        assert_eq!(u16_at(&b, 66), VETH_INFO_PEER);
        assert_eq!(&b[68..84], &[0u8; 16], "peer ifinfomsg");
        assert_eq!((u16_at(&b, 84), u16_at(&b, 86)), (9, IFLA_IFNAME));
        assert_eq!(&b[88..93], b"eth0\0");
        assert_eq!((u16_at(&b, 96), u16_at(&b, 98)), (8, IFLA_NET_NS_FD));
        assert_eq!(u32_at(&b, 100), 9);
        assert_eq!(u16_at(&b, 64) as usize, b.len() - 64, "peer nest length");
        assert_eq!(
            u16_at(&b, 44) as usize,
            b.len() - 44,
            "linkinfo nest length"
        );
    }

    #[test]
    fn link_up_and_master() {
        let b = set_up(5).finish(1);
        assert_eq!(u16_at(&b, 6), NLM_F_REQUEST | NLM_F_ACK);
        assert_eq!(u32_at(&b, 20), 5, "ifi_index");
        assert_eq!((u32_at(&b, 24), u32_at(&b, 28)), (IFF_UP, IFF_UP));
        let m = set_master(5, 2).finish(1);
        assert_eq!(u32_at(&m, 20), 5);
        assert_eq!(
            (u16_at(&m, 32), u16_at(&m, 34), u32_at(&m, 36)),
            (8, IFLA_MASTER, 2)
        );
    }

    #[test]
    fn address_and_default_route() {
        let a = add_address(3, [10, 77, 0, 1], 16).finish(1);
        assert_eq!(u16_at(&a, 4), RTM_NEWADDR);
        assert_eq!(&a[16..20], &[2, 16, 0, 0], "family, prefix, flags, scope");
        assert_eq!(u32_at(&a, 20), 3);
        assert_eq!((u16_at(&a, 24), u16_at(&a, 26)), (8, IFA_LOCAL));
        assert_eq!(&a[28..32], &[10, 77, 0, 1]);
        assert_eq!((u16_at(&a, 32), u16_at(&a, 34)), (8, IFA_ADDRESS));
        assert_eq!(&a[36..40], &[10, 77, 0, 1]);
        let r = add_default_route([10, 77, 0, 1]).finish(1);
        assert_eq!(u16_at(&r, 4), RTM_NEWROUTE);
        assert_eq!(&r[16..24], &[2, 0, 0, 0, 254, 3, 0, 1], "rtmsg");
        assert_eq!((u16_at(&r, 28), u16_at(&r, 30)), (8, RTA_GATEWAY));
        assert_eq!(&r[32..36], &[10, 77, 0, 1]);
    }

    #[test]
    fn get_link_by_name() {
        let b = get_link("lo").finish(1);
        assert_eq!(u16_at(&b, 4), RTM_GETLINK);
        assert_eq!(u16_at(&b, 6), NLM_F_REQUEST);
        assert_eq!(&b[36..39], b"lo\0");
    }
}
