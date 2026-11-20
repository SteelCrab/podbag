//! A swarm service's NAT rules in podbag's table (root mode): published ports and the
//! service's virtual address go to its tasks, spread at random.
//!
//! Three chains per service, named after the start of its ID: `svc-<id>-pre`
//! (prerouting: from other hosts), `svc-<id>-out` (output: from this host) and
//! `svc-<id>-post` (postrouting: answers come back through this host). A random spread
//! needs no sets: rule `i` of `n` matches `numgen random mod (n - i) == 0`, so each task
//! gets one connection in `n`, and the last rule takes what is left.

use super::Message;
use super::nft::{self, TABLE};

/// A transport protocol of a published port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    fn number(self) -> u8 {
        match self {
            Proto::Tcp => 6,
            Proto::Udp => 17,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// A published port: connections to `port` on the host go to `target` on a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    pub proto: Proto,
    pub port: u16,
    pub target: u16,
}

/// What a service's rules are made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceNat {
    /// The start of the service's ID: the chains' names.
    pub chain_id: String,
    /// The bridge of the tasks' network.
    pub bridge: String,
    /// The network's subnet, for the rules' mark.
    pub subnet: ([u8; 4], u8),
    pub ports: Vec<Published>,
    /// The service's virtual address on the network.
    pub vip: Option<[u8; 4]>,
    /// The tasks' addresses on the network.
    pub tasks: Vec<[u8; 4]>,
}

/// One condition of a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Match {
    /// The destination is an address of this host.
    LocalDestination,
    Protocol(Proto),
    DestinationPort(Proto, u16),
    Destination([u8; 4]),
    /// `numgen random mod <n> == 0`.
    OneIn(u32),
    OutputInterface(String),
    /// The connection's destination was changed by a DNAT rule.
    Dnatted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Dnat { addr: [u8; 4], port: Option<u16> },
    Masquerade,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    matches: Vec<Match>,
    action: Action,
}

/// Which chain a rule goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chain {
    Pre,
    Out,
    Post,
}

const CHAINS: [Chain; 3] = [Chain::Pre, Chain::Out, Chain::Post];

fn chain_name(chain_id: &str, chain: Chain) -> String {
    let suffix = match chain {
        Chain::Pre => "pre",
        Chain::Out => "out",
        Chain::Post => "post",
    };
    format!("svc-{chain_id}-{suffix}")
}

/// The DNAT rules (the same in prerouting and output), then the masquerade rule.
fn rules(nat: &ServiceNat) -> Vec<(Chain, Rule)> {
    let n = nat.tasks.len();
    // Rule `i` of `n` takes one connection in `n - i` of those left: one in `n` overall.
    let spread = |before: Vec<Match>, port: Option<u16>| -> Vec<Rule> {
        nat.tasks
            .iter()
            .enumerate()
            .map(|(i, addr)| {
                let mut matches = before.clone();
                let left = (n - i) as u32;
                if left > 1 {
                    matches.push(Match::OneIn(left));
                }
                Rule {
                    matches,
                    action: Action::Dnat { addr: *addr, port },
                }
            })
            .collect()
    };
    let mut dnat = Vec::new();
    for p in &nat.ports {
        dnat.extend(spread(
            vec![
                Match::LocalDestination,
                Match::Protocol(p.proto),
                Match::DestinationPort(p.proto, p.port),
            ],
            Some(p.target),
        ));
    }
    if let Some(vip) = nat.vip {
        dnat.extend(spread(vec![Match::Destination(vip)], None));
    }
    let mut out: Vec<(Chain, Rule)> = Vec::with_capacity(2 * dnat.len() + 1);
    out.extend(dnat.iter().cloned().map(|r| (Chain::Pre, r)));
    out.extend(dnat.into_iter().map(|r| (Chain::Out, r)));
    // Answers must come back through this host to be translated back: a task would answer
    // a client on its own bridge (or this host's loopback) directly otherwise.
    out.push((
        Chain::Post,
        Rule {
            matches: vec![Match::OutputInterface(nat.bridge.clone()), Match::Dnatted],
            action: Action::Masquerade,
        },
    ));
    out
}

