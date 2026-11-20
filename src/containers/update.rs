//! `update`: change a container's resource limits and restart policy. A running
//! container's cgroup gets the new values at once; the settings keep them for later
//! starts. Values and errors follow Docker's.

use super::ContainerHandle;
use crate::cgroup::{CPU_PERIOD, Limits, MIN_MEMORY, Placement};
use crate::restart::RestartPolicy;
use crate::state::{self, Status};
use crate::{Error, ErrorKind, Result, paths};

/// What `docker update` asks for, as Docker sends it: 0 (or empty) leaves a value as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Update {
    /// Bytes.
    pub memory: i64,
    /// Memory plus swap in bytes; -1: swap without limit.
    pub memory_swap: i64,
    pub memory_reservation: i64,
    /// CPUs in billionths.
    pub nano_cpus: i64,
    /// Microseconds per period; -1: no quota.
    pub cpu_quota: i64,
    /// Microseconds.
    pub cpu_period: i64,
    pub cpu_shares: i64,
    pub cpuset_cpus: String,
    pub cpuset_mems: String,
    /// Some(0 or less): no limit.
    pub pids_limit: Option<i64>,
    pub blkio_weight: i64,
    pub restart: Option<RestartPolicy>,
}

/// What the host has, for the checks.
#[derive(Debug, Clone)]
pub struct Host {
    pub cpus: usize,
    /// `0-3`: the CPUs a container may ask for.
    pub cpuset_cpus: String,
    /// `0`: the memory nodes.
    pub cpuset_mems: String,
}

impl Host {
    /// This machine.
    pub fn this() -> Host {
        let read = |paths: &[&str], fallback: &str| {
            paths
                .iter()
                .find_map(|p| {
                    std::fs::read_to_string(p)
                        .ok()
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                })
                .unwrap_or_else(|| fallback.to_string())
        };
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        Host {
            cpus,
            cpuset_cpus: read(
                &[
                    "/sys/fs/cgroup/cpuset.cpus.effective",
                    "/sys/devices/system/cpu/online",
                ],
                &format!("0-{}", cpus.saturating_sub(1)),
            ),
            cpuset_mems: read(
                &[
                    "/sys/fs/cgroup/cpuset.mems.effective",
                    "/sys/devices/system/node/online",
                ],
                "0",
            ),
        }
    }
}

/// A list like `0-2,5`: the numbers in it, or None when it is not one.
pub fn parse_list(text: &str) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for part in text.split(',') {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
        if a > b || b - a > 8192 {
            return None;
        }
        out.extend(a..=b);
    }
    Some(out)
}

fn check_set(
    asked: &str,
    available: &str,
    what: &str,
    missing: &str,
) -> std::result::Result<(), String> {
    let Some(want) = parse_list(asked) else {
        return Err(format!("Invalid value {asked} for {what}"));
    };
    let have = parse_list(available).unwrap_or_default();
    if want.iter().any(|n| !have.contains(n)) {
        return Err(format!(
            "Requested {missing} are not available - requested {asked}, available: {available}"
        ));
    }
    Ok(())
}

impl Update {
    /// Nothing to change.
    pub fn is_empty(&self) -> bool {
        *self == Update::default()
    }

