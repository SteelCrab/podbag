//! CPU and memory settings shared by all starts of a Mac home's Linux VM.

use super::settings;
use objc2_foundation::NSProcessInfo;
use objc2_virtualization::VZVirtualMachineConfiguration as Vz;
use serde::{Deserialize, Serialize};
use std::path::Path;

const MIB: u64 = 1 << 20;
const MEMORY_STEP: u64 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Resources {
    pub cpus: usize,
    pub memory_mib: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Limits {
    min_cpus: usize,
    max_cpus: usize,
    min_memory_mib: u64,
    max_memory_mib: u64,
    memory_step_mib: u64,
}

impl Limits {
    fn host() -> Result<Self, String> {
        let physical = NSProcessInfo::processInfo().physicalMemory();
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
        // SAFETY: these class methods only read the framework's supported range.
        let limits = unsafe {
            Self {
                min_cpus: Vz::minimumAllowedCPUCount().max(1),
                max_cpus: Vz::maximumAllowedCPUCount().min(cpus),
                min_memory_mib: Vz::minimumAllowedMemorySize()
                    .div_ceil(MIB * MEMORY_STEP)
                    .max(1)
                    * MEMORY_STEP,
                max_memory_mib: Vz::maximumAllowedMemorySize().min(physical) / (MIB * MEMORY_STEP)
                    * MEMORY_STEP,
                memory_step_mib: MEMORY_STEP,
            }
        };
        if limits.max_cpus < limits.min_cpus || limits.max_memory_mib < limits.min_memory_mib {
            return Err("this Mac does not support the Linux VM".into());
        }
        Ok(limits)
    }

    fn read(&self, home: &Path) -> Result<Resources, String> {
        let saved = settings::read(home)?;
        let mut values = Resources {
            cpus: saved.cpus.unwrap_or(settings::DEFAULT_CPUS_MAX) as usize,
            memory_mib: saved.memory_mib.unwrap_or(settings::DEFAULT_MEMORY_MIB),
        };
        // A home copied from a larger Mac can still start here.
        values.cpus = values.cpus.clamp(self.min_cpus, self.max_cpus);
        values.memory_mib = values
            .memory_mib
            .clamp(self.min_memory_mib, self.max_memory_mib);
        Ok(values)
    }

    fn save(&self, home: &Path, values: Resources) -> Result<(), String> {
        if !(self.min_cpus..=self.max_cpus).contains(&values.cpus) {
            return Err(format!(
                "choose between {} and {} CPUs",
                self.min_cpus, self.max_cpus
            ));
        }
        if !(self.min_memory_mib..=self.max_memory_mib).contains(&values.memory_mib)
            || !values.memory_mib.is_multiple_of(MEMORY_STEP)
        {
            return Err(format!(
                "choose {} to {} MiB in steps of {MEMORY_STEP} MiB",
                self.min_memory_mib, self.max_memory_mib
            ));
        }
        let mut saved = settings::read(home)?;
        saved.cpus = Some(values.cpus as u32);
        saved.memory_mib = Some(values.memory_mib);
        settings::save(home, &saved)
    }
}

fn env_value(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// Read or save the pair without connecting to the VM.
pub(super) fn command(home: &Path, args: &[String]) -> Result<(), String> {
    let limits = Limits::host()?;
    let values = match args {
        [] => limits.read(home)?,
        [cpus, memory] => {
            let values = Resources {
                cpus: cpus.parse().map_err(|_| "invalid CPU count")?,
                memory_mib: memory.parse().map_err(|_| "invalid memory in MiB")?,
            };
            limits.save(home, values)?;
            values
        }
        _ => return Err("usage: podbag vm resources [CPUS MEMORY_MIB]".into()),
    };
    println!(
        "{}",
        serde_json::json!({
            "values": values, "limits": limits,
            "envOverride": env_value("PODBAG_VM_CPUS").is_some() || env_value("PODBAG_VM_MEMORY").is_some()
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_commands_share_existing_resources_and_idle_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(
            home.join("vm-resources.json"),
            r#"{"cpus":2,"memoryMib":768}"#,
        )
        .unwrap();
        let mut saved = settings::read(home).unwrap();
        assert_eq!(saved.memory_mib, Some(768));
        saved.idle_seconds = Some(90);
        settings::save(home, &saved).unwrap();
        let limits = Limits {
            min_cpus: 1,
            max_cpus: 8,
            min_memory_mib: 256,
            max_memory_mib: 8192,
            memory_step_mib: MEMORY_STEP,
        };
        let updated = Resources {
            cpus: 3,
            memory_mib: 1280,
        };
        limits.save(home, updated).unwrap();
        let saved = settings::read(home).unwrap();
        assert_eq!(saved.idle_seconds, Some(90));
        assert_eq!(limits.read(home).unwrap(), updated);
        let (next, _) = settings::effective(
            &saved,
            &|_| None,
            &settings::Mac {
                cpus: 8,
                memory_mib: 16384,
            },
        );
        assert_eq!(
            (next.cpus, next.memory_mib, next.idle_seconds),
            (3, 1280, 90)
        );
    }

    #[test]
    fn defaults_saved_values_and_rejected_edits() {
        let mut limits = Limits {
            min_cpus: 1,
            max_cpus: 10,
            min_memory_mib: 256,
            max_memory_mib: 8192,
            memory_step_mib: MEMORY_STEP,
        };
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let values = Resources {
            cpus: 9,
            memory_mib: 3072,
        };
        assert_eq!(
            limits.read(&home).unwrap(),
            Resources {
                cpus: 8,
                memory_mib: 2048
            }
        );
        limits.save(&home, values).unwrap();
        for bad in [
            Resources { cpus: 0, ..values },
            Resources { cpus: 11, ..values },
            Resources {
                memory_mib: 0,
                ..values
            },
            Resources {
                memory_mib: 8193,
                ..values
            },
            Resources {
                memory_mib: 2049,
                ..values
            },
        ] {
            assert!(limits.save(&home, bad).is_err());
        }
        assert_eq!(limits.read(&home).unwrap(), values);
        limits.max_cpus = 4;
        limits.max_memory_mib = 2048;
        assert_eq!(
            limits.read(&home).unwrap(),
            Resources {
                cpus: 4,
                memory_mib: 2048
            }
        );
    }
}
