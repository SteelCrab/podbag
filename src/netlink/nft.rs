//! nftables over netlink: podbag's own table `podbag` (family ip) with one NAT chain per
//! network, `post-<network>`, holding `ip saddr <subnet> oifname != <bridge> masquerade`.
//! A chain per network makes removal simple: flush the chain, then delete it.

use super::{Message, NLA_F_NESTED, NLM_F_ACK, NLM_F_CREATE, NLM_F_REQUEST};

const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
const NFNL_MSG_BATCH_END: u16 = 0x11;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_DELCHAIN: u16 = 5;
const NFT_MSG_NEWRULE: u16 = 6;
const NFT_MSG_GETRULE: u16 = 7;
/// `NLM_F_ROOT | NLM_F_MATCH`: a listing.
const NLM_F_DUMP: u16 = 0x300;
/// NFTA_RULE_USERDATA.
const RULE_USERDATA: u16 = 7;

/// The mark on every rule that podbag writes (a rule comment: `nft list` shows it): this
/// text and the network's subnet. With the bridge, which is in the chains' names, that is
/// all the rules are made from. A start leaves a network's rules alone when all of them
/// are there with this mark, so **change the number whenever the rules in
/// `masquerade_batch` change**: hosts that have the rules of an earlier version then get
/// the new ones at the next start.
const RULES_VERSION: &str = "podbag rules 1";
/// How many rules `masquerade_batch` puts into the NAT chain and into the forward chain.
const NAT_RULES: usize = 1;
const FORWARD_RULES: usize = 2;

/// The network address of `subnet` (its host bits cleared) and its mask.
fn network_of(subnet: [u8; 4], prefix: u8) -> ([u8; 4], [u8; 4]) {
    let mask = (u32::MAX
        .checked_shl(32 - u32::from(prefix.min(32)))
        .unwrap_or(0))
    .to_be_bytes();
    let mut network = subnet;
    for (byte, m) in network.iter_mut().zip(mask) {
        *byte &= m;
    }
    (network, mask)
}

/// The mark as rule user data: one comment entry (type 0, length, text, NUL).
fn rules_mark(subnet: [u8; 4], prefix: u8) -> Vec<u8> {
    let ([a, b, c, d], _) = network_of(subnet, prefix);
    let text = format!("{RULES_VERSION} {a}.{b}.{c}.{d}/{}", prefix.min(32));
    let mut mark = vec![0, (text.len() + 1) as u8];
    mark.extend_from_slice(text.as_bytes());
    mark.push(0);
    mark
}
const NFT_MSG_DELRULE: u16 = 8;
const NLM_F_APPEND: u16 = 0x800;
const NFPROTO_IPV4: u8 = 2;
const NF_INET_FORWARD: u32 = 2;
const NF_INET_POST_ROUTING: u32 = 4;
/// `filter`.
const FILTER_PRIORITY: u32 = 0;
const NFT_META_IIFNAME: u32 = 6;
const NFT_CT_STATE: u32 = 0;
/// `ct state established,related` bits (NF_CT_STATE_BIT of each).
const CT_ESTABLISHED_RELATED: u32 = 2 | 4;
const NF_DROP: u32 = 0;
/// The verdict register.
const NFT_REG_VERDICT: u32 = 0;

/// The address pasta maps to the host's loopback (`--map-host-loopback`). Containers must
/// not reach it; the forward chain drops it.
pub const HOST_LOOPBACK: [u8; 4] = [169, 254, 1, 3];
/// `srcnat`.
const NAT_PRIORITY: u32 = 100;
const NFT_REG_1: u32 = 1;
const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
const NFT_META_OIFNAME: u32 = 7;
const NFT_CMP_EQ: u32 = 0;
const NFT_CMP_NEQ: u32 = 1;
const IFNAMSIZ: usize = 16;

/// The table podbag owns; no other table is touched.
pub const TABLE: &str = "podbag";

