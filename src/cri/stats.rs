//! Container and pod statistics from cgroup v2 files, the writable layer and the pod's
//! network namespace.

use std::path::Path;

use super::pb;

/// What a cgroup reports. None: the file is missing (no such controller, or no group).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub cpu_ns: Option<u64>,
    pub memory: Option<u64>,
    pub working_set: Option<u64>,
    pub anon: Option<u64>,
    pub page_faults: Option<u64>,
    pub major_page_faults: Option<u64>,
    pub limit: Option<u64>,
    pub swap: Option<u64>,
    pub pids: Option<u64>,
}

/// A value from a `key value` file (`cpu.stat`, `memory.stat`).
pub fn field(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// The usage of the group at `path`.
pub fn read(path: &str) -> Usage {
    let dir = Path::new(path);
    let file = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let number = |name: &str| file(name).and_then(|t| t.trim().parse::<u64>().ok());
    from_files(
        file("cpu.stat").as_deref(),
        number("memory.current"),
        file("memory.stat").as_deref(),
        file("memory.max").as_deref(),
        number("memory.swap.current"),
        number("pids.current"),
    )
}

/// The usage from the files' contents. The working set is the memory in use minus the
/// file cache the kernel can drop at once (`inactive_file`), as the kubelet counts it.
pub fn from_files(
    cpu_stat: Option<&str>,
    current: Option<u64>,
    memory_stat: Option<&str>,
    memory_max: Option<&str>,
    swap: Option<u64>,
    pids: Option<u64>,
) -> Usage {
    let stat = |key: &str| memory_stat.and_then(|t| field(t, key));
    let working_set = current.map(|c| c.saturating_sub(stat("inactive_file").unwrap_or(0)));
    Usage {
        cpu_ns: cpu_stat
            .and_then(|t| field(t, "usage_usec"))
            .map(|us| us * 1000),
        memory: current,
        working_set,
        anon: stat("anon"),
        page_faults: stat("pgfault"),
        major_page_faults: stat("pgmajfault"),
        limit: memory_max.and_then(|m| m.trim().parse().ok()),
        swap,
        pids,
    }
}

fn value(v: Option<u64>) -> Option<pb::UInt64Value> {
    v.map(|value| pb::UInt64Value { value })
}

impl Usage {
    pub fn cpu(&self, timestamp: i64) -> pb::CpuUsage {
        pb::CpuUsage {
            timestamp,
            usage_core_nano_seconds: value(self.cpu_ns),
            usage_nano_cores: None,
            psi: None,
        }
    }

    pub fn memory(&self, timestamp: i64) -> pb::MemoryUsage {
        let available = match (self.limit, self.working_set) {
            (Some(limit), Some(ws)) => Some(limit.saturating_sub(ws)),
            _ => None,
        };
        pb::MemoryUsage {
            timestamp,
            working_set_bytes: value(self.working_set),
            available_bytes: value(available),
            usage_bytes: value(self.memory),
            rss_bytes: value(self.anon),
            page_faults: value(self.page_faults),
            major_page_faults: value(self.major_page_faults),
            psi: None,
        }
    }

    pub fn swap(&self, timestamp: i64) -> pb::SwapUsage {
        pb::SwapUsage {
            timestamp,
            swap_available_bytes: None,
            swap_usage_bytes: value(self.swap),
        }
    }
}

/// Bytes on disk (in blocks, as `du` counts them) and inodes below `path`, without
/// following links or leaving its file system.
pub fn disk_usage(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let Ok(top) = std::fs::symlink_metadata(path) else {
        return (0, 0);
    };
    let dev = top.dev();
    let (mut bytes, mut inodes) = (top.blocks() * 512, 1);
    let mut todo = vec![path.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.dev() != dev {
                continue;
            }
            bytes += meta.blocks() * 512;
            inodes += 1;
            if meta.is_dir() {
                todo.push(entry.path());
            }
        }
    }
    (bytes, inodes)
}

/// One interface of `/proc/net/dev`: name, received bytes and errors, sent bytes and
/// errors.
pub fn net_dev(text: &str) -> Vec<(String, u64, u64, u64, u64)> {
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let n: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|v| v.parse().ok())
                .collect();
            (n.len() >= 11).then(|| (name.trim().to_string(), n[0], n[2], n[8], n[10]))
        })
        .collect()
}

/// The pod's network use, read inside its namespace (a thread of its own joins it).
#[cfg(target_os = "linux")]
pub fn network(netns: &str, timestamp: i64) -> pb::NetworkUsage {
    let path = netns.to_string();
    let text = std::thread::spawn(move || -> Option<String> {
        let ns = std::fs::File::open(path).ok()?;
        // SAFETY: setns on a namespace file we opened; only this thread changes, and it ends.
        if unsafe { libc::setns(std::os::fd::AsRawFd::as_raw_fd(&ns), libc::CLONE_NEWNET) } != 0 {
            return None;
        }
        std::fs::read_to_string("/proc/thread-self/net/dev").ok()
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or_default();
    let interfaces: Vec<pb::NetworkInterfaceUsage> = net_dev(&text)
        .into_iter()
        .filter(|(name, ..)| name != "lo")
        .map(|(name, rx, rx_err, tx, tx_err)| pb::NetworkInterfaceUsage {
            name,
            rx_bytes: value(Some(rx)),
            rx_errors: value(Some(rx_err)),
            tx_bytes: value(Some(tx)),
            tx_errors: value(Some(tx_err)),
        })
        .collect();
    pb::NetworkUsage {
        timestamp,
        default_interface: interfaces.iter().find(|i| i.name == "eth0").cloned(),
        interfaces,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_cgroup_files() {
        let u = from_files(
            Some("usage_usec 2500\nuser_usec 2000\nsystem_usec 500\n"),
            Some(10_000),
            Some("anon 6000\nfile 4000\ninactive_file 3000\npgfault 77\npgmajfault 1\n"),
            Some("16384\n"),
            None,
            Some(3),
        );
        assert_eq!(u.cpu_ns, Some(2_500_000));
        assert_eq!(u.working_set, Some(7000));
        assert_eq!(u.anon, Some(6000));
        let m = u.memory(5);
        assert_eq!(m.available_bytes.unwrap().value, 16384 - 7000);
        assert_eq!(m.usage_bytes.unwrap().value, 10_000);
        assert_eq!(m.page_faults.unwrap().value, 77);
        assert_eq!(u.pids, Some(3));
        let unlimited = from_files(None, Some(1), None, Some("max\n"), None, None);
        assert_eq!(unlimited.limit, None);
        assert_eq!(unlimited.memory(0).available_bytes, None);
        assert_eq!(unlimited.cpu(0).usage_core_nano_seconds, None);
    }

    #[test]
    fn interfaces_from_proc_net_dev() {
        let text = "Inter-|   Receive                                                |  Transmit\n \
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
lo:     100       2    0    0    0     0          0         0      100       2    0    0    0     0       0          0\n  \
eth0:    5000      40    1    0    0     0          0         0     7000      50    2    0    0     0       0          0\n";
        assert_eq!(
            net_dev(text),
            [
                ("lo".to_string(), 100, 0, 100, 0),
                ("eth0".to_string(), 5000, 1, 7000, 2)
            ]
        );
    }

    #[test]
    fn disk_usage_counts_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join("d/f"), vec![1u8; 10_000]).unwrap();
        let (bytes, inodes) = disk_usage(tmp.path());
        assert_eq!(inodes, 3);
        assert!(bytes >= 10_000, "{bytes}");
        assert_eq!(disk_usage(&tmp.path().join("missing")), (0, 0));
    }
}
