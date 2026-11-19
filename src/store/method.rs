//! How images are mounted: composefs when podbag runs as root
//! and the kernel passes the probe, hard-link trees otherwise.

use crate::config::ImageMount;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The cache of probe results, in the store directory.
const CACHE: &str = "mount-method.json";

/// A failed probe is tried again after this many seconds: the failure may have been
/// passing (a busy loop device, a full disk) or a bug a newer podbag fixed.
const RETRY_AFTER: u64 = 86400;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MountMethod {
    /// The image's EROFS metadata with the object store as overlayfs's data-only layer.
    Composefs,
    /// A directory of hard links to the objects as overlayfs's lower layer.
    Hardlink,
}

/// What the composefs probe found on one kernel release.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Support {
    pub kernel: String,
    pub composefs: bool,
    /// Why composefs cannot be used; empty when it can.
    #[serde(default)]
    pub reason: String,
    /// When the probe ran (seconds since the epoch).
    #[serde(default)]
    pub when: u64,
}

/// The cached probe result for `kernel`, or a new one: `probe` runs in a fresh directory
/// under `<store>/tmp`, and its result is cached for the next command. A success stays
/// for that kernel; a failure is tried again after a day.
pub fn support(
    store_root: &Path,
    kernel: &str,
    probe: impl FnOnce(&Path) -> Result<(), String>,
) -> Support {
    let cache = store_root.join(CACHE);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(cached) = std::fs::read(&cache)
        .ok()
        .and_then(|b| serde_json::from_slice::<Support>(&b).ok())
        .filter(|s| s.kernel == kernel && (s.composefs || now.saturating_sub(s.when) < RETRY_AFTER))
    {
        return cached;
    }
    // Unique per call: library callers may probe from two threads at once.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let scratch = store_root.join("tmp").join(format!(
        "probe.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = std::fs::create_dir_all(store_root.join("tmp"))
        .map_err(|e| format!("cannot create {}: {e}", store_root.join("tmp").display()))
        .and_then(|()| probe(&scratch));
    let found = Support {
        kernel: kernel.to_string(),
        composefs: result.is_ok(),
        reason: result.err().unwrap_or_default(),
        when: now,
    };
    // A cache that cannot be written only costs another probe next time.
    let tmp = store_root.join(format!("{CACHE}.{}.tmp", std::process::id()));
    if let Ok(bytes) = serde_json::to_vec(&found) {
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &cache).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    found
}

/// The method for this command. Rootless mode and `image_mount = "hardlink"` never probe.
pub fn choose(
    setting: ImageMount,
    rootless: bool,
    support: impl FnOnce() -> Support,
) -> MountMethod {
    if rootless || setting == ImageMount::Hardlink {
        return MountMethod::Hardlink;
    }
    if support().composefs {
        MountMethod::Composefs
    } else {
        MountMethod::Hardlink
    }
}

/// The running kernel's release (`7.0.0-43-generic`); empty where there is none to read.
pub fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ImageMount;
    use std::cell::Cell;
    use std::path::Path;

    fn yes(calls: &Cell<u32>) -> impl FnOnce(&Path) -> Result<(), String> + '_ {
        move |_| {
            calls.set(calls.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn support_is_probed_once_per_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        let first = support(tmp.path(), "7.0.0-43-generic", yes(&calls));
        let second = support(tmp.path(), "7.0.0-43-generic", yes(&calls));
        assert_eq!(calls.get(), 1);
        assert_eq!(first, second);
        assert!(first.composefs);
        assert_eq!(first.kernel, "7.0.0-43-generic");
        let cached = std::fs::read_to_string(tmp.path().join("mount-method.json")).unwrap();
        assert!(cached.contains("7.0.0-43-generic"), "{cached}");
    }

    #[test]
    fn the_probe_works_in_the_store_tmp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut seen = None;
        support(tmp.path(), "7.0", |scratch| {
            seen = Some(scratch.to_path_buf());
            Ok(())
        });
        let scratch = seen.unwrap();
        assert_eq!(scratch.parent(), Some(tmp.path().join("tmp").as_path()));
    }

    #[test]
    fn each_probe_gets_its_own_scratch_dir() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let mut seen = Vec::new();
        for tmp in [&a, &b] {
            support(tmp.path(), "7.0", |scratch| {
                seen.push(scratch.file_name().unwrap().to_owned());
                Ok(())
            });
        }
        assert_ne!(seen[0], seen[1]);
    }

    #[test]
    fn another_kernel_is_probed_again() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        support(tmp.path(), "6.8.0", yes(&calls));
        let now = support(tmp.path(), "7.0.0", yes(&calls));
        assert_eq!(calls.get(), 2);
        assert_eq!(now.kernel, "7.0.0");
    }

    #[test]
    fn a_damaged_cache_is_probed_again() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mount-method.json"), "garbage").unwrap();
        let calls = Cell::new(0);
        assert!(support(tmp.path(), "7.0.0", yes(&calls)).composefs);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn a_failed_probe_is_remembered_with_its_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let no = support(tmp.path(), "6.1.0", |_| Err("no EROFS support".into()));
        assert!(!no.composefs);
        assert_eq!(no.reason, "no EROFS support");
        let calls = Cell::new(0);
        assert_eq!(support(tmp.path(), "6.1.0", yes(&calls)), no);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn an_old_failure_is_probed_again() {
        let tmp = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let old = Support {
            kernel: "6.1.0".into(),
            composefs: false,
            reason: "loop device busy".into(),
            when: now - 2 * 86400,
        };
        std::fs::write(
            tmp.path().join("mount-method.json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let calls = Cell::new(0);
        assert!(support(tmp.path(), "6.1.0", yes(&calls)).composefs);
        assert_eq!(calls.get(), 1);
        // A success is kept for good.
        assert!(support(tmp.path(), "6.1.0", yes(&calls)).composefs);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn choose_never_probes_when_rootless_or_set_to_hardlink() {
        let never = || -> Support { panic!("probed") };
        assert_eq!(choose(ImageMount::Auto, true, never), MountMethod::Hardlink);
        assert_eq!(
            choose(ImageMount::Hardlink, false, never),
            MountMethod::Hardlink
        );
    }

    #[test]
    fn choose_follows_the_probe() {
        let answer = |composefs: bool| {
            move || Support {
                kernel: "7.0".into(),
                composefs,
                reason: String::new(),
                when: 0,
            }
        };
        assert_eq!(
            choose(ImageMount::Auto, false, answer(true)),
            MountMethod::Composefs
        );
        assert_eq!(
            choose(ImageMount::Auto, false, answer(false)),
            MountMethod::Hardlink
        );
    }
}