/// `struct nfgenmsg`: family, version 0, resource ID (big-endian).
fn nfgenmsg(family: u8, res_id: u16) -> [u8; 4] {
    let id = res_id.to_be_bytes();
    [family, 0, id[0], id[1]]
}

fn message(ty: u16, flags: u16) -> Message {
    Message::new(
        (NFNL_SUBSYS_NFTABLES << 8) | ty,
        NLM_F_REQUEST | NLM_F_ACK | flags,
        &nfgenmsg(NFPROTO_IPV4, 0),
    )
}

/// Wrap messages in BATCH_BEGIN/END. Returns the bytes and the sequence number of the last
/// message that asks for an acknowledgement.
fn batch(first_seq: u32, msgs: Vec<Message>) -> (Vec<u8>, u32) {
    let control = |ty| Message::new(ty, NLM_F_REQUEST, &nfgenmsg(0, NFNL_SUBSYS_NFTABLES));
    let mut seq = first_seq;
    let mut bytes = control(NFNL_MSG_BATCH_BEGIN).finish(seq);
    let mut last = seq;
    for m in msgs {
        seq += 1;
        last = seq;
        bytes.extend(m.finish(seq));
    }
    seq += 1;
    bytes.extend(control(NFNL_MSG_BATCH_END).finish(seq));
    (bytes, last)
}

/// Chains are named after the bridge, which is unique on the host (and in a rootless
/// session) even when several data directories have a network of the same name.
fn chain_name(bridge: &str) -> String {
    format!("post-{bridge}")
}

fn forward_chain_name(bridge: &str) -> String {
    format!("fwd-{bridge}")
}

fn base_chain(name: &str, hook: u32, priority: u32, kind: &str) -> Message {
    let mut chain = message(NFT_MSG_NEWCHAIN, NLM_F_CREATE);
    chain.attr_str(1, TABLE);
    chain.attr_str(3, name);
    chain.nested(4 | NLA_F_NESTED, |m| {
        be(m, 1, hook);
        be(m, 2, priority);
    });
    chain.attr_str(7, kind);
    chain
}

fn flush(chain: &str) -> Message {
    let mut m = message(NFT_MSG_DELRULE, 0);
    m.attr_str(1, TABLE);
    m.attr_str(2, chain);
    m
}

fn rule(chain: &str, mark: &[u8], exprs: impl FnOnce(&mut Message)) -> Message {
    let mut m = message(NFT_MSG_NEWRULE, NLM_F_CREATE | NLM_F_APPEND);
    m.attr_str(1, TABLE);
    m.attr_str(2, chain);
    m.nested(4 | NLA_F_NESTED, exprs);
    m.attr(RULE_USERDATA, mark);
    m
}

fn ifname_bytes(name: &str) -> [u8; IFNAMSIZ] {
    let mut ifname = [0u8; IFNAMSIZ];
    let n = name.len().min(IFNAMSIZ - 1);
    ifname[..n].copy_from_slice(&name.as_bytes()[..n]);
    ifname
}

/// `meta <key>` then `cmp <op> <ifname>`.
fn match_ifname(m: &mut Message, key: u32, op: u32, name: &str) {
    let ifname = ifname_bytes(name);
    expr(
        m,
        "meta",
        Some(&|m: &mut Message| {
            be(m, 1, NFT_REG_1);
            be(m, 2, key);
        }),
    );
    expr(
        m,
        "cmp",
        Some(&|m: &mut Message| {
            be(m, 1, NFT_REG_1);
            be(m, 2, op);
            value(m, 3, &ifname);
        }),
    );
}

fn drop_verdict(m: &mut Message) {
    expr(
        m,
        "immediate",
        Some(&|m: &mut Message| {
            be(m, 1, NFT_REG_VERDICT);
            m.nested(2 | NLA_F_NESTED, |m| {
                m.nested(2 | NLA_F_NESTED, |m| {
                    be(m, 1, NF_DROP);
                });
            });
        }),
    );
}

