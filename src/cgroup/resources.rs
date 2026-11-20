//! Limits as cgroup v2 files, with runc's conversions of Docker's values. The parts here
//! do not depend on the OS.

use super::{CPU_PERIOD, Limits};

/// One value to write in a container's cgroup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// The controller the file belongs to.
    pub controller: &'static str,
    /// The option that asked for it, for errors.
    pub option: &'static str,
    pub file: &'static str,
    pub value: String,
    /// Written instead when this file exists (`io.bfq.weight` before `io.weight`).
    pub instead: Option<(&'static str, String)>,
    /// A missing file is no error (no swap accounting; no I/O weight).
    pub optional: bool,
}

impl Setting {
    fn new(
        controller: &'static str,
        option: &'static str,
        file: &'static str,
        value: String,
    ) -> Self {
        Setting {
            controller,
            option,
            file,
            value,
            instead: None,
            optional: false,
        }
    }
}

/// Docker's CPU shares (2 to 262144, default 1024) as `cpu.weight` (1 to 10000, default
/// 100), as runc converts them: a curve through the three points, so the default stays
/// the default.
pub fn cpu_weight(shares: u64) -> u64 {
    match shares {
        0 => 0,
        1..=2 => 1,
        262_144.. => 10_000,
        _ => {
            let l = (shares as f64).log2();
            let exponent = (l * l + 125.0 * l) / 612.0 - 7.0 / 34.0;
            10f64.powf(exponent).ceil() as u64
        }
    }
}

/// Docker's block I/O weight (10 to 1000) as `io.weight` (1 to 10000), as runc converts it.
pub fn io_weight(blkio_weight: u16) -> u64 {
    let w = u64::from(blkio_weight);
    if w == 0 {
        return 0;
    }
    1 + (w.saturating_sub(10)) * 9999 / 990
}

/// `memory.swap.max` for Docker's memory+swap value: `max` for -1, the difference for a
/// total, and as much swap as memory when only memory is set.
pub fn swap_max(memory: Option<u64>, memory_swap: Option<i64>) -> Option<String> {
    match (memory, memory_swap) {
        (_, Some(-1)) => Some("max".to_string()),
        (Some(m), Some(total)) if total > 0 => Some((total as u64).saturating_sub(m).to_string()),
        (Some(m), _) => Some(m.to_string()),
        (None, _) => None,
    }
}

/// The `cpu.max` line: quota and period, `max` for no quota.
pub fn cpu_max_line(quota: Option<u64>, period: Option<u64>) -> Option<String> {
    let period = period.unwrap_or(CPU_PERIOD);
    match (quota, period == CPU_PERIOD) {
        (Some(q), _) => Some(format!("{q} {period}")),
        (None, false) => Some(format!("max {period}")),
        (None, true) => None,
    }
}

/// Every file the limits ask for, in the order to write them.
pub fn settings(limits: &Limits) -> Vec<Setting> {
    let mut out = Vec::new();
    if let Some(bytes) = limits.memory {
        out.push(Setting::new(
            "memory",
            "--memory",
            "memory.max",
            bytes.to_string(),
        ));
    }
    if let Some(swap) = swap_max(limits.memory, limits.memory_swap) {
        out.push(Setting {
            optional: true,
            ..Setting::new("memory", "--memory-swap", "memory.swap.max", swap)
        });
    }
    if let Some(bytes) = limits.memory_reservation {
        out.push(Setting::new(
            "memory",
            "--memory-reservation",
            "memory.low",
            bytes.to_string(),
        ));
    }
    if let Some(line) = cpu_max_line(limits.cpu_quota, limits.cpu_period) {
        out.push(Setting::new("cpu", "--cpus", "cpu.max", line));
    }
    if let Some(shares) = limits.cpu_shares.filter(|s| *s > 0) {
        out.push(Setting::new(
            "cpu",
            "--cpu-shares",
            "cpu.weight",
            cpu_weight(shares).to_string(),
        ));
    }
    if let Some(cpus) = &limits.cpuset_cpus {
        out.push(Setting::new(
            "cpuset",
            "--cpuset-cpus",
            "cpuset.cpus",
            cpus.clone(),
        ));
    }
    if let Some(mems) = &limits.cpuset_mems {
        out.push(Setting::new(
            "cpuset",
            "--cpuset-mems",
            "cpuset.mems",
            mems.clone(),
        ));
    }
    if let Some(pids) = limits.pids {
        out.push(Setting::new(
            "pids",
            "--pids-limit",
            "pids.max",
            pids.to_string(),
        ));
    }
    if let Some(weight) = limits.blkio_weight.filter(|w| *w > 0) {
        out.push(Setting {
            instead: Some(("io.bfq.weight", weight.to_string())),
            optional: true,
            ..Setting::new(
                "io",
                "--blkio-weight",
                "io.weight",
                io_weight(weight).to_string(),
            )
        });
    }
    out
}

