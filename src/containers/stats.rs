//! `stats`: a container's resource use, from its cgroup (cgroup v2), the host's
//! `/proc/stat` and the container's network namespace (`/proc/<pid>/net/dev` of the
//! host's procfs). Docker's JSON and the docker CLI's table are made here too.

use super::ContainerHandle;
use crate::cgroup::stats::{NetDev, Usage};
use crate::state::Status;
use crate::{Result, paths};
use serde_json::{Value, json};

/// One look at a container.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// When it was read, in nanoseconds since the epoch; 0 when it does not run.
    pub read_ns: u128,
    /// The cgroup's values; None without a cgroup (or when it does not run).
    pub cgroup: Option<Usage>,
    /// The memory it may use: its limit, a limit above it (the memory budget), or the
    /// host's memory.
    pub memory_limit: Option<u64>,
    /// The host's CPU time, in nanoseconds.
    pub system_cpu_ns: Option<u64>,
    pub online_cpus: u32,
    /// Its network interfaces, without loopback; none on the host's network.
    pub networks: Vec<NetDev>,
    /// Its processes and threads (without a cgroup: its processes).
    pub pids: Option<u64>,
    /// Why values are missing, when they are.
    pub missing: Option<String>,
}

impl ContainerHandle {
    /// Read the container's resource use now (`docker stats`). A container that does not
    /// run gives an empty sample.
    pub fn stats(&self) -> Result<Sample> {
        let dirs = paths::dirs()?;
        let info = self.info()?;
        let (Status::Running, Some(pid)) = (info.state.status, info.state.pid) else {
            return Ok(Sample::default());
        };
        let spec = crate::state::read_config(&dirs, &self.id)?;
        let mut sample = Sample {
            read_ns: now_ns(),
            system_cpu_ns: system_cpu_ns(),
            online_cpus: online_cpus(),
            ..Sample::default()
        };
        if !matches!(spec.network, crate::network::NetworkMode::Host) {
            let text = std::fs::read_to_string(format!("/proc/{pid}/net/dev")).unwrap_or_default();
            sample.networks = crate::cgroup::stats::net_dev(&text)
                .into_iter()
                .filter(|d| d.name != "lo")
                .collect();
        }
        let host_memory = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| crate::cgroup::mem_total(&m));
        match info.state.cgroup.as_deref() {
            Some(group) => {
                let dir = std::path::Path::new(group);
                let usage = crate::cgroup::stats::read(dir);
                let root = std::path::Path::new("/sys/fs/cgroup");
                let limit = crate::cgroup::stats::memory_limit(dir, root);
                sample.memory_limit = match (limit, host_memory) {
                    (Some(l), Some(h)) => Some(l.min(h)),
                    (l, h) => l.or(h),
                };
                sample.pids = usage.pids;
                sample.cgroup = Some(usage);
            }
            None => {
                sample.memory_limit = host_memory;
                sample.pids = process_count(pid);
                sample.missing = Some(format!(
                    "it runs without a cgroup ({}): CPU, memory and block I/O are unknown",
                    super::pause::no_cgroup_reason(&dirs, &self.id)
                ));
            }
        }
        Ok(sample)
    }
}

fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn system_cpu_ns() -> Option<u64> {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        crate::cgroup::stats::system_cpu_ns(&text, hz)
    }
    #[cfg(not(unix))]
    None
}

fn online_cpus() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions.
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if n > 0 {
            return n as u32;
        }
    }
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

/// The processes of the container without a cgroup: those of its PID namespace.
fn process_count(init: u32) -> Option<u64> {
    let own = std::fs::read_link(format!("/proc/{init}/ns/pid")).ok()?;
    let n = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.parse::<u32>().is_ok())
                && std::fs::read_link(e.path().join("ns/pid")).ok().as_ref() == Some(&own)
        })
        .count();
    Some(n as u64)
}

/// RFC 3339 with nanoseconds; Go's zero time for 0.
fn time_text(ns: u128) -> String {
    if ns == 0 {
        return "0001-01-01T00:00:00Z".to_string();
    }
    crate::shim::logbook::rfc3339((ns / 1_000_000_000) as u64, (ns % 1_000_000_000) as u32)
}

fn cpu_json(s: &Sample) -> Value {
    let cpu = s.cgroup.as_ref().and_then(|u| u.cpu).unwrap_or_default();
    let mut v = json!({
        "cpu_usage": {
            "total_usage": cpu.usage_usec * 1000,
            "usage_in_kernelmode": cpu.system_usec * 1000,
            "usage_in_usermode": cpu.user_usec * 1000,
        },
        "throttling_data": {
            "periods": cpu.nr_periods,
            "throttled_periods": cpu.nr_throttled,
            "throttled_time": cpu.throttled_usec * 1000,
        },
    });
    if s.read_ns != 0 {
        v["system_cpu_usage"] = json!(s.system_cpu_ns.unwrap_or(0));
        v["online_cpus"] = json!(s.online_cpus);
    }
    v
}