fn ip(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

/// A rule as `nft list` would show it, nearly: for tests and messages.
fn text(rule: &Rule) -> String {
    let mut parts: Vec<String> = rule
        .matches
        .iter()
        .map(|m| match m {
            Match::LocalDestination => "fib daddr type local".to_string(),
            Match::Protocol(p) => format!("meta l4proto {}", p.name()),
            Match::DestinationPort(p, port) => format!("{} dport {port}", p.name()),
            Match::Destination(a) => format!("ip daddr {}", ip(*a)),
            Match::OneIn(n) => format!("numgen random mod {n} == 0"),
            Match::OutputInterface(name) => format!("oifname {name}"),
            Match::Dnatted => "ct status dnat".to_string(),
        })
        .collect();
    parts.push(match &rule.action {
        Action::Dnat {
            addr,
            port: Some(port),
        } => format!("dnat to {}:{port}", ip(*addr)),
        Action::Dnat { addr, port: None } => format!("dnat to {}", ip(*addr)),
        Action::Masquerade => "masquerade".to_string(),
    });
    parts.join(" ")
}

const NFT_META_L4PROTO: u32 = 16;
const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;
const NFT_FIB_RESULT_ADDRTYPE: u32 = 3;
const NFTA_FIB_F_DADDR: u32 = 2;
const RTN_LOCAL: u32 = 2;
const NFT_NG_RANDOM: u32 = 1;
const NFT_NAT_DNAT: u32 = 1;
const NFPROTO_IPV4: u32 = 2;
const NFT_REG_2: u32 = 2;
const NF_INET_PRE_ROUTING: u32 = 0;
const NF_INET_LOCAL_OUT: u32 = 3;
/// `dstnat`.
const DNAT_PRIORITY: i32 = -100;

/// `cmp eq <bytes>` on register 1.
fn equals(m: &mut Message, bytes: &[u8]) {
    nft::expr(
        m,
        "cmp",
        Some(&|m: &mut Message| {
            nft::be(m, 1, nft::NFT_REG_1);
            nft::be(m, 2, nft::NFT_CMP_EQ);
            nft::value(m, 3, bytes);
        }),
    );
}

/// `payload load <len> bytes at <offset> of <base>` into register 1.
fn payload(m: &mut Message, base: u32, offset: u32, len: u32) {
    nft::expr(
        m,
        "payload",
        Some(&|m: &mut Message| {
            nft::be(m, 1, nft::NFT_REG_1);
            nft::be(m, 2, base);
            nft::be(m, 3, offset);
            nft::be(m, 4, len);
        }),
    );
}

fn immediate(m: &mut Message, register: u32, bytes: &[u8]) {
    nft::expr(
        m,
        "immediate",
        Some(&|m: &mut Message| {
            nft::be(m, 1, register);
            nft::value(m, 2, bytes);
        }),
    );
}

fn expressions(m: &mut Message, rule: &Rule) {
    for condition in &rule.matches {
        match condition {
            Match::LocalDestination => {
                nft::expr(
                    m,
                    "fib",
                    Some(&|m: &mut Message| {
                        nft::be(m, 1, nft::NFT_REG_1);
                        nft::be(m, 2, NFT_FIB_RESULT_ADDRTYPE);
                        nft::be(m, 3, NFTA_FIB_F_DADDR);
                    }),
                );
                equals(m, &RTN_LOCAL.to_ne_bytes());
            }
            Match::Protocol(p) => {
                nft::expr(
                    m,
                    "meta",
                    Some(&|m: &mut Message| {
                        nft::be(m, 1, nft::NFT_REG_1);
                        nft::be(m, 2, NFT_META_L4PROTO);
                    }),
                );
                equals(m, &[p.number()]);
            }
            Match::DestinationPort(_, port) => {
                payload(m, NFT_PAYLOAD_TRANSPORT_HEADER, 2, 2);
                equals(m, &port.to_be_bytes());
            }
            Match::Destination(addr) => {
                payload(m, nft::NFT_PAYLOAD_NETWORK_HEADER, 16, 4);
                equals(m, addr);
            }
            Match::OneIn(n) => {
                nft::expr(
                    m,
                    "numgen",
                    Some(&|m: &mut Message| {
                        nft::be(m, 1, nft::NFT_REG_1);
                        nft::be(m, 2, *n);
                        nft::be(m, 3, NFT_NG_RANDOM);
                    }),
                );
                equals(m, &0u32.to_ne_bytes());
            }
            Match::OutputInterface(name) => {
                nft::match_ifname(m, nft::NFT_META_OIFNAME, nft::NFT_CMP_EQ, name);
            }
            Match::Dnatted => nft::match_ct_status(m, nft::CT_STATUS_DNAT, nft::NFT_CMP_NEQ),
        }
    }
    match &rule.action {
        Action::Dnat { addr, port } => {
            immediate(m, nft::NFT_REG_1, addr);
            if let Some(port) = port {
                immediate(m, NFT_REG_2, &port.to_be_bytes());
            }
            nft::expr(
                m,
                "nat",
                Some(&|m: &mut Message| {
                    nft::be(m, 1, NFT_NAT_DNAT);
                    nft::be(m, 2, NFPROTO_IPV4);
                    nft::be(m, 3, nft::NFT_REG_1);
                    if port.is_some() {
                        nft::be(m, 5, NFT_REG_2);
                    }
                }),
            );
        }
        Action::Masquerade => nft::expr(m, "masq", None),
    }
}

/// The rules as text, with their chains' names: what [`apply_batch`] writes.
pub fn rules_text(nat: &ServiceNat) -> Vec<(String, String)> {
    rules(nat)
        .iter()
        .map(|(chain, rule)| (chain_name(&nat.chain_id, *chain), text(rule)))
        .collect()
}

/// Create (if needed) the service's chains and write its rules, replacing the old ones.
pub fn apply_batch(first_seq: u32, nat: &ServiceNat) -> (Vec<u8>, u32) {
    let mut table = nft::message(nft::NFT_MSG_NEWTABLE, super::NLM_F_CREATE);
    table.attr_str(1, TABLE);
    let mut msgs = vec![table];
    for chain in CHAINS {
        let name = chain_name(&nat.chain_id, chain);
        let (hook, priority) = match chain {
            Chain::Pre => (NF_INET_PRE_ROUTING, DNAT_PRIORITY as u32),
            Chain::Out => (NF_INET_LOCAL_OUT, DNAT_PRIORITY as u32),
            Chain::Post => (nft::NF_INET_POST_ROUTING, nft::NAT_PRIORITY),
        };
        msgs.push(nft::base_chain(&name, hook, priority, "nat"));
        msgs.push(nft::flush(&name));
    }
    let mark = nft::rules_mark(nat.subnet.0, nat.subnet.1);
    for (chain, rule) in rules(nat) {
        let name = chain_name(&nat.chain_id, chain);
        msgs.push(nft::rule(&name, &mark, |m| expressions(m, &rule)));
    }
    nft::batch(first_seq, msgs)
}

/// Remove the service's chains: flush each, then delete it.
pub fn remove_batch(first_seq: u32, chain_id: &str) -> (Vec<u8>, u32) {
    let mut msgs = Vec::new();
    for chain in CHAINS {
        let name = chain_name(chain_id, chain);
        msgs.push(nft::flush(&name));
        let mut delete = nft::message(nft::NFT_MSG_DELCHAIN, 0);
        delete.attr_str(1, TABLE);
        delete.attr_str(3, &name);
        msgs.push(delete);
    }
    nft::batch(first_seq, msgs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::NLA_F_NESTED;

    fn nat(tasks: usize) -> ServiceNat {
        ServiceNat {
            chain_id: "abc123".into(),
            bridge: "pbg-1".into(),
            subnet: ([10, 78, 1, 0], 24),
            ports: vec![
                Published {
                    proto: Proto::Tcp,
                    port: 8080,
                    target: 80,
                },
                Published {
                    proto: Proto::Udp,
                    port: 5353,
                    target: 53,
                },
            ],
            vip: Some([10, 78, 1, 9]),
            tasks: (0..tasks).map(|i| [10, 78, 1, 2 + i as u8]).collect(),
        }
    }

    #[test]
    fn rules_spread_ports_and_the_virtual_address_over_tasks() {
        let text = rules_text(&nat(3));
        let pre: Vec<&str> = text
            .iter()
            .filter(|(c, _)| c == "svc-abc123-pre")
            .map(|(_, r)| r.as_str())
            .collect();
        assert_eq!(
            pre,
            [
                "fib daddr type local meta l4proto tcp tcp dport 8080 numgen random mod 3 == 0 dnat to 10.78.1.2:80",
                "fib daddr type local meta l4proto tcp tcp dport 8080 numgen random mod 2 == 0 dnat to 10.78.1.3:80",
                "fib daddr type local meta l4proto tcp tcp dport 8080 dnat to 10.78.1.4:80",
                "fib daddr type local meta l4proto udp udp dport 5353 numgen random mod 3 == 0 dnat to 10.78.1.2:53",
                "fib daddr type local meta l4proto udp udp dport 5353 numgen random mod 2 == 0 dnat to 10.78.1.3:53",
                "fib daddr type local meta l4proto udp udp dport 5353 dnat to 10.78.1.4:53",
                "ip daddr 10.78.1.9 numgen random mod 3 == 0 dnat to 10.78.1.2",
                "ip daddr 10.78.1.9 numgen random mod 2 == 0 dnat to 10.78.1.3",
                "ip daddr 10.78.1.9 dnat to 10.78.1.4",
            ]
        );
        let out: Vec<&str> = text
            .iter()
            .filter(|(c, _)| c == "svc-abc123-out")
            .map(|(_, r)| r.as_str())
            .collect();
        assert_eq!(out, pre, "the host's own connections too");
        let post: Vec<&str> = text
            .iter()
            .filter(|(c, _)| c == "svc-abc123-post")
            .map(|(_, r)| r.as_str())
            .collect();
        assert_eq!(post, ["oifname pbg-1 ct status dnat masquerade"]);
    }

    #[test]
    fn no_tasks_no_dnat() {
        let text = rules_text(&nat(0));
        assert_eq!(
            text,
            [(
                "svc-abc123-post".to_string(),
                "oifname pbg-1 ct status dnat masquerade".to_string()
            )]
        );
        let one = rules_text(&nat(1));
        assert!(
            one.iter()
                .any(|(_, r)| r == "ip daddr 10.78.1.9 dnat to 10.78.1.2"),
            "{one:?}"
        );
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

    type Attrs = Vec<(u16, Vec<u8>)>;

    /// (message type, attributes) of each message in a batch.
    fn messages(bytes: &[u8]) -> Vec<(u16, Attrs)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let len = u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let ty = u16::from_ne_bytes([bytes[at + 4], bytes[at + 5]]);
            out.push((ty, attrs(&bytes[at + 20..at + len])));
            at += len;
        }
        out
    }

    const SUBSYS: u16 = 10 << 8;

    #[test]
    fn the_batch_makes_three_nat_chains_and_the_rules() {
        let n = nat(2);
        let (bytes, last) = apply_batch(10, &n);
        let m = messages(&bytes);
        let rule_count = rules(&n).len();
        // begin, table, 3 × (chain, flush), the rules, end.
        assert_eq!(m.len(), 1 + 1 + 6 + rule_count + 1);
        assert_eq!(last, 10 + 1 + 6 + rule_count as u32);
        let chains: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = m
            .iter()
            .filter(|(ty, _)| *ty == SUBSYS | 3)
            .map(|(_, a)| {
                let hook = attrs(&get(a, 4 | NLA_F_NESTED));
                (get(a, 3), get(&hook, 1), get(&hook, 2))
            })
            .collect();
        assert_eq!(
            chains,
            [
                (
                    b"svc-abc123-pre\0".to_vec(),
                    0u32.to_be_bytes().to_vec(),
                    (-100i32).to_be_bytes().to_vec()
                ),
                (
                    b"svc-abc123-out\0".to_vec(),
                    3u32.to_be_bytes().to_vec(),
                    (-100i32).to_be_bytes().to_vec()
                ),
                (
                    b"svc-abc123-post\0".to_vec(),
                    4u32.to_be_bytes().to_vec(),
                    100i32.to_be_bytes().to_vec()
                ),
            ]
        );
        // The first rule: its expressions in order, and its mark.
        let first = m.iter().find(|(ty, _)| *ty == SUBSYS | 6).unwrap();
        let exprs = attrs(&get(&first.1, 4 | NLA_F_NESTED));
        let names: Vec<Vec<u8>> = exprs.iter().map(|(_, e)| get(&attrs(e), 1)).collect();
        let want: Vec<&[u8]> = vec![
            b"fib\0",
            b"cmp\0",
            b"meta\0",
            b"cmp\0",
            b"payload\0",
            b"cmp\0",
            b"numgen\0",
            b"cmp\0",
            b"immediate\0",
            b"immediate\0",
            b"nat\0",
        ];
        assert_eq!(names, want);
        let mark = get(&first.1, 7);
        assert!(
            mark.windows(b"podbag rules".len())
                .any(|w| w == b"podbag rules"),
            "{mark:?}"
        );
        let data = |i: usize| attrs(&get(&attrs(&exprs[i].1), 2 | NLA_F_NESTED));
        let value = |a: &[(u16, Vec<u8>)], ty: u16| get(&attrs(&get(a, ty | NLA_F_NESTED)), 1);
        assert_eq!(
            get(&data(0), 3),
            2u32.to_be_bytes(),
            "fib on the destination"
        );
        assert_eq!(get(&data(0), 2), 3u32.to_be_bytes(), "the address type");
        assert_eq!(get(&data(6), 2), 2u32.to_be_bytes(), "mod 2");
        assert_eq!(get(&data(6), 3), 1u32.to_be_bytes(), "random");
        assert_eq!(value(&data(5), 3), 8080u16.to_be_bytes(), "dport");
        assert_eq!(value(&data(8), 2), [10, 78, 1, 2], "address");
        assert_eq!(value(&data(9), 2), 80u16.to_be_bytes(), "port");
        assert_eq!(get(&data(10), 1), 1u32.to_be_bytes(), "dnat");
    }

    #[test]
    fn removal_flushes_and_deletes_the_three_chains() {
        let (bytes, last) = remove_batch(1, "abc123");
        let m = messages(&bytes);
        let types: Vec<u16> = m.iter().map(|x| x.0).collect();
        assert_eq!(
            types,
            [
                0x10,
                SUBSYS | 8,
                SUBSYS | 5,
                SUBSYS | 8,
                SUBSYS | 5,
                SUBSYS | 8,
                SUBSYS | 5,
                0x11
            ]
        );
        assert_eq!(get(&m[6].1, 3), b"svc-abc123-post\0");
        assert_eq!(last, 7);
    }
}
