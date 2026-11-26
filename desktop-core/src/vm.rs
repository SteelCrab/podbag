//! The VM of podbag on macOS and Windows: its state (`podbag vm status --json`), its
//! settings and its resources.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

pub use podbag::dashboard::cli::VmSettings;

/// CPUs, memory and the idle time of a VM. What is None is not set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VmValues {
    pub cpus: Option<u32>,
    #[serde(rename = "memoryMiB")]
    pub memory_mib: Option<u64>,
    pub idle_seconds: Option<u64>,
}

/// What this computer allows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VmLimits {
    pub cpus_min: u32,
    pub cpus_max: u32,
    #[serde(rename = "memoryMinMiB")]
    pub memory_min_mib: u64,
    #[serde(rename = "memoryMaxMiB")]
    pub memory_max_mib: u64,
    #[serde(rename = "macMemoryMiB")]
    pub mac_memory_mib: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VmDisk {
    pub path: String,
    pub allocated_bytes: u64,
    pub size_bytes: u64,
}

/// `podbag vm status --json`. Windows gives only `platform`, `state` and
/// `resourcesEditable`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VmStatus {
    /// `macos` or `windows`.
    pub platform: String,
    /// `running`, `standby`, `starting`, `stopped` or `not set up`.
    pub state: String,
    pub resources_editable: bool,
    /// What the running VM uses.
    pub in_use: Option<VmValues>,
    /// What the next start uses.
    pub next: Option<VmValues>,
    /// What the user saved.
    pub saved: Option<VmValues>,
    /// The environment variables that win over the saved values.
    pub overridden_by: Vec<String>,
    /// The next start uses other values than the running VM.
    pub pending: bool,
    pub limits: Option<VmLimits>,
    pub uptime_seconds: Option<u64>,
    pub containers: Option<u64>,
    pub cpu_percent: Option<f64>,
    #[serde(rename = "memoryUsedMiB")]
    pub memory_used_mib: Option<u64>,
    #[serde(rename = "memoryTotalMiB")]
    pub memory_total_mib: Option<u64>,
    pub disk: Option<VmDisk>,
}

impl VmStatus {
    /// From the JSON of `podbag vm status --json`; None for `null`.
    pub fn from_json(v: Value) -> Result<Option<VmStatus>, String> {
        if v.is_null() {
            return Ok(None);
        }
        serde_json::from_value(v)
            .map(Some)
            .map_err(|e| format!("podbag vm status --json: {e}"))
    }
}

/// New resources for `podbag vm resources`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmResources {
    pub cpus: usize,
    pub memory_mib: u64,
}

/// `podbag vm resources [CPUS MEMORY]`: what it printed, as JSON.
pub fn resources(podbag: &Path, values: Option<VmResources>) -> Result<Value, String> {
    let mut command = std::process::Command::new(podbag);
    command.args(["vm", "resources"]);
    if let Some(values) = values {
        command.args([values.cpus.to_string(), values.memory_mib.to_string()]);
    }
    let out = command
        .output()
        .map_err(|e| format!("{}: {e}", podbag.display()))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_of_a_mac_vm_is_read() {
        let json = serde_json::json!({
            "platform": "macos", "state": "running", "resourcesEditable": true,
            "inUse": { "cpus": 4, "memoryMiB": 4096, "idleSeconds": 300 },
            "next": { "cpus": 4, "memoryMiB": 6144, "idleSeconds": 300 },
            "saved": { "cpus": null, "memoryMiB": 6144, "idleSeconds": null },
            "overriddenBy": ["PODBAG_VM_CPUS"], "pending": true,
            "limits": { "cpusMin": 1, "cpusMax": 10, "memoryMinMiB": 1024,
                        "memoryMaxMiB": 20480, "macMemoryMiB": 24576 },
            "uptimeSeconds": 7560, "containers": 3, "cpuPercent": 12.5,
            "memoryUsedMiB": 1350, "memoryTotalMiB": 3900,
            "disk": { "path": "/x/disk.img", "allocatedBytes": 5, "sizeBytes": 6 },
        });
        let s = VmStatus::from_json(json.clone()).unwrap().unwrap();
        assert_eq!(s.state, "running");
        assert_eq!(s.next.unwrap().memory_mib, Some(6144));
        assert_eq!(s.saved.unwrap().cpus, None);
        assert_eq!(s.limits.unwrap().mac_memory_mib, 24576);
        assert_eq!(s.memory_used_mib, Some(1350));
        assert_eq!(s.disk.as_ref().unwrap().size_bytes, 6);
        assert_eq!(serde_json::to_value(&s).unwrap(), json);
    }

    #[test]
    fn the_status_of_a_windows_vm_has_the_state_only() {
        let json = serde_json::json!({"platform": "windows", "state": "not set up",
                                      "resourcesEditable": false});
        let s = VmStatus::from_json(json).unwrap().unwrap();
        assert_eq!(
            (s.platform.as_str(), s.state.as_str()),
            ("windows", "not set up")
        );
        assert!(!s.resources_editable && s.next.is_none() && s.limits.is_none());
        assert_eq!(VmStatus::from_json(Value::Null), Ok(None));
    }
}