    /// The checks Docker makes of the values alone. The error is Docker's message.
    pub fn check(&self, host: &Host) -> std::result::Result<(), String> {
        let min = MIN_MEMORY as i64;
        if self.memory != 0 && self.memory < min {
            return Err("Minimum memory limit allowed is 6MB".into());
        }
        if self.memory > 0 && self.memory_swap > 0 && self.memory_swap < self.memory {
            return Err(
                "Minimum memoryswap limit should be larger than memory limit, see usage".into(),
            );
        }
        if self.memory_swap < -1 {
            return Err(format!(
                "invalid memory swap limit {}: give -1 or a size",
                self.memory_swap
            ));
        }
        if self.memory_reservation != 0 && self.memory_reservation < min {
            return Err("Minimum memory reservation allowed is 6MB".into());
        }
        if self.memory > 0 && self.memory_reservation > 0 && self.memory < self.memory_reservation {
            return Err(
                "Minimum memory limit can not be less than memory reservation limit, see usage"
                    .into(),
            );
        }
        if self.nano_cpus > 0 && self.cpu_period > 0 {
            return Err("Conflicting options: Nano CPUs and CPU Period cannot both be set".into());
        }
        if self.nano_cpus > 0 && self.cpu_quota > 0 {
            return Err("Conflicting options: Nano CPUs and CPU Quota cannot both be set".into());
        }
        let n = host.cpus as i64;
        if self.nano_cpus < 0
            || self.nano_cpus > n * 1_000_000_000
            || (self.nano_cpus > 0 && self.nano_cpus < 10_000_000)
        {
            return Err(format!(
                "Range of CPUs is from 0.01 to {n}.00, as there are only {n} CPUs available"
            ));
        }
        if self.cpu_period != 0 && !(1000..=1_000_000).contains(&self.cpu_period) {
            return Err(
                "CPU cfs period can not be less than 1ms (i.e. 1000) or larger than 1s \
                 (i.e. 1000000)"
                    .into(),
            );
        }
        if self.cpu_quota > 0 && self.cpu_quota < 1000 {
            return Err("CPU cfs quota can not be less than 1ms (i.e. 1000)".into());
        }
        if self.cpu_quota < -1 {
            return Err(format!(
                "invalid CPU quota {}: give -1 or at least 1000",
                self.cpu_quota
            ));
        }
        if self.cpu_shares < 0 {
            return Err(format!(
                "invalid CPU shares ({}): value must be a positive integer",
                self.cpu_shares
            ));
        }
        if !self.cpuset_cpus.is_empty() {
            check_set(&self.cpuset_cpus, &host.cpuset_cpus, "cpuset cpus", "CPUs")?;
        }
        if !self.cpuset_mems.is_empty() {
            check_set(
                &self.cpuset_mems,
                &host.cpuset_mems,
                "cpuset mems",
                "memory nodes",
            )?;
        }
        if self.blkio_weight != 0 && !(10..=1000).contains(&self.blkio_weight) {
            return Err("Range of blkio weight is from 10 to 1000".into());
        }
        Ok(())
    }