/// One expression: `name` with the attributes `data` adds (none for `masq`).
fn expr(m: &mut Message, name: &str, data: Option<&dyn Fn(&mut Message)>) {
    m.nested(1 | NLA_F_NESTED, |m| {
        m.attr_str(1, name);
        if let Some(data) = data {
            m.nested(2 | NLA_F_NESTED, |m| data(m));
        }
    });
}

fn be(m: &mut Message, ty: u16, v: u32) {
    m.attr(ty, &v.to_be_bytes());
}

/// A data attribute: `ty` { NFTA_DATA_VALUE bytes }.
fn value(m: &mut Message, ty: u16, bytes: &[u8]) {
    m.nested(ty | NLA_F_NESTED, |m| {
        m.attr(1, bytes);
    });
}

/// Create (if needed) the table and the network's chain, and make its one rule:
/// `ip saddr <subnet>/<prefix> oifname != <bridge> masquerade`. Safe to repeat.
pub fn masquerade_batch(
    first_seq: u32,
    subnet: [u8; 4],
    prefix: u8,
    bridge: &str,
) -> (Vec<u8>, u32) {
    let chain = chain_name(bridge);
    let mut table = message(NFT_MSG_NEWTABLE, NLM_F_CREATE);
    table.attr_str(1, TABLE);

    let (network_addr, mask) = network_of(subnet, prefix);
    let mark = rules_mark(subnet, prefix);
    let masquerade = rule(&chain, &mark, |m| {
        expr(
            m,
            "payload",
            Some(&|m: &mut Message| {
                be(m, 1, NFT_REG_1);
                be(m, 2, NFT_PAYLOAD_NETWORK_HEADER);
                be(m, 3, 12);
                be(m, 4, 4);
            }),
        );
        expr(
            m,
            "bitwise",
            Some(&|m: &mut Message| {
                be(m, 1, NFT_REG_1);
                be(m, 2, NFT_REG_1);
                be(m, 3, 4);
                value(m, 4, &mask);
                value(m, 5, &[0; 4]);
            }),
        );
        expr(
            m,
            "cmp",
            Some(&|m: &mut Message| {
                be(m, 1, NFT_REG_1);
                be(m, 2, NFT_CMP_EQ);
                value(m, 3, &network_addr);
            }),
        );
        match_ifname(m, NFT_META_OIFNAME, NFT_CMP_NEQ, bridge);
        expr(m, "masq", None);
    });

    // Isolation: no new connection reaches the bridge from anywhere but itself (other
    // networks, other hosts routing through this one); answers to its own may.
    let forward = forward_chain_name(bridge);
    let inbound = rule(&forward, &mark, |m| {
        match_ifname(m, NFT_META_OIFNAME, NFT_CMP_EQ, bridge);
        match_ifname(m, NFT_META_IIFNAME, NFT_CMP_NEQ, bridge);
        expr(
            m,
            "ct",
            Some(&|m: &mut Message| {
                be(m, 1, NFT_REG_1);
                be(m, 2, NFT_CT_STATE);
            }),
        );
        expr(
            m,
            "bitwise",
            Some(&|m: &mut Message| {
                be(m, 1, NFT_REG_1);
                be(m, 2, NFT_REG_1);
                be(m, 3, 4);
                value(m, 4, &CT_ESTABLISHED_RELATED.to_ne_bytes());
                value(m, 5, &[0; 4]);
            }),
        );
        expr(
            m,
            "cmp",
            Some(&|m: &mut Message| {
                be(m, 1, NFT_REG_1);
                be(m, 2, NFT_CMP_EQ);
                value(m, 3, &[0; 4]);
            }),
        );
        drop_verdict(m);
    });
    let host_loopback = rule(&forward, &mark, |m| {
        match_ifname(m, NFT_META_IIFNAME, NFT_CMP_EQ, bridge);
        expr(
            m,
            "payload",
            Some(&|m: &mut Message| {
                be(m, 1, NFT_REG_1);
                be(m, 2, NFT_PAYLOAD_NETWORK_HEADER);
                be(m, 3, 16);
                be(m, 4, 4);
            }),
        );
        expr(
            m,
            "cmp",
            Some(&|m: &mut Message| {
                be(m, 1, NFT_REG_1);
                be(m, 2, NFT_CMP_EQ);
                value(m, 3, &HOST_LOOPBACK);
            }),
        );
        drop_verdict(m);
    });
    batch(
        first_seq,
        vec![
            table,
            base_chain(&chain, NF_INET_POST_ROUTING, NAT_PRIORITY, "nat"),
            flush(&chain),
            masquerade,
            base_chain(&forward, NF_INET_FORWARD, FILTER_PRIORITY, "filter"),
            flush(&forward),
            inbound,
            host_loopback,
        ],
    )
}

