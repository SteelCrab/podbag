//! Resource limits (cgroup v2): value parsing, the memory budget, and why a container
//! ran out of memory. The parts here do not depend on the OS.

use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod group;
#[cfg(target_os = "linux")]
pub use group::{
    Group, ROOT, choose_placement, create, enable_controllers, own_cgroup, parent_name, remove_dir,
    slice_budget, systemd_run, try_remove_dir, user_manager,
};

/// Where a container's cgroup goes. The library decides; the shim creates it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// No cgroup: the container shares the shim's.
    #[default]
    None,
    /// Root mode: the shim creates `<parent>/<id>`.
    Direct { parent: String },
    /// Rootless: the shim runs in a systemd scope in `slice` and creates the group there.
    Scope { slice: String },
}

/// Rootless: the container's cgroup lives in a systemd scope, which needs `systemd-run`,
/// the user's service manager (`user_manager`) and cgroup v2. Without them the container
/// runs without a cgroup (with a warning), unless it has limits (an error).
pub fn rootless_placement(
    systemd_run: Option<std::path::PathBuf>,
    user_manager: bool,
    cgroup2: bool,
    limits: &Limits,
    parent: &str,
) -> Result<(Placement, Option<String>)> {
    let missing = if systemd_run.is_none() {
        "systemd-run was not found"
    } else if !user_manager {
        "there is no systemd user session ($XDG_RUNTIME_DIR/systemd/private)"
    } else if !cgroup2 {
        "cgroup v2 is not mounted at /sys/fs/cgroup"
    } else {
        return Ok((
            Placement::Scope {
                slice: format!("{parent}.slice"),
            },
            None,
        ));
    };
    unavailable(
        limits,
        &format!("rootless resource limits need a systemd user session: {missing}"),
    )
}

/// No cgroup: fine without limits (with a warning), an error with them.
pub(crate) fn unavailable(limits: &Limits, why: &str) -> Result<(Placement, Option<String>)> {
    if limits.is_empty() {
        return Ok((
            Placement::None,
            Some(format!(
                "{why}; the container runs without resource limits or the memory budget"
            )),
        ));
    }
    Err(Error::new(
        ErrorKind::MissingRequirement,
        "cannot apply the resource limits",
        why,
        "run without --memory, --cpus and --pids-limit, or see README \"Requirements\"",
    ))
}

/// The `cpu.max` period: quotas are microseconds of CPU time per 100 ms, like Docker.
pub const CPU_PERIOD: u64 = 100_000;

/// The smallest `--memory` Docker accepts.
pub const MIN_MEMORY: u64 = 6 << 20;

/// Limits of one container. `None` means no limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Bytes: `memory.max` (and `memory.swap.max`).
    pub memory: Option<u64>,
    /// Microseconds per [`CPU_PERIOD`]: `cpu.max`.
    pub cpu_quota: Option<u64>,
    /// Processes and threads: `pids.max`.
    pub pids: Option<u64>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.memory.is_none() && self.cpu_quota.is_none() && self.pids.is_none()
    }
}

/// A `--memory` value, read like Docker: a number with an optional `b`, `k`, `m`, `g` or
/// `t` (any case, optionally followed by `i` and `b`). Every unit is a power of 1024.
pub fn parse_memory(s: &str) -> Result<u64> {
    let bad = |why: &str| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid memory limit {s:?}"),
            why,
            "write it like 512m or 2g",
        )
    };
    let format = "a memory limit is a number with an optional unit: b, k, m, g or t";
    let text = s.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = (&text[..split], text[split..].trim_start());
    if number.is_empty() || number.matches('.').count() > 1 {
        return Err(bad(format));
    }
    let value: f64 = number.parse().map_err(|_| bad(format))?;
    let unit = unit.to_ascii_lowercase();
    let unit = unit
        .strip_suffix("ib")
        .or_else(|| unit.strip_suffix('b').filter(|u| !u.is_empty()))
        .unwrap_or(&unit);
    let shift = match unit {
        "" | "b" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        _ => return Err(bad(format)),
    };
    let bytes = (value * (1u64 << shift) as f64).round() as u64;
    if bytes < MIN_MEMORY {
        return Err(bad("Minimum memory limit allowed is 6MiB"));
    }
    Ok(bytes)
}