    /// The limits and policy after this update of `limits` and `restart`. The error is
    /// Docker's message for a change that clashes with what is set.
    pub fn merge(
        &self,
        limits: &Limits,
        restart: RestartPolicy,
        auto_remove: bool,
    ) -> std::result::Result<(Limits, RestartPolicy), String> {
        let mut new = limits.clone();
        // NanoCpus and a quota of one's own exclude each other, as in Docker.
        let nano_set = limits.cpu_quota.is_some() && limits.cpu_period.is_none();
        let quota_set = limits.cpu_period.is_some();
        if self.nano_cpus > 0 && quota_set {
            return Err(
                "Conflicting options: NanoCPUs cannot be updated as CPU Period or \
                        CPU Quota has already been set"
                    .into(),
            );
        }
        if self.cpu_period > 0 && nano_set {
            return Err(
                "Conflicting options: CPU Period cannot be updated as NanoCPUs has already been set"
                    .into(),
            );
        }
        if self.cpu_quota > 0 && nano_set {
            return Err(
                "Conflicting options: CPU Quota cannot be updated as NanoCPUs has already been set"
                    .into(),
            );
        }
        if self.memory > 0 {
            let total = limits.memory_swap.filter(|t| *t > 0);
            if self.memory_swap == 0 && total.is_some_and(|t| self.memory > t) {
                return Err(
                    "Memory limit should be smaller than already set memoryswap limit, \
                            update the memoryswap at the same time"
                        .into(),
                );
            }
            new.memory = Some(self.memory as u64);
        }
        if self.memory_swap != 0 {
            if self.memory_swap > 0 {
                let Some(memory) = new.memory else {
                    return Err(
                        "You should always set the Memory limit when using Memoryswap limit, see usage"
                            .into(),
                    );
                };
                if (self.memory_swap as u64) < memory {
                    return Err(
                        "Minimum memoryswap limit should be larger than memory limit, see usage"
                            .into(),
                    );
                }
            }
            new.memory_swap = Some(self.memory_swap);
        }
        if self.memory_reservation > 0 {
            new.memory_reservation = Some(self.memory_reservation as u64);
        }
        if new
            .memory
            .zip(new.memory_reservation)
            .is_some_and(|(memory, reservation)| memory < reservation)
        {
            return Err(
                "Minimum memory limit can not be less than memory reservation limit, see usage"
                    .into(),
            );
        }
        if self.nano_cpus > 0 {
            new.cpu_quota =
                Some((self.nano_cpus as u64).saturating_mul(CPU_PERIOD) / 1_000_000_000);
            new.cpu_period = None;
        }
        if self.cpu_period > 0 {
            new.cpu_period = Some(self.cpu_period as u64);
        }
        match self.cpu_quota {
            q if q > 0 => {
                new.cpu_quota = Some(q as u64);
                new.cpu_period.get_or_insert(CPU_PERIOD);
            }
            -1 => {
                new.cpu_quota = None;
                new.cpu_period.get_or_insert(CPU_PERIOD);
            }
            _ => {}
        }
        if self.cpu_shares > 0 {
            new.cpu_shares = Some(self.cpu_shares as u64);
        }
        if !self.cpuset_cpus.is_empty() {
            new.cpuset_cpus = Some(self.cpuset_cpus.clone());
        }
        if !self.cpuset_mems.is_empty() {
            new.cpuset_mems = Some(self.cpuset_mems.clone());
        }
        if let Some(pids) = self.pids_limit {
            new.pids = crate::cgroup::pids_max(pids);
        }
        if self.blkio_weight > 0 {
            new.blkio_weight = u16::try_from(self.blkio_weight).ok();
        }
        let restart = match self.restart {
            Some(p) if p != RestartPolicy::No && auto_remove => {
                return Err(
                    "Restart policy cannot be updated because AutoRemove is enabled for the container"
                        .into(),
                );
            }
            Some(p) => p,
            None => restart,
        };
        Ok((new, restart))
    }
}

fn invalid(what: String, why: String) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        what,
        why,
        "see `podbag update --help`",
    )
}

impl ContainerHandle {
    /// Change the container's limits and restart policy (`docker update`): at once in a
    /// running container's cgroup, and in its settings for later starts. Returns warnings
    /// about what this host cannot apply.
    pub fn update(&self, update: &Update) -> Result<Vec<String>> {
        let dirs = paths::dirs()?;
        update
            .check(&Host::this())
            .map_err(|why| invalid("invalid update".into(), why))?;
        // Held while the settings are read, changed and written: a rename or another
        // update cannot get in between.
        let _names = state::lock_names(&dirs)?;
        let mut spec = state::read_config(&dirs, &self.id)?;
        let what = || format!("Cannot update container {}", self.id);
        let (limits, restart) = update
            .merge(&spec.limits, spec.restart, spec.auto_remove)
            .map_err(|why| invalid(what(), why))?;
        let mut warnings = Vec::new();
        if limits != spec.limits {
            if spec.cgroup == Placement::None {
                return Err(Error::new(
                    ErrorKind::MissingRequirement,
                    what(),
                    format!(
                        "it runs without a cgroup ({}), so it can have no resource limits",
                        super::pause::no_cgroup_reason(&dirs, &self.id)
                    ),
                    "create it again where cgroups work; see README \"Requirements\"",
                ));
            }
            let st = state::reconcile(&dirs, &self.id)?;
            if let (Status::Running, Some(group)) = (st.status, st.cgroup.as_deref()) {
                warnings = apply_live(group, &spec.limits, &limits)?;
            }
        }
        spec.limits = limits;
        spec.restart = restart;
        state::write_config(&dirs, &spec)?;
        Ok(warnings)
    }
}

#[cfg(target_os = "linux")]
fn apply_live(group: &str, old: &Limits, new: &Limits) -> Result<Vec<String>> {
    crate::cgroup::apply_live(std::path::Path::new(group), old, new)
}

