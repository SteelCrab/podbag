//! Container cgroups on Linux: choose where they go, create them, read why they ended,
//! and remove them.

use super::{
    Limits, OomReason, Placement, budget_values, cpu_max, oom_reason, rootless_placement,
    unavailable,
};
use crate::{Error, ErrorKind, Result};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where cgroup v2 is mounted.
pub const ROOT: &str = "/sys/fs/cgroup";

/// The controllers podbag uses.
const CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];

/// The name of the group (or systemd slice) that holds all containers: `podbag`, or
/// `PODBAG_CGROUP_PARENT`.
pub fn parent_name() -> Result<String> {
    let name = std::env::var("PODBAG_CGROUP_PARENT").unwrap_or_else(|_| "podbag".into());
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !valid {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("invalid cgroup parent {name:?}"),
            "PODBAG_CGROUP_PARENT may hold only a-z, 0-9 and _",
            "unset it, or choose a name like podbag_test",
        ));
    }
    Ok(name)
}

/// Decide where the container's cgroup goes. The second value is a warning to show when
/// the container runs without one.
pub fn choose_placement(rootless: bool, limits: &Limits) -> Result<(Placement, Option<String>)> {
    let parent = parent_name()?;
    let root = Path::new(ROOT);
    if rootless {
        return rootless_placement(
            systemd_run(),
            user_manager(),
            root.join("cgroup.controllers").exists(),
            limits,
            &parent,
        );
    }
    direct_placement(root, &parent, limits)
}

/// Root mode: the parent group `<root>/<parent>` must exist and pass the controllers down.
/// This is set up here, in the caller, so a failure becomes a warning the caller sees
/// (the shim of a background container has no terminal). The shim repeats the steps; they
/// change nothing the second time.
fn direct_placement(
    root: &Path,
    parent: &str,
    limits: &Limits,
) -> Result<(Placement, Option<String>)> {
    if !root.join("cgroup.controllers").exists() {
        return unavailable(
            limits,
            &format!("cgroup v2 is not mounted at {}", root.display()),
        );
    }
    let writable = std::ffi::CString::new(root.as_os_str().as_encoded_bytes())
        // SAFETY: access with a valid NUL-terminated path has no other preconditions.
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0);
    if !writable {
        return unavailable(limits, &format!("{} is read-only", root.display()));
    }
    let parent = root.join(parent);
    if let Err(e) = prepare_parent(&parent) {
        return unavailable(
            limits,
            &format!("cannot use the cgroup {}: {e}", parent.display()),
        );
    }
    Ok((
        Placement::Direct {
            parent: parent.to_string_lossy().into_owned(),
        },
        None,
    ))
}

/// Create the parent group and turn its controllers on, from the cgroup root down.
/// Returns the controllers that are on for the containers.
fn prepare_parent(parent: &Path) -> std::io::Result<Vec<String>> {
    if let Some(root) = parent.parent() {
        enable(root)?;
    }
    match std::fs::create_dir(parent) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    enable(parent)
}

/// Whether the user's systemd service manager is reachable (systemd-run --user needs it).
pub fn user_manager() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|d| Path::new(&d).join("systemd/private").exists())
}

/// The systemd-run program: `PODBAG_SYSTEMD_RUN`, or `systemd-run` in PATH.
pub fn systemd_run() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if let Some(p) = std::env::var_os("PODBAG_SYSTEMD_RUN") {
        let p = PathBuf::from(p);
        return executable(&p).then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("systemd-run"))
        .find(|p| executable(p))
}

/// A container's cgroup.
pub struct Group {
    path: PathBuf,
    dir: OwnedFd,
    /// podbag created it and removes it (root mode); systemd removes a scope.
    owned: bool,
}

impl Group {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The open group directory, for `CLONE_INTO_CGROUP`.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.dir.as_fd()
    }

    /// Why the kernel ended the container for memory, if it did.
    pub fn oom_reason(&self) -> Option<OomReason> {
        std::fs::read_to_string(self.path.join("memory.events"))
            .ok()
            .and_then(|events| oom_reason(&events))
    }

    /// Remove the group once its processes are gone. True when nothing is left for us to
    /// remove (systemd removes a scope's groups itself).
    pub fn remove(self) -> bool {
        !self.owned || remove_dir(&self.path)
    }
}