/// A `--cpus` value as a `cpu.max` quota.
pub fn cpus_to_quota(cpus: f64) -> Result<u64> {
    if !cpus.is_finite() || cpus < 0.01 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("invalid CPU limit {cpus}"),
            "the number of CPUs must be at least 0.01",
            "write it like 0.5 or 2",
        ));
    }
    Ok((cpus * CPU_PERIOD as f64).round() as u64)
}

/// The `cpu.max` line for a quota.
pub fn cpu_max(quota: u64) -> String {
    format!("{quota} {CPU_PERIOD}")
}

/// A `--pids-limit` value: zero or less means no limit, like Docker.
pub fn pids_max(limit: i64) -> Option<u64> {
    (limit > 0).then_some(limit as u64)
}

/// Why the kernel ended a container for memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OomReason {
    /// The container used more than its own `--memory`.
    ContainerLimit,
    /// All containers together used more than the memory budget.
    MemoryBudget,
}

impl std::fmt::Display for OomReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OomReason::ContainerLimit => "container limit",
            OomReason::MemoryBudget => "memory budget",
        })
    }
}

/// Read a container group's `memory.events`. `oom_kill` counts processes the OOM killer
/// ended in the group; `oom` counts only the group's own limit (events go up the tree,
/// never down), so a kill without it came from the budget above.
pub fn oom_reason(memory_events: &str) -> Option<OomReason> {
    let count = |key: &str| {
        memory_events
            .lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    if count("oom_kill") == 0 {
        return None;
    }
    Some(if count("oom") > 0 {
        OomReason::ContainerLimit
    } else {
        OomReason::MemoryBudget
    })
}

/// The `memory_budget` setting: how much memory all containers may use together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Unlimited,
    Bytes(u64),
    /// Percent of the host's memory, 1 to 100.
    Percent(u8),
}

impl Default for Budget {
    fn default() -> Self {
        Budget::Percent(50)
    }
}

impl Budget {
    /// The budget in bytes on a host with `mem_total` bytes; `None` means no limit.
    pub fn bytes(&self, mem_total: u64) -> Option<u64> {
        match *self {
            Budget::Unlimited => None,
            Budget::Bytes(b) => Some(b),
            Budget::Percent(p) => Some((mem_total as u128 * p as u128 / 100) as u64),
        }
    }
}

/// `"50%"`, `"0"` (no limit), or a size such as `"8GiB"`.
pub fn parse_budget(s: &str) -> Result<Budget> {
    let s = s.trim();
    if s == "0" {
        return Ok(Budget::Unlimited);
    }
    if let Some(p) = s.strip_suffix('%') {
        return match p.trim().parse::<u8>() {
            Ok(p @ 1..=100) => Ok(Budget::Percent(p)),
            _ => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("invalid memory budget {s:?}"),
                "a percentage must be from 1% to 100%",
                "write it like 50%, a size like 8GiB, or 0 for no limit",
            )),
        };
    }
    crate::config::parse_size(s).map(Budget::Bytes)
}

/// `memory.high` and `memory.max` for the parent of all containers: the kernel reclaims
/// above 90% of the budget and ends a container above the budget. Both are whole pages,
/// as the kernel stores them, so a value read back compares equal.
pub fn budget_values(budget: Option<u64>, page: u64) -> (String, String) {
    let page = page.max(1);
    let aligned = |b: u64| b / page * page;
    match budget {
        None => ("max".to_string(), "max".to_string()),
        Some(b) => (
            aligned((b as u128 * 9 / 10) as u64).to_string(),
            aligned(b).to_string(),
        ),
    }
}

