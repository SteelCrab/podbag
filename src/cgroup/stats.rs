//! Statistics from a cgroup's files (cgroup v2), from `/proc/stat` and from
//! `/proc/<pid>/net/dev`. The parsers do not depend on the OS; `read` only reads files.

use std::path::Path;

/// `cpu.stat`: CPU time in microseconds, and how often the quota held the group back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cpu {
    pub usage_usec: u64,
    pub user_usec: u64,
    pub system_usec: u64,
    pub nr_periods: u64,
    pub nr_throttled: u64,
    pub throttled_usec: u64,
}

/// One device of `io.stat`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Io {
    pub major: u64,
    pub minor: u64,
    pub rbytes: u64,
    pub wbytes: u64,
    pub rios: u64,
    pub wios: u64,
}

/// One interface of `/proc/net/dev`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetDev {
    pub name: String,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}

/// What a cgroup reports. None or empty: the file is missing (no such controller).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub cpu: Option<Cpu>,
    /// `memory.current`.
    pub memory: Option<u64>,
    /// `memory.stat`, in the file's order.
    pub memory_stat: Vec<(String, u64)>,
    /// `memory.max`; None when it is `max` or missing.
    pub memory_max: Option<u64>,
    /// `memory.swap.current`.
    pub swap: Option<u64>,
    /// `pids.current`.
    pub pids: Option<u64>,
    /// `pids.max`; None when it is `max` or missing.
    pub pids_max: Option<u64>,
    /// `io.stat`.
    pub io: Vec<Io>,
}

impl Usage {
    /// A value of `memory.stat`.
    pub fn stat(&self, key: &str) -> Option<u64> {
        self.memory_stat
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| *v)
    }

    /// The memory in use minus the file cache the kernel can drop at once
    /// (`inactive_file`): what the docker CLI and the kubelet count.
    pub fn working_set(&self) -> Option<u64> {
        let current = self.memory?;
        let inactive = self.stat("inactive_file").unwrap_or(0);
        Some(current.saturating_sub(inactive))
    }

    /// CPU time in nanoseconds.
    pub fn cpu_ns(&self) -> Option<u64> {
        self.cpu.map(|c| c.usage_usec.saturating_mul(1000))
    }
}

/// A value from a `key value` file (`cpu.stat`, `memory.stat`).
pub fn field(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// Every `key value` line with a number.
pub fn pairs(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .filter_map(|(k, v)| Some((k.to_string(), v.trim().parse().ok()?)))
        .collect()
}

/// A limit file (`memory.max`, `pids.max`): a number, or None for `max`.
pub fn limit(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// `cpu.stat`. None without `usage_usec` (an empty or foreign file).
pub fn cpu_stat(text: &str) -> Option<Cpu> {
    let get = |key| field(text, key).unwrap_or(0);
    field(text, "usage_usec")?;
    Some(Cpu {
        usage_usec: get("usage_usec"),
        user_usec: get("user_usec"),
        system_usec: get("system_usec"),
        nr_periods: get("nr_periods"),
        nr_throttled: get("nr_throttled"),
        throttled_usec: get("throttled_usec"),
    })
}

/// `io.stat`: `8:0 rbytes=1 wbytes=2 rios=3 wios=4 dbytes=0 dios=0` per device.
pub fn io_stat(text: &str) -> Vec<Io> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let (major, minor) = words.next()?.split_once(':')?;
            let mut io = Io {
                major: major.parse().ok()?,
                minor: minor.parse().ok()?,
                ..Io::default()
            };
            for word in words {
                let Some((key, value)) = word.split_once('=') else {
                    continue;
                };
                let value = value.parse().unwrap_or(0);
                match key {
                    "rbytes" => io.rbytes = value,
                    "wbytes" => io.wbytes = value,
                    "rios" => io.rios = value,
                    "wios" => io.wios = value,
                    _ => {}
                }
            }
            Some(io)
        })
        .collect()
}

/// The interfaces of a `/proc/net/dev` file.
pub fn net_dev(text: &str) -> Vec<NetDev> {
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let n: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|v| v.parse().ok())
                .collect();
            (n.len() >= 12).then(|| NetDev {
                name: name.trim().to_string(),
                rx_bytes: n[0],
                rx_packets: n[1],
                rx_errors: n[2],
                rx_dropped: n[3],
                tx_bytes: n[8],
                tx_packets: n[9],
                tx_errors: n[10],
                tx_dropped: n[11],
            })
        })
        .collect()
}

/// The host's CPU time in nanoseconds from `/proc/stat`, as Docker counts it: the first
/// seven fields of the `cpu` line (user to softirq), in clock ticks.
pub fn system_cpu_ns(proc_stat: &str, ticks_per_second: u64) -> Option<u64> {
    let line = proc_stat.lines().find(|l| l.starts_with("cpu "))?;
    let ticks: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(7)
        .map(|v| v.parse().ok())
        .collect::<Option<_>>()?;
    if ticks.len() < 7 || ticks_per_second == 0 {
        return None;
    }
    let total: u128 = ticks.iter().map(|&t| u128::from(t)).sum();
    u64::try_from(total * 1_000_000_000 / u128::from(ticks_per_second)).ok()
}

/// The usage of the group at `dir`.
pub fn read(dir: &Path) -> Usage {
    let file = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let number = |name: &str| file(name).and_then(|t| t.trim().parse::<u64>().ok());
    Usage {
        cpu: file("cpu.stat").as_deref().and_then(cpu_stat),
        memory: number("memory.current"),
        memory_stat: file("memory.stat")
            .as_deref()
            .map(pairs)
            .unwrap_or_default(),
        memory_max: file("memory.max").as_deref().and_then(limit),
        swap: number("memory.swap.current"),
        pids: number("pids.current"),
        pids_max: file("pids.max").as_deref().and_then(limit),
        io: file("io.stat").as_deref().map(io_stat).unwrap_or_default(),
    }
}