/// Remove an empty cgroup directory once, with the groups below it; a busy or missing
/// one is left as it is.
pub fn try_remove_dir(path: &Path) {
    remove_below(path);
    let _ = std::fs::remove_dir(path);
}

/// Remove the (empty) groups below `path`, deepest first: a container may make groups of
/// its own (BuildKit and docker-in-docker do), and the kernel removes only a group without
/// children. A loop, not recursion: the container decides how deep they go.
fn remove_below(path: &Path) {
    let mut todo = vec![(path.to_path_buf(), false)];
    while let Some((dir, children_done)) = todo.pop() {
        if children_done {
            if dir != path {
                let _ = std::fs::remove_dir(&dir);
            }
            continue;
        }
        todo.push((dir.clone(), true));
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                todo.push((entry.path(), false));
            }
        }
    }
}

/// Remove an empty cgroup directory, with the groups below it. Processes that are still
/// ending keep it busy for a moment, so retry for up to a second. If it is still busy, the next state check
/// (`state::reconcile`) tries again. True when the directory is gone.
pub fn remove_dir(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        remove_below(path);
        match std::fs::remove_dir(path) {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(()) => return true,
            Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
        }
    }
}

/// Create the cgroup of container `id` and write its limits. `budget` (bytes, `None` for
/// no limit) goes on the parent of all containers; failing to set it is only a warning.
pub fn create(
    placement: &Placement,
    id: &str,
    limits: &Limits,
    budget: Option<u64>,
) -> Result<Option<Group>> {
    match placement {
        Placement::None => Ok(None),
        Placement::Direct { parent } => direct(Path::new(parent), id, limits, budget).map(Some),
        Placement::Scope { slice } => scoped(slice, limits, budget).map(Some),
        Placement::Exact { path } => exact(Path::new(path), limits).map(Some),
    }
}

/// The group at `path`, with its missing parents; controllers are turned on from the root
/// down. The parents are left as they are (a kubelet's pod group has its own limits).
fn exact(path: &Path, limits: &Limits) -> Result<Group> {
    let fail = |e: std::io::Error| cgroup_error(path, e);
    let root = Path::new(ROOT);
    let rel = path.strip_prefix(root).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            "cannot set up the container's cgroup",
            format!("{} is not below {ROOT}", path.display()),
            "report this bug",
        )
    })?;
    let more = limits.controllers();
    let mut dir = root.to_path_buf();
    let mut enabled = enable_with(&dir, &more).map_err(fail)?;
    for part in rel.parent().into_iter().flat_map(Path::components) {
        dir.push(part);
        match std::fs::create_dir(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(fail(e)),
            _ => {}
        }
        enabled = enable_with(&dir, &more).map_err(fail)?;
    }
    if let Err(e) = std::fs::create_dir(path) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(fail(e));
        }
        remove_below(path);
        std::fs::remove_dir(path)
            .and_then(|()| std::fs::create_dir(path))
            .map_err(fail)?;
    }
    let group = configure(path, &enabled, limits).and_then(|()| {
        let dir = open_dir(path).map_err(fail)?;
        Ok(Group {
            path: path.to_path_buf(),
            dir,
            owned: true,
        })
    });
    if group.is_err() {
        let _ = std::fs::remove_dir(path);
    }
    group
}

fn warn_budget(e: impl std::fmt::Display) {
    eprintln!("podbag: warning: cannot set the memory budget: {e}");
}

/// The budget's `memory.high` and `memory.max`, unless `dir` already has them.
fn budget_change(dir: &Path, budget: Option<u64>) -> Option<(String, String)> {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let (high, max) = budget_values(budget, page);
    let current = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
    let same = current("memory.high").trim() == high && current("memory.max").trim() == max;
    (!same).then_some((high, max))
}

