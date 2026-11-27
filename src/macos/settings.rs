//! The VM's saved settings (`podbag vm set`): CPUs, memory and the idle power-off time,
//! in `vm.json` in the podbag home. `PODBAG_VM_CPUS`, `PODBAG_VM_MEMORY` and
//! `PODBAG_VM_IDLE` still win over the file. The host reads them when it starts a VM.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::vm::shares;

/// The least memory a VM gets.
pub const MEMORY_MIN_MIB: u64 = 1024;
/// What macOS keeps for itself: the VM may take the Mac's memory minus this.
pub const MAC_KEEPS_MIB: u64 = 4096;
/// The defaults: memory, idle power-off, and at most this many CPUs.
pub const DEFAULT_MEMORY_MIB: u64 = 2048;
pub const DEFAULT_IDLE_SECS: u64 = 60;
pub const DEFAULT_CPUS_MAX: u32 = 8;

/// What `vm.json` holds; a missing value takes the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Saved {
    pub cpus: Option<u32>,
    #[serde(rename = "memoryMiB", alias = "memoryMib")]
    pub memory_mib: Option<u64>,
    pub idle_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_paths: Option<Vec<PathBuf>>,
}

impl Saved {
    pub(super) fn shared_paths(&self) -> Vec<PathBuf> {
        self.shared_paths.clone().unwrap_or_else(shares::defaults)
    }
}

/// Resolve paths when saved so each host directory has one guest mount point.
pub(super) fn normalize_shares(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut shares = Vec::with_capacity(paths.len());
    for path in paths {
        let label = path.display();
        if !path.is_absolute() {
            return Err(format!("shared folder {label} must be an absolute path"));
        }
        let real = path
            .canonicalize()
            .map_err(|e| format!("cannot share {label}: {e}"))?;
        if !real.is_dir() {
            return Err(format!("cannot share {label}: it is not a directory"));
        }
        if real.to_str().is_none_or(|s| s.contains(['\n', '\r'])) || !shares::valid_target(&real) {
            return Err(format!(
                "cannot share {label}: {} is not a usable VM path",
                real.display()
            ));
        }
        if shares
            .iter()
            .any(|previous: &PathBuf| previous.starts_with(&real) || real.starts_with(previous))
        {
            return Err(format!(
                "cannot share {label}: shared folders must not overlap"
            ));
        }
        shares.push(real);
    }
    shares.sort_unstable();
    Ok(shares)
}

/// What a VM runs with. `idle_seconds` 0: it never powers off by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    pub cpus: u32,
    #[serde(rename = "memoryMiB")]
    pub memory_mib: u64,
    pub idle_seconds: u64,
}

/// The Mac's CPUs and memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mac {
    pub cpus: u32,
    pub memory_mib: u64,
}

impl Mac {
    pub fn this() -> Mac {
        Mac {
            cpus: std::thread::available_parallelism().map_or(2, |n| n.get() as u32),
            memory_mib: mac_memory() >> 20,
        }
    }

    /// The most memory a VM may get: the Mac's minus 4 GiB for macOS, and at least
    /// 1 GiB.
    pub fn memory_max_mib(&self) -> u64 {
        self.memory_mib
            .saturating_sub(MAC_KEEPS_MIB)
            .max(MEMORY_MIN_MIB)
    }
}

/// The Mac's memory in bytes (hw.memsize).
fn mac_memory() -> u64 {
    let mut size: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: hw.memsize is a 64-bit integer; the pointers name `size` and its length.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            std::ptr::from_mut(&mut size).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 { size } else { 8 << 30 }
}

/// A memory size: `4G`, `4GiB`, `4096M`, `4096MiB`, `1.5G`; a plain number is MiB.
pub fn parse_memory(text: &str) -> Result<u64, String> {
    let t = text.trim().to_ascii_lowercase();
    let bad = || format!("{text:?} is not a memory size (for example 4G or 4096M)");
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let n: f64 = number.parse().map_err(|_| bad())?;
    let factor = match unit.trim() {
        "" | "m" | "mb" | "mib" => 1.0,
        "g" | "gb" | "gib" => 1024.0,
        _ => return Err(bad()),
    };
    let mib = n * factor;
    if !mib.is_finite() || mib < 1.0 || mib.fract() != 0.0 {
        return Err(bad());
    }
    Ok(mib as u64)
}

/// An idle power-off time: `0` or `never` (never), `90` (seconds), `30s`, `5m`, `1h`.
pub fn parse_idle(text: &str) -> Result<u64, String> {
    let t = text.trim().to_ascii_lowercase();
    if t == "never" {
        return Ok(0);
    }
    let bad = || format!("{text:?} is not a time (for example 60s, 5m, 1h, or 0 for never)");
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let n: u64 = number.parse().map_err(|_| bad())?;
    let factor = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(bad()),
    };
    n.checked_mul(factor).ok_or_else(bad)
}