/// What a running container's cgroup needs when its limits change from `old` to `new`:
/// the new values, and the defaults back for limits that went away.
pub fn changes(old: &Limits, new: &Limits) -> Vec<Setting> {
    let mut out = settings(new);
    if old.pids.is_some() && new.pids.is_none() {
        out.push(Setting::new(
            "pids",
            "--pids-limit",
            "pids.max",
            "max".to_string(),
        ));
    }
    let had_quota = cpu_max_line(old.cpu_quota, old.cpu_period).is_some();
    if had_quota && cpu_max_line(new.cpu_quota, new.cpu_period).is_none() {
        out.push(Setting::new(
            "cpu",
            "--cpus",
            "cpu.max",
            format!("max {CPU_PERIOD}"),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_shares_become_cpu_weight_like_runc() {
        assert_eq!(cpu_weight(0), 0);
        assert_eq!(cpu_weight(1), 1);
        assert_eq!(cpu_weight(2), 1);
        assert_eq!(cpu_weight(1024), 100);
        assert_eq!(cpu_weight(262_144), 10_000);
        assert_eq!(cpu_weight(1 << 20), 10_000);
        // Between the points the curve rises.
        assert_eq!(cpu_weight(512), 59);
        assert_eq!(cpu_weight(2048), 174);
        assert!(cpu_weight(100) < cpu_weight(200));
    }

    #[test]
    fn blkio_weight_becomes_io_weight_like_runc() {
        assert_eq!(io_weight(0), 0);
        assert_eq!(io_weight(10), 1);
        assert_eq!(io_weight(500), 4950);
        assert_eq!(io_weight(1000), 10_000);
    }

    #[test]
    fn swap_follows_docker() {
        let mib = 1u64 << 20;
        assert_eq!(swap_max(Some(64 * mib), None), Some((64 * mib).to_string()));
        assert_eq!(
            swap_max(Some(64 * mib), Some(100 * mib as i64)),
            Some((36 * mib).to_string())
        );
        assert_eq!(
            swap_max(Some(64 * mib), Some(64 * mib as i64)),
            Some("0".into())
        );
        assert_eq!(swap_max(Some(64 * mib), Some(-1)), Some("max".into()));
        assert_eq!(swap_max(None, Some(-1)), Some("max".into()));
        assert_eq!(swap_max(None, None), None);
    }

    #[test]
    fn limits_as_files() {
        let limits = Limits {
            memory: Some(64 << 20),
            cpu_quota: Some(50_000),
            pids: Some(10),
            memory_reservation: Some(32 << 20),
            cpu_shares: Some(1024),
            cpuset_cpus: Some("0-1".into()),
            blkio_weight: Some(500),
            ..Limits::default()
        };
        let files: Vec<(&str, String)> = settings(&limits)
            .into_iter()
            .map(|s| (s.file, s.value))
            .collect();
        assert_eq!(
            files,
            [
                ("memory.max", (64u64 << 20).to_string()),
                ("memory.swap.max", (64u64 << 20).to_string()),
                ("memory.low", (32u64 << 20).to_string()),
                ("cpu.max", "50000 100000".to_string()),
                ("cpu.weight", "100".to_string()),
                ("cpuset.cpus", "0-1".to_string()),
                ("pids.max", "10".to_string()),
                ("io.weight", "4950".to_string()),
            ]
        );
        let io = settings(&limits).pop().unwrap();
        assert_eq!(io.instead, Some(("io.bfq.weight", "500".to_string())));
        assert!(io.optional);
        assert!(settings(&Limits::default()).is_empty());
        // A period of its own, with and without a quota.
        let period = Limits {
            cpu_period: Some(50_000),
            ..Limits::default()
        };
        assert_eq!(settings(&period)[0].value, "max 50000");
        let both = Limits {
            cpu_quota: Some(25_000),
            ..period
        };
        assert_eq!(settings(&both)[0].value, "25000 50000");
    }

    #[test]
    fn removed_limits_go_back_to_the_defaults() {
        let old = Limits {
            pids: Some(10),
            cpu_quota: Some(50_000),
            ..Limits::default()
        };
        let files: Vec<(&str, String)> = changes(&old, &Limits::default())
            .into_iter()
            .map(|s| (s.file, s.value))
            .collect();
        assert_eq!(
            files,
            [
                ("pids.max", "max".to_string()),
                ("cpu.max", "max 100000".to_string())
            ]
        );
        assert_eq!(changes(&old, &old), settings(&old));
    }
}