/// Root mode: `<parent>/<id>`, created here.
fn direct(parent: &Path, id: &str, limits: &Limits, budget: Option<u64>) -> Result<Group> {
    let fail = |e: std::io::Error| cgroup_error(parent, e);
    let enabled = prepare_parent(parent).map_err(fail)?;
    if let Some((high, max)) = budget_change(parent, budget) {
        let set = std::fs::write(parent.join("memory.high"), &high)
            .and_then(|()| std::fs::write(parent.join("memory.max"), &max));
        if let Err(e) = set {
            warn_budget(format!("{}: {e}", parent.display()));
        }
    }
    let path = parent.join(id);
    if let Err(e) = std::fs::create_dir(&path) {
        // Left by an earlier run of the same container (a restart): an empty group can be
        // removed and made again; one with processes in it fails here.
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(fail(e));
        }
        remove_below(&path);
        std::fs::remove_dir(&path)
            .and_then(|()| std::fs::create_dir(&path))
            .map_err(fail)?;
    }
    let group = configure(&path, &enabled, limits).and_then(|()| {
        let dir = open_dir(&path).map_err(fail)?;
        Ok(Group {
            path: path.clone(),
            dir,
            owned: true,
        })
    });
    if group.is_err() {
        let _ = std::fs::remove_dir(&path);
    }
    group
}

/// Rootless: this shim runs in its own delegated systemd scope. Move it to `supervisor`
/// (a group with processes cannot pass controllers down) and create `container` beside it.
/// systemd removes the scope with both groups when the shim ends.
fn scoped(slice: &str, limits: &Limits, budget: Option<u64>) -> Result<Group> {
    let own = own_cgroup().map_err(|e| cgroup_error(Path::new("/proc/self/cgroup"), e))?;
    let fail = |e: std::io::Error| cgroup_error(&own, e);
    let path = own.join("container");
    for dir in [own.join("supervisor"), path.clone()] {
        std::fs::create_dir(&dir).map_err(fail)?;
    }
    std::fs::write(own.join("supervisor/cgroup.procs"), "0").map_err(fail)?;
    // A detached shim's first process may not have ended yet; it leaves within moments.
    let deadline = Instant::now() + Duration::from_secs(1);
    let enabled = loop {
        match enable(&own) {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            r => break r.map_err(fail)?,
        }
    };
    if let Some(slice_dir) = own.parent() {
        slice_budget(slice, slice_dir, budget);
    }
    configure(&path, &enabled, limits)?;
    let dir = open_dir(&path).map_err(fail)?;
    Ok(Group {
        path,
        dir,
        owned: false,
    })
}

/// Rootless: put the budget on `slice` (its cgroup is `slice_dir`), unless it is there
/// already. Failing is only a warning.
pub fn slice_budget(slice: &str, slice_dir: &Path, budget: Option<u64>) {
    if let Some((high, max)) = budget_change(slice_dir, budget) {
        set_slice_budget(slice, &high, &max);
    }
}