/// A time as `podbag vm set` takes it: `never`, `90s`, `5m`, `2h`.
pub fn idle_text(secs: u64) -> String {
    match secs {
        0 => "never".into(),
        s if s.is_multiple_of(3600) => format!("{}h", s / 3600),
        s if s.is_multiple_of(60) => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// A memory size for people: `2 GiB`, `1536 MiB`.
pub fn memory_text(mib: u64) -> String {
    if mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

/// Whether the values fit this Mac.
pub fn validate(s: &Saved, mac: &Mac) -> Result<(), String> {
    if let Some(n) = s.cpus
        && !(1..=mac.cpus).contains(&n)
    {
        return Err(format!(
            "--cpus must be from 1 to {} (this Mac's CPUs)",
            mac.cpus
        ));
    }
    if let Some(m) = s.memory_mib {
        let max = mac.memory_max_mib();
        if !(MEMORY_MIN_MIB..=max).contains(&m) {
            return Err(format!(
                "--memory must be from {} to {} (this Mac's {} minus {} for macOS)",
                memory_text(MEMORY_MIN_MIB),
                memory_text(max),
                memory_text(mac.memory_mib),
                memory_text(MAC_KEEPS_MIB)
            ));
        }
    }
    Ok(())
}

/// The environment variables that win over the file.
pub const VARIABLES: [&str; 3] = ["PODBAG_VM_CPUS", "PODBAG_VM_MEMORY", "PODBAG_VM_IDLE"];

/// What a VM started now would get: each variable that is set, else the file, else the
/// default. Also the variables that were used.
pub fn effective(
    s: &Saved,
    env: &dyn Fn(&str) -> Option<String>,
    mac: &Mac,
) -> (Resources, Vec<&'static str>) {
    let mut used = Vec::new();
    let mut number = |name: &'static str| -> Option<u64> {
        let v = env(name)?.trim().parse().ok()?;
        used.push(name);
        Some(v)
    };
    let cpus = match number("PODBAG_VM_CPUS").or(s.cpus.map(u64::from)) {
        Some(n) => (n as u32).clamp(1, mac.cpus.max(1)),
        None => mac.cpus.clamp(1, DEFAULT_CPUS_MAX),
    };
    let memory_mib = match number("PODBAG_VM_MEMORY") {
        Some(m) => m.max(1),
        None => s.memory_mib.map_or(DEFAULT_MEMORY_MIB, |m| {
            if m > mac.memory_mib {
                mac.memory_max_mib()
            } else {
                m.max(1)
            }
        }),
    };
    let idle_seconds = number("PODBAG_VM_IDLE")
        .or(s.idle_seconds)
        .unwrap_or(DEFAULT_IDLE_SECS);
    (
        Resources {
            cpus,
            memory_mib,
            idle_seconds,
        },
        used,
    )
}

/// The saved settings; none when there is no file or it cannot be read.
pub fn load(home: &Path) -> Saved {
    read(home).unwrap_or_default()
}

pub(super) fn read(home: &Path) -> Result<Saved, String> {
    for file in ["vm.json", "vm-resources.json"] {
        let path = home.join(file);
        match std::fs::read(&path) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes)
                    .map_err(|e| format!("{}: {e}", path.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok(Saved::default())
}

/// Save the settings (written beside the file, then renamed over it).
pub fn save(home: &Path, s: &Saved) -> Result<(), String> {
    let file = home.join("vm.json");
    std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    let tmp = home.join(format!(".vm.json.{}", std::process::id()));
    std::fs::write(&tmp, text + "\n").map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", file.display())
    })
}

/// What the running VM's host started with, and what it saw last (`run/vm.json`, written
/// by the host): for `podbag vm status`, which must not connect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Running {
    /// Unix seconds.
    pub started: u64,
    pub resources: Option<Resources>,
    pub shared_paths: Option<Vec<PathBuf>>,
    /// Running containers, as the host last counted them.
    pub containers: Option<u64>,
    /// The guest's CPU use (percent of all its CPUs) and memory, as last read.
    pub cpu_percent: Option<f64>,
    #[serde(rename = "memoryUsedMiB")]
    pub memory_used_mib: Option<u64>,
    #[serde(rename = "memoryTotalMiB")]
    pub memory_total_mib: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Mac = Mac {
        cpus: 10,
        memory_mib: 24 * 1024,
    };

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn shares_resolve_links_and_reject_overlapping_or_system_paths() {
        let dir = tempfile::tempdir_in("/private/tmp").unwrap();
        let root = dir.path().canonicalize().unwrap();
        let folder = root.join("my projects");
        let child = folder.join("app");
        std::fs::create_dir_all(&child).unwrap();
        let link = root.join("alias");
        std::os::unix::fs::symlink(&folder, &link).unwrap();

        assert_eq!(normalize_shares(&[link.clone()]).unwrap(), [folder.clone()]);
        assert!(normalize_shares(&[link.clone(), folder.clone()]).is_err());
        assert!(normalize_shares(&[child, folder]).is_err());
        for path in ["relative", "/", "/dev", "/tmp/does-not-exist"] {
            assert!(normalize_shares(&[PathBuf::from(path)]).is_err(), "{path}");
        }
        assert!(normalize_shares(&[]).unwrap().is_empty());
        assert_eq!(Saved::default().shared_paths(), [PathBuf::from("/Users")]);
    }

    #[test]
    fn memory_sizes() {
        assert_eq!(parse_memory("4G").unwrap(), 4096);
        assert_eq!(parse_memory("4gib").unwrap(), 4096);
        assert_eq!(parse_memory("1.5G").unwrap(), 1536);
        assert_eq!(parse_memory("3072M").unwrap(), 3072);
        assert_eq!(parse_memory("3072 MiB").unwrap(), 3072);
        assert_eq!(parse_memory("2048").unwrap(), 2048);
        for bad in ["", "G", "4T", "-1G", "0", "abc", "1.2.3G", "0.3M"] {
            assert!(parse_memory(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn idle_times() {
        assert_eq!(parse_idle("0").unwrap(), 0);
        assert_eq!(parse_idle("never").unwrap(), 0);
        assert_eq!(parse_idle("90").unwrap(), 90);
        assert_eq!(parse_idle("30s").unwrap(), 30);
        assert_eq!(parse_idle("5m").unwrap(), 300);
        assert_eq!(parse_idle("2h").unwrap(), 7200);
        for bad in ["", "m", "5d", "-5", "1.5m", "5 m"] {
            assert!(parse_idle(bad).is_err(), "{bad:?}");
        }
        assert_eq!(idle_text(0), "never");
        assert_eq!(idle_text(300), "5m");
        assert_eq!(idle_text(7200), "2h");
        assert_eq!(idle_text(45), "45s");
    }

    #[test]
    fn values_must_fit_the_mac() {
        let ok = Saved {
            cpus: Some(10),
            memory_mib: Some(20 * 1024),
            idle_seconds: Some(0),
            shared_paths: None,
        };
        assert!(validate(&ok, &MAC).is_ok());
        let e = validate(
            &Saved {
                cpus: Some(0),
                ..Saved::default()
            },
            &MAC,
        )
        .unwrap_err();
        assert!(e.contains("1 to 10"), "{e}");
        assert!(
            validate(
                &Saved {
                    cpus: Some(11),
                    ..Saved::default()
                },
                &MAC
            )
            .is_err()
        );
        assert!(
            validate(
                &Saved {
                    memory_mib: Some(1023),
                    ..Saved::default()
                },
                &MAC
            )
            .is_err()
        );
        let e = validate(
            &Saved {
                memory_mib: Some(20 * 1024 + 1),
                ..Saved::default()
            },
            &MAC,
        )
        .unwrap_err();
        assert!(e.contains("20 GiB"), "{e}");
        // A small Mac still allows 1 GiB.
        let small = Mac {
            cpus: 4,
            memory_mib: 4096,
        };
        assert_eq!(small.memory_max_mib(), 1024);
    }

    #[test]
    fn variables_win_over_the_file_and_the_file_over_the_defaults() {
        let (r, used) = effective(&Saved::default(), &no_env, &MAC);
        assert_eq!(
            r,
            Resources {
                cpus: 8,
                memory_mib: 2048,
                idle_seconds: 60
            }
        );
        assert!(used.is_empty());
        let saved = Saved {
            cpus: Some(2),
            memory_mib: Some(4096),
            idle_seconds: Some(0),
            shared_paths: None,
        };
        let (r, _) = effective(&saved, &no_env, &MAC);
        assert_eq!((r.cpus, r.memory_mib, r.idle_seconds), (2, 4096, 0));
        let env = |k: &str| (k == "PODBAG_VM_MEMORY").then(|| "1024".to_string());
        let (r, used) = effective(&saved, &env, &MAC);
        assert_eq!((r.cpus, r.memory_mib), (2, 1024));
        assert_eq!(used, ["PODBAG_VM_MEMORY"]);
        // A file from a bigger Mac is cut to fit.
        let big = Saved {
            cpus: Some(64),
            memory_mib: Some(1 << 20),
            idle_seconds: None,
            shared_paths: None,
        };
        let (r, _) = effective(&big, &no_env, &MAC);
        assert_eq!((r.cpus, r.memory_mib), (10, 20 * 1024));
    }

    #[test]
    fn saved_and_loaded() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Saved::default());
        let s = Saved {
            cpus: Some(4),
            memory_mib: None,
            idle_seconds: Some(300),
            shared_paths: None,
        };
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()), s);
        let text = std::fs::read_to_string(dir.path().join("vm.json")).unwrap();
        assert!(text.contains("\"idleSeconds\": 300"), "{text}");
        std::fs::write(dir.path().join("vm.json"), "{").unwrap();
        assert_eq!(load(dir.path()), Saved::default());
    }
}
