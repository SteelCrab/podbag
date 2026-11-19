//! Rootless: the keeper lives in one delegated systemd scope and makes the cgroups of its
//! containers there itself, so a start needs no `systemd-run` of its own.

use crate::cgroup::{Group, Limits, Placement};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `<scope>/c`: the parent of the containers' cgroups, when the keeper has a scope.
static PARENT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// As root: the group of the keepers and of the helpers they fork. Not the group of
/// whoever started the first container (a login session, a service): that one may be
/// stopped while containers of other callers run.
const ROOT_GROUP: &str = "podbag.keeper";

fn root_group() -> PathBuf {
    Path::new(crate::cgroup::ROOT).join(ROOT_GROUP)
}

/// In the keeper, once: when it runs in a delegated scope of the user's service manager,
/// move it to `<scope>/keeper` (a group with processes cannot pass controllers down) and
/// make `<scope>/c` for the containers.
pub(super) fn settle() {
    PARENT.get_or_init(|| {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            // Containers get their groups below the parent group, as without a keeper;
            // only the keeper itself moves. Where it cannot, it stays where it is.
            let group = root_group();
            if std::fs::create_dir(&group).is_ok() || group.is_dir() {
                let _ = std::fs::write(group.join("cgroup.procs"), "0");
            }
            return None;
        }
        let own = crate::cgroup::own_cgroup().ok()?;
        own.extension().filter(|e| *e == "scope")?;
        let leaf = own.join("keeper");
        let parent = own.join("c");
        std::fs::create_dir(&leaf).ok()?;
        std::fs::create_dir(&parent).ok()?;
        std::fs::write(leaf.join("cgroup.procs"), "0").ok()?;
        // The first process of the keeper (it forked) may not have ended yet.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match crate::cgroup::enable_controllers(&own) {
                Err(e)
                    if e.raw_os_error() == Some(libc::EBUSY)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Ok(_) => break,
                Err(_) => return None,
            }
        }
        Some(parent)
    });
}

/// At the keeper's end, as root: go back to the root group and remove the keepers' group
/// when this was its last process, so that nothing of podbag stays at rest.
pub(super) fn leave() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let group = root_group();
    if crate::cgroup::own_cgroup().is_ok_and(|own| own == group) {
        let _ = std::fs::write(Path::new(crate::cgroup::ROOT).join("cgroup.procs"), "0");
    }
    // Fails while another keeper or one of its helpers is there: theirs to remove.
    let _ = std::fs::remove_dir(&group);
}

/// The parent for the cgroups of this keeper's containers (inherited by its starters).
pub fn parent() -> Option<PathBuf> {
    PARENT.get().cloned().flatten()
}

/// Where a starter makes its container's cgroup: a rootless container that would get a
/// systemd scope of its own gets a group below the keeper's scope instead.
pub fn placement(wanted: &Placement, limits: &Limits) -> crate::Result<Placement> {
    if !super::under_keeper() {
        return Ok(wanted.clone());
    }
    match (wanted, parent()) {
        (Placement::Scope { .. }, Some(parent)) => Ok(Placement::Direct {
            parent: parent.to_string_lossy().into_owned(),
        }),
        (Placement::Scope { .. }, None) => {
            let (placement, warning) = crate::cgroup::unavailable(
                limits,
                "the keeper runs outside a systemd scope, so it cannot make cgroups",
            )?;
            if let Some(w) = warning {
                eprintln!("podbag: warning: {w}");
            }
            Ok(placement)
        }
        _ => Ok(wanted.clone()),
    }
}

/// Make the cgroup of container `id` (see `placement`). Under the keeper the memory
/// budget stays where it is without one: on the slice that holds the keeper's scope, so
/// one budget covers every container of the slice, whoever watches it.
pub fn create(
    wanted: &Placement,
    id: &str,
    limits: &Limits,
    budget: Option<u64>,
) -> crate::Result<Option<Group>> {
    let placement = placement(wanted, limits)?;
    let (Placement::Scope { slice }, Placement::Direct { parent }) = (wanted, &placement) else {
        return crate::cgroup::create(&placement, id, limits, budget);
    };
    // `<slice>/<scope>/c`: the keeper's slice is the one its first client asked for.
    let slice_dir = Path::new(parent).parent().and_then(Path::parent);
    let name = slice_dir
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .filter(|n| n.ends_with(".slice"));
    if let (Some(dir), Some(name)) = (slice_dir, name) {
        if name != slice {
            eprintln!(
                "podbag: warning: the keeper of this runtime directory runs in {name}; \
                 this container goes there, not to {slice}"
            );
        }
        crate::cgroup::slice_budget(name, dir, budget);
    }
    crate::cgroup::create(&placement, id, limits, None)
}