#[cfg(not(target_os = "linux"))]
fn apply_live(_group: &str, _old: &Limits, _new: &Limits) -> Result<Vec<String>> {
    Err(Error::unsupported_os())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: i64 = 1 << 20;

    fn host() -> Host {
        Host {
            cpus: 4,
            cpuset_cpus: "0-3".into(),
            cpuset_mems: "0".into(),
        }
    }

    fn check(u: Update) -> std::result::Result<(), String> {
        u.check(&host())
    }

    #[test]
    fn values_are_checked_like_docker() {
        assert_eq!(check(Update::default()), Ok(()));
        let cases: Vec<(Update, &str)> = vec![
            (
                Update {
                    memory: MIB,
                    ..Update::default()
                },
                "Minimum memory limit allowed is 6MB",
            ),
            (
                Update {
                    memory: -5,
                    ..Update::default()
                },
                "Minimum memory limit allowed is 6MB",
            ),
            (
                Update {
                    memory: 64 * MIB,
                    memory_swap: 32 * MIB,
                    ..Update::default()
                },
                "Minimum memoryswap limit should be larger than memory limit",
            ),
            (
                Update {
                    memory_reservation: MIB,
                    ..Update::default()
                },
                "Minimum memory reservation allowed is 6MB",
            ),
            (
                Update {
                    memory: 32 * MIB,
                    memory_reservation: 64 * MIB,
                    ..Update::default()
                },
                "can not be less than memory reservation",
            ),
            (
                Update {
                    nano_cpus: 1_000_000_000,
                    cpu_period: 50_000,
                    ..Update::default()
                },
                "Nano CPUs and CPU Period cannot both be set",
            ),
            (
                Update {
                    nano_cpus: 1_000_000_000,
                    cpu_quota: 50_000,
                    ..Update::default()
                },
                "Nano CPUs and CPU Quota cannot both be set",
            ),
            (
                Update {
                    nano_cpus: 5_000_000_000,
                    ..Update::default()
                },
                "Range of CPUs is from 0.01 to 4.00, as there are only 4 CPUs available",
            ),
            (
                Update {
                    cpu_period: 500,
                    ..Update::default()
                },
                "CPU cfs period can not be less than 1ms",
            ),
            (
                Update {
                    cpu_quota: 999,
                    ..Update::default()
                },
                "CPU cfs quota can not be less than 1ms (i.e. 1000)",
            ),
            (
                Update {
                    cpu_shares: -2,
                    ..Update::default()
                },
                "invalid CPU shares (-2)",
            ),
            (
                Update {
                    cpuset_cpus: "1-x".into(),
                    ..Update::default()
                },
                "Invalid value 1-x for cpuset cpus",
            ),
            (
                Update {
                    cpuset_cpus: "2-5".into(),
                    ..Update::default()
                },
                "Requested CPUs are not available - requested 2-5, available: 0-3",
            ),
            (
                Update {
                    cpuset_mems: "1".into(),
                    ..Update::default()
                },
                "Requested memory nodes are not available",
            ),
            (
                Update {
                    blkio_weight: 5,
                    ..Update::default()
                },
                "Range of blkio weight is from 10 to 1000",
            ),
        ];
        for (u, want) in cases {
            let got = check(u.clone()).expect_err(&format!("{u:?} passed"));
            assert!(got.contains(want), "{u:?}: {got}");
        }
        let fine = Update {
            memory: 64 * MIB,
            memory_swap: -1,
            memory_reservation: 32 * MIB,
            cpu_period: 50_000,
            cpu_quota: 25_000,
            cpu_shares: 512,
            cpuset_cpus: "0,2-3".into(),
            cpuset_mems: "0".into(),
            pids_limit: Some(-1),
            blkio_weight: 300,
            ..Update::default()
        };
        assert_eq!(check(fine), Ok(()));
    }

    #[test]
    fn lists_of_cpus() {
        assert_eq!(parse_list("0-2,5"), Some(vec![0, 1, 2, 5]));
        assert_eq!(parse_list("3"), Some(vec![3]));
        for bad in ["", "a", "2-1", "1,", "-1", "0-100000"] {
            assert_eq!(parse_list(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_update_changes_only_what_it_names() {
        let old = Limits {
            memory: Some(64 << 20),
            pids: Some(10),
            ..Limits::default()
        };
        let (new, restart) = Update {
            memory: 128 * MIB,
            cpu_shares: 512,
            restart: Some(RestartPolicy::Always),
            ..Update::default()
        }
        .merge(&old, RestartPolicy::No, false)
        .unwrap();
        assert_eq!(new.memory, Some(128 << 20));
        assert_eq!(new.pids, Some(10));
        assert_eq!(new.cpu_shares, Some(512));
        assert_eq!(restart, RestartPolicy::Always);
        // No restart policy given: the old one stays. A pids limit of 0 or less: none.
        let (new, restart) = Update {
            pids_limit: Some(0),
            ..Update::default()
        }
        .merge(&old, RestartPolicy::OnFailure { max: 2 }, false)
        .unwrap();
        assert_eq!(new.pids, None);
        assert_eq!(restart, RestartPolicy::OnFailure { max: 2 });
        // CPUs: --cpus, then a quota of one's own.
        let (new, _) = Update {
            nano_cpus: 1_500_000_000,
            ..Update::default()
        }
        .merge(&Limits::default(), RestartPolicy::No, false)
        .unwrap();
        assert_eq!((new.cpu_quota, new.cpu_period), (Some(150_000), None));
        let (quota, _) = Update {
            cpu_quota: 20_000,
            cpu_period: 50_000,
            ..Update::default()
        }
        .merge(&Limits::default(), RestartPolicy::No, false)
        .unwrap();
        assert_eq!(
            (quota.cpu_quota, quota.cpu_period),
            (Some(20_000), Some(50_000))
        );
        let (unlimited, _) = Update {
            cpu_quota: -1,
            ..Update::default()
        }
        .merge(&quota, RestartPolicy::No, false)
        .unwrap();
        assert_eq!(
            (unlimited.cpu_quota, unlimited.cpu_period),
            (None, Some(50_000))
        );
    }

    #[test]
    fn clashes_with_what_is_set_are_refused() {
        let nano = Limits {
            cpu_quota: Some(50_000),
            ..Limits::default()
        };
        let quota = Update {
            cpu_quota: 20_000,
            ..Update::default()
        };
        let e = quota.merge(&nano, RestartPolicy::No, false).unwrap_err();
        assert!(e.contains("CPU Quota cannot be updated as NanoCPUs"), "{e}");
        let own_quota = Limits {
            cpu_quota: Some(20_000),
            cpu_period: Some(50_000),
            ..Limits::default()
        };
        let cpus = Update {
            nano_cpus: 1_000_000_000,
            ..Update::default()
        };
        assert!(cpus.merge(&own_quota, RestartPolicy::No, false).is_err());
        // Memory above the memory+swap that is set.
        let swap = Limits {
            memory: Some(64 << 20),
            memory_swap: Some(100 << 20),
            ..Limits::default()
        };
        let bigger = Update {
            memory: 128 * MIB,
            ..Update::default()
        };
        let e = bigger.merge(&swap, RestartPolicy::No, false).unwrap_err();
        assert!(e.contains("smaller than already set memoryswap"), "{e}");
        // Swap without memory; a reservation above the memory that is set.
        let swap_alone = Update {
            memory_swap: 100 * MIB,
            ..Update::default()
        };
        assert!(
            swap_alone
                .merge(&Limits::default(), RestartPolicy::No, false)
                .is_err()
        );
        let reservation = Update {
            memory_reservation: 100 * MIB,
            ..Update::default()
        };
        assert!(reservation.merge(&swap, RestartPolicy::No, false).is_err());
        // A restart policy with --rm.
        let always = Update {
            restart: Some(RestartPolicy::Always),
            ..Update::default()
        };
        let e = always
            .merge(&Limits::default(), RestartPolicy::No, true)
            .unwrap_err();
        assert!(e.contains("AutoRemove"), "{e}");
        let no = Update {
            restart: Some(RestartPolicy::No),
            ..Update::default()
        };
        assert!(
            no.merge(&Limits::default(), RestartPolicy::No, true)
                .is_ok()
        );
    }
}