impl Sample {
    /// Docker's stats JSON, with `previous` as `precpu_stats` (empty without one).
    pub fn docker_json(&self, previous: Option<&Sample>, id: &str, name: &str) -> Value {
        let empty = Sample::default();
        let pre = previous.unwrap_or(&empty);
        let usage = self.cgroup.clone().unwrap_or_default();
        let mut memory = json!({});
        if let (true, Some(current)) = (self.read_ns != 0, usage.memory) {
            memory = json!({
                "usage": current,
                "stats": usage
                    .memory_stat
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect::<serde_json::Map<String, Value>>(),
                "limit": self.memory_limit.unwrap_or(0),
            });
        }
        let blkio: Vec<Value> = usage
            .io
            .iter()
            .flat_map(|d| {
                [("read", d.rbytes), ("write", d.wbytes)].map(|(op, value)| {
                    json!({"major": d.major, "minor": d.minor, "op": op, "value": value})
                })
            })
            .collect();
        let mut v = json!({
            "name": format!("/{name}"),
            "id": id,
            "read": time_text(self.read_ns),
            "preread": time_text(pre.read_ns),
            "pids_stats": {},
            "blkio_stats": {
                "io_service_bytes_recursive": if blkio.is_empty() { Value::Null } else { json!(blkio) },
                "io_serviced_recursive": null,
                "io_queue_recursive": null,
                "io_service_time_recursive": null,
                "io_wait_time_recursive": null,
                "io_merged_recursive": null,
                "io_time_recursive": null,
                "sectors_recursive": null,
            },
            "num_procs": 0,
            "storage_stats": {},
            "cpu_stats": cpu_json(self),
            "precpu_stats": cpu_json(pre),
            "memory_stats": memory,
        });
        if let Some(current) = self.pids {
            v["pids_stats"] = json!({"current": current});
            if let Some(limit) = usage.pids_max {
                v["pids_stats"]["limit"] = json!(limit);
            }
        }
        if !self.networks.is_empty() {
            v["networks"] = self
                .networks
                .iter()
                .map(|d| {
                    (
                        d.name.clone(),
                        json!({
                            "rx_bytes": d.rx_bytes,
                            "rx_packets": d.rx_packets,
                            "rx_errors": d.rx_errors,
                            "rx_dropped": d.rx_dropped,
                            "tx_bytes": d.tx_bytes,
                            "tx_packets": d.tx_packets,
                            "tx_errors": d.tx_errors,
                            "tx_dropped": d.tx_dropped,
                        }),
                    )
                })
                .collect::<serde_json::Map<String, Value>>()
                .into();
        }
        v
    }
}

/// One line of the `stats` table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub cpu_percent: f64,
    pub memory: u64,
    pub memory_limit: u64,
    pub memory_percent: f64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub block_read: u64,
    pub block_write: u64,
    pub pids: u64,
}