/// Listings that the kernel answers without a transaction: the rules of each of the
/// network's chains, with the number of rules that this version puts there. Send each
/// with `Socket::dump` and give the answer to [`rules_in_place`].
pub fn rule_queries(bridge: &str) -> [(Message, usize); 2] {
    [
        (chain_name(bridge), NAT_RULES),
        (forward_chain_name(bridge), FORWARD_RULES),
    ]
    .map(|(chain, expected)| {
        let mut query = Message::new(
            (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETRULE,
            NLM_F_REQUEST | NLM_F_DUMP,
            &nfgenmsg(NFPROTO_IPV4, 0),
        );
        query.attr_str(1, TABLE);
        query.attr_str(2, &chain);
        (query, expected)
    })
}

/// Whether a chain's listing (`rules`: one entry for each rule) holds exactly this
/// version's rules for this subnet: their number, each with the mark. A chain that is not
/// there, was emptied, or has rules of another version, of another subnet or of somebody
/// else is not. (A bridge is named after its subnet, so another subnet on the same bridge
/// means that two names fell together; the rules then follow the network that starts a
/// container.)
pub fn rules_in_place(rules: &[Vec<u8>], expected: usize, subnet: [u8; 4], prefix: u8) -> bool {
    let mark = rules_mark(subnet, prefix);
    rules.len() == expected
        && rules
            .iter()
            .all(|rule| user_data(rule) == Some(mark.as_slice()))
}

/// The user data of one rule in a listing: after the 4-byte header, attributes of
/// (length, type, payload), each padded to 4 bytes.
fn user_data(rule: &[u8]) -> Option<&[u8]> {
    let mut rest = rule.get(4..)?;
    loop {
        let len = usize::from(u16::from_ne_bytes([*rest.first()?, *rest.get(1)?]));
        let ty = u16::from_ne_bytes([*rest.get(2)?, *rest.get(3)?]) & 0x3fff;
        if len < 4 {
            return None;
        }
        if ty == RULE_USERDATA {
            return rest.get(4..len);
        }
        rest = rest.get((len + 3) & !3..)?;
    }
}

/// Remove the network's chains: flush each, then delete it.
pub fn remove_masquerade_batch(first_seq: u32, bridge: &str) -> (Vec<u8>, u32) {
    let mut msgs = Vec::new();
    for chain in [chain_name(bridge), forward_chain_name(bridge)] {
        msgs.push(flush(&chain));
        let mut delete = message(NFT_MSG_DELCHAIN, 0);
        delete.attr_str(1, TABLE);
        delete.attr_str(3, &chain);
        msgs.push(delete);
    }
    batch(first_seq, msgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules of a batch for `chain`, as a listing would give them: each without its
    /// 16-byte message header.
    fn listed_rules(batch: &[u8], chain: &str) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 16 <= batch.len() {
            let len = u32::from_ne_bytes(batch[at..at + 4].try_into().unwrap()) as usize;
            let ty = u16::from_ne_bytes([batch[at + 4], batch[at + 5]]);
            let body = &batch[at + 16..at + len];
            let mut name = chain.as_bytes().to_vec();
            name.push(0);
            if ty == (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWRULE
                && body.windows(name.len()).any(|w| w == name.as_slice())
            {
                out.push(body.to_vec());
            }
            at += (len + 3) & !3;
        }
        out
    }

    #[test]
    fn rules_are_in_place_only_with_this_versions_mark() {
        let (batch, _) = masquerade_batch(1, [10, 77, 1, 0], 24, "pbg-1");
        let nat = listed_rules(&batch, "post-pbg-1");
        let forward = listed_rules(&batch, "fwd-pbg-1");
        assert_eq!((nat.len(), forward.len()), (NAT_RULES, FORWARD_RULES));
        let in_place =
            |rules: &[Vec<u8>], expected| rules_in_place(rules, expected, [10, 77, 1, 0], 24);
        assert!(in_place(&nat, NAT_RULES));
        assert!(in_place(&forward, FORWARD_RULES));
        // The subnet as it is written does not matter, only the network it names.
        assert!(rules_in_place(&nat, NAT_RULES, [10, 77, 1, 9], 24));
        // A chain that is not there or was emptied, and one with a rule too few or too many.
        assert!(!in_place(&[], NAT_RULES));
        assert!(!in_place(&forward[..1], FORWARD_RULES));
        assert!(!in_place(&forward, NAT_RULES));
        // The rules of another subnet on the same bridge, in both chains.
        assert!(!rules_in_place(&nat, NAT_RULES, [10, 77, 2, 0], 24));
        assert!(!rules_in_place(&nat, NAT_RULES, [10, 77, 0, 0], 16));
        assert!(!rules_in_place(&forward, FORWARD_RULES, [10, 77, 2, 0], 24));
        // A rule of another version: the same rule with another number in its mark, or
        // with no mark.
        let mut other = nat.clone();
        let text = RULES_VERSION.as_bytes();
        let at = other[0]
            .windows(text.len())
            .position(|w| w == text)
            .unwrap();
        let number = at + text.len() - 1;
        assert_eq!(other[0][number], b'1');
        other[0][number] = b'2';
        assert!(!in_place(&other, NAT_RULES));
        let mark_attr = (4 + rules_mark([10, 77, 1, 0], 24).len() + 3) & !3;
        let unmarked = vec![nat[0][..nat[0].len() - mark_attr].to_vec()];
        assert_eq!(user_data(&unmarked[0]), None);
        assert!(!in_place(&unmarked, NAT_RULES));
        // Answers that are no rules at all.
        assert!(!in_place(&[vec![1, 2, 3]], 1));
        assert!(!in_place(&[vec![0; 4]], 1));
        assert!(!in_place(&[vec![0, 0, 0, 0, 3, 0, 7, 0]], 1));
        assert_eq!(user_data(&[]), None);
        // The mark names the network, whatever the prefix.
        assert_eq!(
            &rules_mark([10, 77, 1, 9], 24)[2..],
            b"podbag rules 1 10.77.1.0/24\0"
        );
        assert_eq!(
            &rules_mark([10, 77, 1, 9], 0)[2..],
            b"podbag rules 1 0.0.0.0/0\0"
        );
        assert_eq!(
            &rules_mark([10, 77, 1, 9], 32)[2..],
            b"podbag rules 1 10.77.1.9/32\0"
        );
    }

    #[test]
    fn the_listing_asks_for_each_chain_of_the_network() {
        let [(nat, n), (forward, f)] = rule_queries("pbg-1");
        assert_eq!((n, f), (NAT_RULES, FORWARD_RULES));
        for (query, chain) in [(nat, "post-pbg-1"), (forward, "fwd-pbg-1")] {
            let bytes = query.finish(7);
            let ty = u16::from_ne_bytes([bytes[4], bytes[5]]);
            let flags = u16::from_ne_bytes([bytes[6], bytes[7]]);
            assert_eq!(ty, (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETRULE);
            assert_eq!(flags, NLM_F_REQUEST | NLM_F_DUMP);
            let a = attrs(&bytes[20..]);
            assert_eq!(get(&a, 1), b"podbag\0");
            let mut name = chain.as_bytes().to_vec();
            name.push(0);
            assert_eq!(get(&a, 2), name);
        }
    }

    /// (type, payload) of each attribute in `b`.
    fn attrs(b: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 4 <= b.len() {
            let len = u16::from_ne_bytes([b[at], b[at + 1]]) as usize;
            let ty = u16::from_ne_bytes([b[at + 2], b[at + 3]]);
            out.push((ty, b[at + 4..at + len].to_vec()));
            at += (len + 3) & !3;
        }
        out
    }

    fn get(a: &[(u16, Vec<u8>)], ty: u16) -> Vec<u8> {
        a.iter()
            .find(|(t, _)| *t == ty)
            .unwrap_or_else(|| panic!("no attribute {ty:#x}"))
            .1
            .clone()
    }

    /// The messages in a batch: (type, attributes after the 4-byte nfgenmsg).
    fn messages(bytes: &[u8]) -> Vec<(u16, [u8; 4], Vec<(u16, Vec<u8>)>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let len = u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let ty = u16::from_ne_bytes([bytes[at + 4], bytes[at + 5]]);
            let genmsg: [u8; 4] = bytes[at + 16..at + 20].try_into().unwrap();
            out.push((ty, genmsg, attrs(&bytes[at + 20..at + len])));
            at += len;
        }
        out
    }

    const SUBSYS: u16 = 10 << 8;

    #[test]
    fn masquerade_batch_creates_table_chain_and_rule() {
        let (bytes, last) = masquerade_batch(100, [10, 77, 1, 0], 24, "pbg-1");
        let m = messages(&bytes);
        let types: Vec<u16> = m.iter().map(|x| x.0).collect();
        assert_eq!(
            types,
            [
                0x10,
                SUBSYS,
                SUBSYS | 3,
                SUBSYS | 8,
                SUBSYS | 6,
                SUBSYS | 3,
                SUBSYS | 8,
                SUBSYS | 6,
                SUBSYS | 6,
                0x11
            ],
            "begin, table, nat chain, flush, masquerade, forward chain, flush, 2 rules, end"
        );
        assert_eq!(m[0].1, [0, 0, 0, 10], "batch for the nftables subsystem");
        assert_eq!(m[1].1, [2, 0, 0, 0], "family ip");
        assert_eq!(last, 108, "the last rule's sequence number");
        assert_eq!(get(&m[1].2, 1), b"podbag\0");
        let chain = &m[2].2;
        assert_eq!(get(chain, 1), b"podbag\0");
        assert_eq!(get(chain, 3), b"post-pbg-1\0");
        assert_eq!(get(chain, 7), b"nat\0");
        let hook = attrs(&get(chain, 4 | NLA_F_NESTED));
        assert_eq!(get(&hook, 1), 4u32.to_be_bytes(), "postrouting");
        assert_eq!(get(&hook, 2), 100u32.to_be_bytes(), "srcnat priority");
        assert_eq!(get(&m[3].2, 2), b"post-pbg-1\0", "flush that chain");

        let rule = &m[4].2;
        let exprs = attrs(&get(rule, 4 | NLA_F_NESTED));
        let names: Vec<Vec<u8>> = exprs.iter().map(|(_, e)| get(&attrs(e), 1)).collect();
        let want: Vec<&[u8]> = vec![
            b"payload\0",
            b"bitwise\0",
            b"cmp\0",
            b"meta\0",
            b"cmp\0",
            b"masq\0",
        ];
        assert_eq!(names, want);
        let data = |i: usize| attrs(&get(&attrs(&exprs[i].1), 2 | NLA_F_NESTED));
        let value = |a: &[(u16, Vec<u8>)], ty: u16| get(&attrs(&get(a, ty | NLA_F_NESTED)), 1);
        let payload = data(0);
        assert_eq!(get(&payload, 2), 1u32.to_be_bytes(), "network header");
        assert_eq!(get(&payload, 3), 12u32.to_be_bytes(), "saddr offset");
        assert_eq!(value(&data(1), 4), [255, 255, 255, 0], "mask");
        assert_eq!(value(&data(2), 3), [10, 77, 1, 0], "network");
        assert_eq!(get(&data(2), 2), 0u32.to_be_bytes(), "eq");
        assert_eq!(get(&data(3), 2), 7u32.to_be_bytes(), "oifname");
        let mut ifname = [0u8; 16];
        ifname[..5].copy_from_slice(b"pbg-1");
        assert_eq!(value(&data(4), 3), ifname);
        assert_eq!(get(&data(4), 2), 1u32.to_be_bytes(), "neq");
    }

    #[test]
    fn forward_chain_isolates_the_network() {
        let (bytes, _) = masquerade_batch(1, [10, 78, 1, 0], 24, "pbg-1");
        let m = messages(&bytes);
        let chain = &m[5].2;
        assert_eq!(get(chain, 3), b"fwd-pbg-1\0");
        assert_eq!(get(chain, 7), b"filter\0");
        let hook = attrs(&get(chain, 4 | NLA_F_NESTED));
        assert_eq!(get(&hook, 1), 2u32.to_be_bytes(), "forward");
        assert_eq!(get(&hook, 2), 0u32.to_be_bytes(), "filter priority");
        let names = |i: usize| -> Vec<Vec<u8>> {
            attrs(&get(&m[i].2, 4 | NLA_F_NESTED))
                .iter()
                .map(|(_, e)| get(&attrs(e), 1))
                .collect()
        };
        let inbound: Vec<&[u8]> = vec![
            b"meta\0",
            b"cmp\0",
            b"meta\0",
            b"cmp\0",
            b"ct\0",
            b"bitwise\0",
            b"cmp\0",
            b"immediate\0",
        ];
        assert_eq!(
            names(7),
            inbound,
            "new connections into the bridge from elsewhere"
        );
        let host_loopback: Vec<&[u8]> =
            vec![b"meta\0", b"cmp\0", b"payload\0", b"cmp\0", b"immediate\0"];
        assert_eq!(
            names(8),
            host_loopback,
            "traffic to pasta's host-loopback address"
        );
        let exprs = attrs(&get(&m[8].2, 4 | NLA_F_NESTED));
        let data = |i: usize| attrs(&get(&attrs(&exprs[i].1), 2 | NLA_F_NESTED));
        let value = |a: &[(u16, Vec<u8>)], ty: u16| get(&attrs(&get(a, ty | NLA_F_NESTED)), 1);
        assert_eq!(get(&data(2), 3), 16u32.to_be_bytes(), "daddr offset");
        assert_eq!(value(&data(3), 3), HOST_LOOPBACK);
        let verdict = attrs(&get(&data(4), 2 | NLA_F_NESTED));
        let code = attrs(&get(&verdict, 2 | NLA_F_NESTED));
        assert_eq!(get(&code, 1), 0u32.to_be_bytes(), "drop");
    }

    #[test]
    fn removal_flushes_then_deletes_the_chains() {
        let (bytes, last) = remove_masquerade_batch(7, "pbg-1");
        let m = messages(&bytes);
        let types: Vec<u16> = m.iter().map(|x| x.0).collect();
        assert_eq!(
            types,
            [0x10, SUBSYS | 8, SUBSYS | 5, SUBSYS | 8, SUBSYS | 5, 0x11]
        );
        assert_eq!(get(&m[2].2, 3), b"post-pbg-1\0");
        assert_eq!(get(&m[4].2, 3), b"fwd-pbg-1\0");
        assert_eq!(last, 11);
    }

    #[test]
    fn odd_prefixes_get_their_mask() {
        let (bytes, _) = masquerade_batch(1, [10, 77, 0, 0], 13, "b");
        let m = messages(&bytes);
        let exprs = attrs(&get(&m[4].2, 4 | NLA_F_NESTED));
        let bitwise = attrs(&get(&attrs(&exprs[1].1), 2 | NLA_F_NESTED));
        let mask = get(&attrs(&get(&bitwise, 4 | NLA_F_NESTED)), 1);
        assert_eq!(mask, [255, 248, 0, 0]);
    }
}