/// The lowest `memory.max` of `dir` and the groups above it, up to (not including) `root`:
/// the memory the group may use at most (the memory budget sits on a group above).
pub fn memory_limit(dir: &Path, root: &Path) -> Option<u64> {
    let mut lowest: Option<u64> = None;
    let mut at = Some(dir);
    while let Some(d) = at.filter(|d| d.starts_with(root) && *d != root) {
        let max = std::fs::read_to_string(d.join("memory.max"))
            .ok()
            .as_deref()
            .and_then(limit);
        if let Some(m) = max {
            lowest = Some(lowest.map_or(m, |l| l.min(m)));
        }
        at = d.parent();
    }
    lowest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_cgroup_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let put = |name: &str, text: &str| std::fs::write(dir.join(name), text).unwrap();
        put(
            "cpu.stat",
            "usage_usec 2500\nuser_usec 2000\nsystem_usec 500\nnr_periods 7\n\
             nr_throttled 2\nthrottled_usec 900\n",
        );
        put("memory.current", "10000\n");
        put(
            "memory.stat",
            "anon 6000\nfile 4000\ninactive_file 3000\npgfault 77\n",
        );
        put("memory.max", "16384\n");
        put("pids.current", "3\n");
        put("pids.max", "max\n");
        put(
            "io.stat",
            "8:0 rbytes=4096 wbytes=8192 rios=1 wios=2 dbytes=0 dios=0\n",
        );
        let u = read(dir);
        assert_eq!(
            u.cpu,
            Some(Cpu {
                usage_usec: 2500,
                user_usec: 2000,
                system_usec: 500,
                nr_periods: 7,
                nr_throttled: 2,
                throttled_usec: 900,
            })
        );
        assert_eq!(u.cpu_ns(), Some(2_500_000));
        assert_eq!(u.memory, Some(10_000));
        assert_eq!(u.working_set(), Some(7000));
        assert_eq!(u.stat("anon"), Some(6000));
        assert_eq!(u.stat("pgfault"), Some(77));
        assert_eq!(u.memory_max, Some(16384));
        assert_eq!(u.pids, Some(3));
        assert_eq!(u.pids_max, None);
        assert_eq!(u.swap, None);
        assert_eq!(
            u.io,
            [Io {
                major: 8,
                minor: 0,
                rbytes: 4096,
                wbytes: 8192,
                rios: 1,
                wios: 2,
            }]
        );
        // A group without controllers reports nothing.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(read(empty.path()), Usage::default());
        assert_eq!(Usage::default().working_set(), None);
    }

    #[test]
    fn limits_and_odd_lines() {
        assert_eq!(limit("max\n"), None);
        assert_eq!(limit("4096\n"), Some(4096));
        assert_eq!(cpu_stat(""), None);
        assert_eq!(cpu_stat("usage_usec 5\n").unwrap().user_usec, 0);
        assert_eq!(
            pairs("a 1\nbad\nb x\nc 3\n"),
            [("a".into(), 1), ("c".into(), 3)]
        );
        assert_eq!(field("a 1\nb 2\n", "b"), Some(2));
        // A device line without values, and a line that is not a device.
        assert_eq!(
            io_stat("259:0\nnonsense\n"),
            [Io {
                major: 259,
                minor: 0,
                ..Io::default()
            }]
        );
    }

    #[test]
    fn interfaces_from_proc_net_dev() {
        let text = "Inter-|   Receive                                                |  Transmit\n \
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
lo:     100       2    0    0    0     0          0         0      100       2    0    0    0     0       0          0\n  \
eth0:    5000      40    1    3    0     0          0         0     7000      50    2    4    0     0       0          0\n";
        let devs = net_dev(text);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].name, "lo");
        assert_eq!(
            devs[1],
            NetDev {
                name: "eth0".into(),
                rx_bytes: 5000,
                rx_packets: 40,
                rx_errors: 1,
                rx_dropped: 3,
                tx_bytes: 7000,
                tx_packets: 50,
                tx_errors: 2,
                tx_dropped: 4,
            }
        );
        assert!(net_dev("").is_empty());
    }

    #[test]
    fn system_cpu_time_like_docker() {
        // user nice system idle iowait irq softirq steal: the first seven count.
        let stat = "cpu  100 20 30 400 5 6 7 99 0 0\ncpu0 1 2 3 4 5 6 7 8 0 0\n";
        assert_eq!(system_cpu_ns(stat, 100), Some(568 * 10_000_000));
        assert_eq!(system_cpu_ns("cpu  1 2\n", 100), None);
        assert_eq!(system_cpu_ns("intr 5\n", 100), None);
        assert_eq!(system_cpu_ns(stat, 0), None);
    }

    #[test]
    fn the_lowest_limit_on_the_way_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let parent = root.join("podbag");
        let group = parent.join("c1");
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(root.join("memory.max"), "1\n").unwrap();
        std::fs::write(parent.join("memory.max"), "8192\n").unwrap();
        std::fs::write(group.join("memory.max"), "max\n").unwrap();
        // The root's own file is not read: the root group has no limit.
        assert_eq!(memory_limit(&group, root), Some(8192));
        std::fs::write(group.join("memory.max"), "4096\n").unwrap();
        assert_eq!(memory_limit(&group, root), Some(4096));
        std::fs::write(parent.join("memory.max"), "max\n").unwrap();
        std::fs::write(group.join("memory.max"), "max\n").unwrap();
        assert_eq!(memory_limit(&group, root), None);
    }
}