/// Rootless: the slice belongs to the user's service manager; ask it to set the values.
fn set_slice_budget(slice: &str, high: &str, max: &str) {
    let systemd = |v: &str| {
        if v == "max" {
            "infinity".to_string()
        } else {
            v.to_string()
        }
    };
    let systemctl = systemd_run()
        .and_then(|p| Some(p.parent()?.join("systemctl")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("systemctl"));
    let out = std::process::Command::new(&systemctl)
        .args(["--user", "set-property", "--runtime", slice])
        .arg(format!("MemoryHigh={}", systemd(high)))
        .arg(format!("MemoryMax={}", systemd(max)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => warn_budget(format!(
            "{} {}: {}",
            systemctl.display(),
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => warn_budget(format!("{}: {e}", systemctl.display())),
    }
}

/// This process's cgroup directory, from `/proc/self/cgroup` (`0::<path>`).
pub fn own_cgroup() -> std::io::Result<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup")?;
    let rel = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| std::io::Error::other("no cgroup v2 entry in /proc/self/cgroup"))?;
    Ok(Path::new(ROOT).join(rel.trim_start_matches('/')))
}

/// Turn on the controllers podbag uses for the children of `dir`, where available.
/// Returns the ones that are on.
pub fn enable_controllers(dir: &Path) -> std::io::Result<Vec<String>> {
    enable(dir)
}

pub(super) fn enable(dir: &Path) -> std::io::Result<Vec<String>> {
    enable_with(dir, &[])
}

/// As `enable`, with further controllers when available.
fn enable_with(dir: &Path, more: &[String]) -> std::io::Result<Vec<String>> {
    let available = std::fs::read_to_string(dir.join("cgroup.controllers"))?;
    let on = std::fs::read_to_string(dir.join("cgroup.subtree_control"))?;
    let has = |list: &str, c: &str| list.split_whitespace().any(|x| x == c);
    let wanted: Vec<&str> = CONTROLLERS
        .into_iter()
        .chain(more.iter().map(String::as_str))
        .filter(|c| has(&available, c))
        .collect();
    let missing: Vec<String> = wanted
        .iter()
        .filter(|c| !has(&on, c))
        .map(|c| format!("+{c}"))
        .collect();
    if !missing.is_empty() {
        std::fs::write(dir.join("cgroup.subtree_control"), missing.join(" "))?;
    }
    Ok(wanted.into_iter().map(String::from).collect())
}

/// Write the limits of a new container group. `enabled` lists the controllers on for it.
pub(super) fn configure(path: &Path, enabled: &[String], limits: &Limits) -> Result<()> {
    let need = |controller: &str, option: &str| {
        if enabled.iter().any(|c| c == controller) {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::MissingRequirement,
                format!("cannot apply {option}"),
                format!(
                    "the cgroup {controller} controller is not available in {}",
                    path.parent().unwrap_or(path).display()
                ),
                "enable the controller for podbag's cgroup, or run without the option",
            ))
        }
    };
    let write = |file: &str, value: &str| {
        std::fs::write(path.join(file), value).map_err(|e| cgroup_error(path, e))
    };
    if enabled.iter().any(|c| c == "memory") {
        write("memory.oom.group", "1")?;
    }
    if let Some(bytes) = limits.memory {
        need("memory", "--memory")?;
        write("memory.max", &bytes.to_string())?;
        // Like Docker without --memory-swap: as much swap as memory. No swap support: skip.
        match std::fs::write(path.join("memory.swap.max"), bytes.to_string()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(cgroup_error(path, e));
            }
            _ => {}
        }
    }
    if let Some(quota) = limits.cpu_quota {
        need("cpu", "--cpus")?;
        write("cpu.max", &cpu_max(quota))?;
    }
    if let Some(pids) = limits.pids {
        need("pids", "--pids-limit")?;
        write("pids.max", &pids.to_string())?;
    }
    write_files(path, &limits.files)
}

/// Write further cgroup files (a running container's new limits, too). Swap files are
/// skipped on a kernel without swap accounting.
pub fn write_files(path: &Path, files: &std::collections::BTreeMap<String, String>) -> Result<()> {
    for (name, value) in files {
        if name.contains('/') || name.starts_with("cgroup.") {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cannot set up the container's cgroup",
                format!("{name} is not a limit file"),
                "report this bug",
            ));
        }
        match std::fs::write(path.join(name), value) {
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound && name.starts_with("memory.swap.") => {
            }
            Err(e) => {
                return Err(cgroup_error(
                    path,
                    std::io::Error::new(e.kind(), format!("{name} = {value}: {e}")),
                ));
            }
            Ok(()) => {}
        }
    }
    Ok(())
}

pub(super) fn open_dir(path: &Path) -> std::io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)?;
    Ok(dir.into())
}

fn cgroup_error(path: &Path, e: std::io::Error) -> Error {
    Error::io(
        "cannot set up the container's cgroup",
        &std::io::Error::new(e.kind(), format!("{}: {e}", path.display())),
        "check that cgroup v2 is mounted at /sys/fs/cgroup and writable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cgroup root whose controllers cannot be passed down: inside a cgroup namespace
    /// whose root has processes, writing `cgroup.subtree_control` fails (EBUSY). Here the
    /// file is a directory, which fails the same step.
    fn stuck_root() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        std::fs::create_dir(tmp.path().join("cgroup.subtree_control")).unwrap();
        tmp
    }

    #[test]
    fn unusable_root_runs_without_a_cgroup_and_warns() {
        let root = stuck_root();
        let (placement, warning) =
            direct_placement(root.path(), "podbag", &Limits::default()).unwrap();
        assert_eq!(placement, Placement::None);
        let warning = warning.unwrap();
        assert!(warning.contains("podbag"), "{warning}");
    }

    #[test]
    fn unusable_root_refuses_limits() {
        let root = stuck_root();
        let limits = Limits {
            pids: Some(10),
            ..Default::default()
        };
        let e = direct_placement(root.path(), "podbag", &limits).unwrap_err();
        assert_eq!(e.kind, ErrorKind::MissingRequirement);
    }
}