/// The host's memory in bytes, from `/proc/meminfo`.
pub fn mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_memory_like_docker() {
        assert_eq!(parse_memory("32m").unwrap(), 32 << 20);
        assert_eq!(parse_memory("32MB").unwrap(), 32 << 20);
        assert_eq!(parse_memory("64MiB").unwrap(), 64 << 20);
        assert_eq!(parse_memory("1g").unwrap(), 1 << 30);
        assert_eq!(parse_memory("1.5g").unwrap(), 1_610_612_736);
        assert_eq!(parse_memory("8192k").unwrap(), 8 << 20);
        assert_eq!(parse_memory("67108864").unwrap(), 64 << 20);
        for bad in ["", "m", "-1m", "10x", "1.2.3m"] {
            assert_eq!(
                parse_memory(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
        let small = parse_memory("4m").unwrap_err();
        assert!(small.why.contains("6MiB"), "{}", small.why);
    }

    #[test]
    fn cpus_to_cpu_max() {
        assert_eq!(cpu_max(cpus_to_quota(0.5).unwrap()), "50000 100000");
        assert_eq!(cpu_max(cpus_to_quota(2.0).unwrap()), "200000 100000");
        assert_eq!(cpus_to_quota(0.01).unwrap(), 1000);
        for bad in [0.0, -1.0, 0.001, f64::NAN, f64::INFINITY] {
            assert_eq!(
                cpus_to_quota(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad}"
            );
        }
    }

    #[test]
    fn pids_limit_like_docker() {
        assert_eq!(pids_max(100), Some(100));
        assert_eq!(pids_max(0), None);
        assert_eq!(pids_max(-1), None);
    }

    #[test]
    fn budget_from_config() {
        let gib = 1u64 << 30;
        assert_eq!(parse_budget("50%").unwrap().bytes(4 * gib), Some(2 * gib));
        assert_eq!(parse_budget("0").unwrap().bytes(4 * gib), None);
        assert_eq!(parse_budget("2GiB").unwrap().bytes(4 * gib), Some(2 * gib));
        for bad in ["0%", "101%", "abc", "%"] {
            assert_eq!(
                parse_budget(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
        assert_eq!(
            budget_values(Some(64 << 20), 4096),
            ("60395520".to_string(), "67108864".to_string())
        );
        assert_eq!(
            budget_values(Some(64 << 20), 1),
            ("60397977".to_string(), "67108864".to_string())
        );
        assert_eq!(
            budget_values(None, 4096),
            ("max".to_string(), "max".to_string())
        );
    }

    #[test]
    fn oom_reason_from_events() {
        let own = "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\noom_group_kill 1\n";
        assert_eq!(oom_reason(own), Some(OomReason::ContainerLimit));
        assert_eq!(
            oom_reason("max 0\noom 0\noom_kill 2\n"),
            Some(OomReason::MemoryBudget)
        );
        assert_eq!(oom_reason("max 5\noom 0\noom_kill 0\n"), None);
        assert_eq!(oom_reason(""), None);
        assert_eq!(OomReason::ContainerLimit.to_string(), "container limit");
        assert_eq!(OomReason::MemoryBudget.to_string(), "memory budget");
    }

    #[test]
    fn rootless_placement_needs_systemd_for_limits() {
        use std::path::PathBuf;
        let some = Limits {
            memory: Some(64 << 20),
            ..Default::default()
        };
        let none = Limits::default();
        let run = Some(PathBuf::from("/usr/bin/systemd-run"));
        assert_eq!(
            rootless_placement(run.clone(), true, true, &some, "openpod").unwrap(),
            (
                Placement::Scope {
                    slice: "openpod.slice".into()
                },
                None
            )
        );
        let (p, warning) = rootless_placement(None, true, true, &none, "openpod").unwrap();
        assert_eq!(p, Placement::None);
        assert!(warning.unwrap().contains("systemd"));
        let e = rootless_placement(None, true, true, &some, "openpod").unwrap_err();
        assert_eq!(e.kind, ErrorKind::MissingRequirement);
        assert!(rootless_placement(run.clone(), false, true, &some, "openpod").is_err());
        assert!(rootless_placement(run, true, false, &some, "openpod").is_err());
    }

    #[test]
    fn mem_total_from_meminfo() {
        assert_eq!(
            mem_total("MemTotal:        3989504 kB\nMemFree: 1 kB\n"),
            Some(3989504 * 1024)
        );
        assert_eq!(mem_total("MemFree: 1 kB\n"), None);
    }
}