impl Row {
    /// The docker CLI's numbers from two samples: CPU time against the host's CPU time,
    /// memory without the inactive file cache.
    pub fn new(id: &str, name: &str, previous: Option<&Sample>, now: &Sample) -> Row {
        let cpu = |s: &Sample| s.cgroup.as_ref().and_then(Usage::cpu_ns).unwrap_or(0) as f64;
        let system = |s: &Sample| s.system_cpu_ns.unwrap_or(0) as f64;
        let cpu_percent = match previous {
            Some(p) => {
                let (cpu_delta, system_delta) = (cpu(now) - cpu(p), system(now) - system(p));
                if cpu_delta > 0.0 && system_delta > 0.0 {
                    cpu_delta / system_delta * f64::from(now.online_cpus) * 100.0
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        let usage = now.cgroup.clone().unwrap_or_default();
        let memory = usage.working_set().unwrap_or(0);
        let memory_limit = if now.read_ns == 0 {
            0
        } else {
            now.memory_limit.unwrap_or(0)
        };
        let memory_percent = if memory_limit > 0 {
            memory as f64 / memory_limit as f64 * 100.0
        } else {
            0.0
        };
        Row {
            id: id.to_string(),
            name: name.to_string(),
            cpu_percent,
            memory,
            memory_limit,
            memory_percent,
            net_rx: now.networks.iter().map(|d| d.rx_bytes).sum(),
            net_tx: now.networks.iter().map(|d| d.tx_bytes).sum(),
            block_read: usage.io.iter().map(|d| d.rbytes).sum(),
            block_write: usage.io.iter().map(|d| d.wbytes).sum(),
            pids: now.pids.unwrap_or(0),
        }
    }
}

/// Go's `%.<precision>g`: that many significant digits, no trailing zeros, and an
/// exponent when the number has more digits before the point.
fn go_g(value: f64, precision: usize) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let exponent = value.abs().log10().floor() as i32;
    let rounded: f64 = format!("{:.*e}", precision - 1, value)
        .parse()
        .unwrap_or(value);
    let exponent = if rounded.abs() >= 10f64.powi(exponent + 1) {
        exponent + 1
    } else {
        exponent
    };
    let trim = |s: String| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if exponent < -4 || exponent >= precision as i32 {
        let mantissa = trim(format!(
            "{:.*}",
            precision - 1,
            rounded / 10f64.powi(exponent)
        ));
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    } else {
        let decimals = (precision as i32 - 1 - exponent).max(0) as usize;
        trim(format!("{rounded:.decimals$}"))
    }
}

fn scaled(size: f64, base: f64, units: &[&str], precision: usize) -> String {
    let mut size = size;
    let mut i = 0;
    while size >= base && i < units.len() - 1 {
        size /= base;
        i += 1;
    }
    format!("{}{}", go_g(size, precision), units[i])
}

/// go-units `BytesSize`: binary units, 4 digits (`2.312MiB`).
pub fn binary_size(bytes: u64) -> String {
    const UNITS: [&str; 9] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"];
    scaled(bytes as f64, 1024.0, &UNITS, 4)
}

/// go-units `HumanSizeWithPrecision(_, 3)`: decimal units (`1.05kB`).
pub fn decimal_size(bytes: u64) -> String {
    const UNITS: [&str; 9] = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    scaled(bytes as f64, 1000.0, &UNITS, 3)
}

/// The table `docker stats` prints.
pub fn table(rows: &[Row]) -> String {
    let header = [
        "CONTAINER ID",
        "NAME",
        "CPU %",
        "MEM USAGE / LIMIT",
        "MEM %",
        "NET I/O",
        "BLOCK I/O",
        "PIDS",
    ];
    let cells: Vec<[String; 8]> = rows
        .iter()
        .map(|r| {
            [
                crate::id::short_id(&r.id).to_string(),
                r.name.clone(),
                format!("{:.2}%", r.cpu_percent),
                format!(
                    "{} / {}",
                    binary_size(r.memory),
                    binary_size(r.memory_limit)
                ),
                format!("{:.2}%", r.memory_percent),
                format!("{} / {}", decimal_size(r.net_rx), decimal_size(r.net_tx)),
                format!(
                    "{} / {}",
                    decimal_size(r.block_read),
                    decimal_size(r.block_write)
                ),
                r.pids.to_string(),
            ]
        })
        .collect();
    let mut widths = header.map(str::len);
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cols: Vec<&str>| {
        let last = cols.len() - 1;
        let mut out = String::new();
        for (i, c) in cols.into_iter().enumerate() {
            if i == last {
                out.push_str(c);
            } else {
                out.push_str(&format!("{c:<width$}   ", width = widths[i]));
            }
        }
        out.push('\n');
        out
    };
    let mut out = line(header.to_vec());
    for row in &cells {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::stats::{Cpu, Io};

    fn sample(read_ns: u128, cpu_usec: u64, system_ns: u64) -> Sample {
        Sample {
            read_ns,
            cgroup: Some(Usage {
                cpu: Some(Cpu {
                    usage_usec: cpu_usec,
                    user_usec: cpu_usec / 2,
                    system_usec: cpu_usec / 2,
                    ..Cpu::default()
                }),
                memory: Some(10 << 20),
                memory_stat: vec![("inactive_file".into(), 2 << 20), ("anon".into(), 7 << 20)],
                memory_max: Some(64 << 20),
                pids: Some(3),
                pids_max: Some(100),
                io: vec![Io {
                    major: 8,
                    minor: 0,
                    rbytes: 4096,
                    wbytes: 1_500_000,
                    ..Io::default()
                }],
                ..Usage::default()
            }),
            memory_limit: Some(64 << 20),
            system_cpu_ns: Some(system_ns),
            online_cpus: 4,
            networks: vec![NetDev {
                name: "eth0".into(),
                rx_bytes: 1050,
                tx_bytes: 0,
                ..NetDev::default()
            }],
            pids: Some(3),
            missing: None,
        }
    }

    #[test]
    fn docker_json_carries_what_the_cli_reads() {
        let first = sample(1_700_000_000_123_456_789, 1000, 1_000_000_000);
        let second = sample(1_700_000_001_000_000_000, 1500, 2_000_000_000);
        let v = second.docker_json(Some(&first), "abc", "web");
        assert_eq!(v["name"], "/web");
        assert_eq!(v["read"], "2023-11-14T22:13:21.000000000Z");
        assert_eq!(v["preread"], "2023-11-14T22:13:20.123456789Z");
        assert_eq!(v["cpu_stats"]["cpu_usage"]["total_usage"], 1_500_000);
        assert_eq!(v["precpu_stats"]["cpu_usage"]["total_usage"], 1_000_000);
        assert_eq!(v["cpu_stats"]["system_cpu_usage"], 2_000_000_000u64);
        assert_eq!(v["cpu_stats"]["online_cpus"], 4);
        assert_eq!(v["memory_stats"]["usage"], 10 << 20);
        assert_eq!(v["memory_stats"]["limit"], 64 << 20);
        assert_eq!(v["memory_stats"]["stats"]["inactive_file"], 2 << 20);
        assert_eq!(v["pids_stats"]["current"], 3);
        assert_eq!(v["pids_stats"]["limit"], 100);
        let io = &v["blkio_stats"]["io_service_bytes_recursive"];
        assert_eq!(io[0]["op"], "read");
        assert_eq!(io[1]["value"], 1_500_000);
        assert_eq!(v["networks"]["eth0"]["rx_bytes"], 1050);
        // Without a previous sample, and for a container that does not run.
        let alone = second.docker_json(None, "abc", "web");
        assert_eq!(alone["preread"], "0001-01-01T00:00:00Z");
        assert_eq!(alone["precpu_stats"]["cpu_usage"]["total_usage"], 0);
        let idle = Sample::default().docker_json(None, "abc", "web");
        assert_eq!(idle["read"], "0001-01-01T00:00:00Z");
        assert_eq!(idle["memory_stats"], json!({}));
        assert!(idle.get("networks").is_none());
    }

    #[test]
    fn rows_count_like_the_docker_cli() {
        let first = sample(1, 1000, 1_000_000_000);
        let second = sample(2, 51_000, 2_000_000_000);
        let row = Row::new("a".repeat(64).as_str(), "web", Some(&first), &second);
        // 50 ms of CPU in 1 s of all CPUs' time, on 4 CPUs.
        assert!((row.cpu_percent - 20.0).abs() < 1e-9, "{}", row.cpu_percent);
        assert_eq!(row.memory, 8 << 20);
        assert!((row.memory_percent - 12.5).abs() < 1e-9);
        assert_eq!(
            (row.net_rx, row.block_write, row.pids),
            (1050, 1_500_000, 3)
        );
        assert_eq!(Row::new("b", "x", None, &second).cpu_percent, 0.0);
        let idle = Row::new("c", "off", None, &Sample::default());
        assert_eq!((idle.memory, idle.memory_limit, idle.pids), (0, 0, 0));
    }

    #[test]
    fn sizes_like_go_units() {
        assert_eq!(binary_size(0), "0B");
        assert_eq!(binary_size(1023), "1023B");
        assert_eq!(binary_size(2_424_832), "2.312MiB");
        assert_eq!(binary_size(8 << 30), "8GiB");
        assert_eq!(binary_size(8_232_067_072), "7.667GiB");
        assert_eq!(decimal_size(0), "0B");
        assert_eq!(decimal_size(1050), "1.05kB");
        assert_eq!(decimal_size(999), "999B");
        assert_eq!(decimal_size(1_500_000), "1.5MB");
        // Go's quirk: three digits that round up to four.
        assert_eq!(decimal_size(999_600), "1e+03kB");
        assert_eq!(go_g(0.5, 3), "0.5");
        assert_eq!(go_g(123.456, 4), "123.5");
    }

    #[test]
    fn the_table_lines_up() {
        let row = Row {
            id: "0123456789abcdef".into(),
            name: "web".into(),
            cpu_percent: 1.5,
            memory: 2_424_832,
            memory_limit: 8 << 30,
            memory_percent: 0.03,
            net_rx: 1050,
            net_tx: 0,
            block_read: 0,
            block_write: 0,
            pids: 2,
        };
        let text = table(&[row]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "CONTAINER ID   NAME   CPU %   MEM USAGE / LIMIT   MEM %   NET I/O       BLOCK I/O   PIDS"
        );
        assert_eq!(
            lines[1],
            "0123456789ab   web    1.50%   2.312MiB / 8GiB     0.03%   1.05kB / 0B   0B / 0B     2"
        );
    }
}
