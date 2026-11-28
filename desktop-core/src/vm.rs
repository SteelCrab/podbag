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
    /// macOS: the folders shared with the VM.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_paths: Option<SharedPaths>,
}

/// The macOS folders the VM shares: those of the running VM, and those of the next
/// start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SharedPaths {
    pub in_use: Option<Vec<String>>,
    pub next: Vec<String>,
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

/// A synchronized share of the macOS VM (`podbag vm sync list`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncShare {
    pub name: String,
    /// The Mac folder.
    pub source: String,
    /// The named volume that holds the guest's copy.
    pub volume: String,
    pub conflicts: Vec<SyncConflict>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConflict {
    pub path: String,
    pub reason: String,
}

/// A change to the synchronized shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    List,
    Add {
        name: String,
        path: String,
    },
    /// Synchronize now.
    Now(String),
    /// Stop synchronizing; the cache volume stays.
    Remove(String),
}

impl SyncAction {
    /// The action, the name and the path, as `podbag vm sync` takes them.
    pub fn args(&self) -> (&'static str, Option<&str>, Option<&str>) {
        match self {
            SyncAction::List => ("list", None, None),
            SyncAction::Add { name, path } => ("add", Some(name), Some(path)),
            SyncAction::Now(name) => ("now", Some(name), None),
            SyncAction::Remove(name) => ("remove", Some(name), None),
        }
    }
}

/// `podbag vm sync <action>`, then `podbag vm sync list`: the shares after it.
pub fn sync(podbag: &Path, action: &SyncAction) -> Result<Vec<SyncShare>, String> {
    use podbag::dashboard::cli;
    if *action != SyncAction::List {
        let (what, name, path) = action.args();
        cli::vm_sync(podbag, what, name, path)?;
    }
    serde_json::from_value(cli::vm_sync(podbag, "list", None, None)?)
        .map_err(|e| format!("podbag vm sync list: {e}"))
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
    fn the_shared_folders_of_a_mac_vm_are_read() {
        let json = serde_json::json!({"platform": "macos", "state": "stopped",
            "sharedPaths": {"next": ["/Users", "/Volumes/Projects"], "inUse": null}});
        let s = VmStatus::from_json(json.clone()).unwrap().unwrap();
        let shares = s.shared_paths.clone().unwrap();
        assert_eq!(shares.next, ["/Users", "/Volumes/Projects"]);
        assert_eq!(shares.in_use, None);
        assert_eq!(
            serde_json::to_value(&s).unwrap()["sharedPaths"],
            json["sharedPaths"]
        );
    }

    #[test]
    fn synchronized_shares_are_read_as_the_cli_prints_them() {
        let list: Vec<SyncShare> = serde_json::from_value(serde_json::json!([{
            "name": "project", "source": "/Users/me/projects/app",
            "volume": "podbag-sync-project", "conflicts": [{"path": "a.txt", "reason": "both changed"}],
            "lastError": null, "checked": 1764300000
        }]))
        .unwrap();
        assert_eq!(list[0].volume, "podbag-sync-project");
        assert_eq!(list[0].conflicts[0].reason, "both changed");
        assert_eq!(
            SyncAction::Add {
                name: "second".into(),
                path: "/Users/me/Second Project".into()
            }
            .args(),
            ("add", Some("second"), Some("/Users/me/Second Project"))
        );
        assert_eq!(SyncAction::Now("x".into()).args(), ("now", Some("x"), None));
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
