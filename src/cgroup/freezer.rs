//! The cgroup v2 freezer: `pause` and `unpause` stop and continue every process of a
//! container's group at once (`cgroup.freeze`); `cgroup.events` says when it is done.

use std::path::Path;
use std::time::{Duration, Instant};

/// How long freezing may take before it is given up (runc waits about as long).
pub const LIMIT: Duration = Duration::from_secs(10);

/// Whether the group at `dir` is frozen, from the `frozen` line of `cgroup.events`.
/// None: the file or the line is missing (no freezer, or no group).
pub fn frozen(dir: &Path) -> Option<bool> {
    let events = std::fs::read_to_string(dir.join("cgroup.events")).ok()?;
    frozen_in(&events)
}

fn frozen_in(events: &str) -> Option<bool> {
    events
        .lines()
        .find_map(|l| l.strip_prefix("frozen "))
        .map(|v| v.trim() == "1")
}

/// Freeze (`true`) or thaw the group, and wait until the kernel says it is done. A
/// freeze that does not finish within `limit` is undone.
pub fn set(dir: &Path, freeze: bool, limit: Duration) -> std::io::Result<()> {
    let file = dir.join("cgroup.freeze");
    if !file.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "{} is missing: the kernel has no cgroup v2 freezer (Linux 5.2 or later)",
                file.display()
            ),
        ));
    }
    std::fs::write(&file, if freeze { "1" } else { "0" })?;
    let deadline = Instant::now() + limit;
    loop {
        match frozen(dir) {
            Some(now) if now == freeze => return Ok(()),
            None => return Err(std::io::Error::other("cannot read cgroup.events")),
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            if freeze {
                let _ = std::fs::write(&file, "0");
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "the processes did not {} within {} seconds",
                    if freeze { "stop" } else { "continue" },
                    limit.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frozen_line_of_cgroup_events() {
        assert_eq!(frozen_in("populated 1\nfrozen 1\n"), Some(true));
        assert_eq!(frozen_in("populated 1\nfrozen 0\n"), Some(false));
        assert_eq!(frozen_in("populated 1\n"), None);
    }

    #[test]
    fn freezing_waits_for_the_kernel_and_gives_up_in_time() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // No freezer at all.
        let e = set(dir, true, Duration::from_millis(50)).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
        // The kernel has frozen it already.
        std::fs::write(dir.join("cgroup.freeze"), "0").unwrap();
        std::fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        set(dir, true, Duration::from_millis(50)).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("cgroup.freeze")).unwrap(),
            "1"
        );
        assert_eq!(frozen(dir), Some(true));
        // It never thaws: the wait ends.
        let e = set(dir, false, Duration::from_millis(50)).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        // A freeze that never finishes is undone.
        std::fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        let e = set(dir, true, Duration::from_millis(50)).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            std::fs::read_to_string(dir.join("cgroup.freeze")).unwrap(),
            "0"
        );
    }
}
