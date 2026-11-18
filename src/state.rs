//! Container directories and their settings files.

use crate::paths::Dirs;
use crate::spec::ContainerSpec;
use crate::{Error, ErrorKind, Result, id};
use serde::{Deserialize, Serialize};
use std::io::Write;

const CONFIG: &str = "config.json";

/// Create the container directory (mode 0700) and write its settings atomically.
pub fn write_config(dirs: &Dirs, spec: &ContainerSpec) -> Result<()> {
    id::check(&spec.id)?;
    let dir = dirs.container_dir(&spec.id);
    let fail = |e: std::io::Error| {
        Error::io(
            format!("cannot save container {}", spec.id),
            &e,
            format!("check that {} is writable", dirs.data.display()),
        )
    };
    create_private_dir(&dir).map_err(fail)?;

    let json = serde_json::to_vec_pretty(spec).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "cannot encode container settings",
            e.to_string(),
            "report this bug",
        )
    })?;
    let tmp = dir.join(format!(".{CONFIG}.tmp"));
    let mut f = std::fs::File::create(&tmp).map_err(fail)?;
    f.write_all(&json)
        .and_then(|_| f.sync_all())
        .map_err(fail)?;
    std::fs::rename(&tmp, dir.join(CONFIG)).map_err(fail)
}

pub fn read_config(dirs: &Dirs, id: &str) -> Result<ContainerSpec> {
    id::check(id)?;
    let path = dirs.container_dir(id).join(CONFIG);
    let bytes = std::fs::read(&path).map_err(|e| {
        let kind = if e.kind() == std::io::ErrorKind::NotFound {
            ErrorKind::NotFound
        } else {
            ErrorKind::Runtime
        };
        Error::new(
            kind,
            format!("cannot load container {id}"),
            format!("{}: {e}", path.display()),
            "check the container ID",
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            format!("cannot load container {id}"),
            format!("{} is damaged: {e}", path.display()),
            "remove the container directory",
        )
    })
}

/// Remove a container directory. Removing a directory that does not exist succeeds.
pub fn remove_container_dir(dirs: &Dirs, id: &str) -> Result<()> {
    id::check(id)?;
    let dir = dirs.container_dir(id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(
            format!("cannot remove container directory {}", dir.display()),
            &e,
            "remove it by hand",
        )),
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(parent) = dir.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

const STATE: &str = "state.json";

/// Where a container is in its life.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Created,
    Running,
    Exited,
}

/// The changing part of a container record. Times are seconds since the epoch.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub status: Status,
    pub pid: Option<u32>,
    pub shim_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub created: u64,
    pub started: Option<u64>,
    pub finished: Option<u64>,
    /// The container's cgroup directory, while it has one.
    #[serde(default)]
    pub cgroup: Option<String>,
    /// Why the kernel ended the container for memory, if it did.
    #[serde(default)]
    pub oom: Option<crate::cgroup::OomReason>,
    /// The container's address on its bridge network.
    #[serde(default)]
    pub ip: Option<String>,
    /// Published ports, with the host ports in use.
    #[serde(default)]
    pub ports: Vec<crate::network::PortMapping>,
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn write_state(dirs: &Dirs, id: &str, state: &State) -> Result<()> {
    id::check(id)?;
    let dir = dirs.container_dir(id);
    let json = serde_json::to_vec(state).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "cannot encode container state",
            e.to_string(),
            "report this bug",
        )
    })?;
    // A name of its own for each write, so two writers cannot mix their bytes.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{STATE}.{}.{n}.tmp", std::process::id()));
    let fail = |e: std::io::Error| {
        Error::io(
            format!("cannot save the state of container {id}"),
            &e,
            format!("check that {} is writable", dirs.data.display()),
        )
    };
    let mut f = std::fs::File::create(&tmp).map_err(fail)?;
    f.write_all(&json).map_err(fail)?;
    std::fs::rename(&tmp, dir.join(STATE)).map_err(fail)
}

pub fn read_state(dirs: &Dirs, id: &str) -> Result<State> {
    id::check(id)?;
    let path = dirs.container_dir(id).join(STATE);
    let bytes = std::fs::read(&path).map_err(|e| {
        Error::io(
            format!("cannot load container {id}"),
            &e,
            "run `openpod ps -a` to list containers",
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            format!("cannot load container {id}"),
            format!("{} is damaged: {e}", path.display()),
            "remove the container with `openpod rm -f`",
        )
    })
}

/// IDs of every container directory.
pub fn list_ids(dirs: &Dirs) -> Result<Vec<String>> {
    let dir = dirs.data.join("containers");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::io(
                "cannot list containers",
                &e,
                format!("check that {} is readable", dir.display()),
            ));
        }
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| id::is_valid(n))
        .collect();
    ids.sort();
    Ok(ids)
}

/// Find a container by full ID, name, or ID prefix of at least 4 characters.
pub fn resolve(dirs: &Dirs, name_or_id: &str) -> Result<String> {
    let not_found = || {
        Error::new(
            ErrorKind::NotFound,
            format!("cannot find container {name_or_id}"),
            "no container has this name or ID",
            "run `openpod ps -a` to list containers",
        )
    };
    let ids = list_ids(dirs)?;
    if ids.iter().any(|i| i == name_or_id) {
        return Ok(name_or_id.to_string());
    }
    for i in &ids {
        if read_config(dirs, i).is_ok_and(|c| c.name == name_or_id) {
            return Ok(i.clone());
        }
    }
    if name_or_id.len() < 4 {
        return Err(not_found());
    }
    let matches: Vec<&String> = ids.iter().filter(|i| i.starts_with(name_or_id)).collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(not_found()),
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("cannot choose container {name_or_id}"),
            format!("{name_or_id} matches more than one container"),
            "give more characters of the ID, or use the name",
        )),
    }
}

/// Record a new container: check its name is free, then write its settings and a
/// "created" state. Names are checked under a lock, so two creates cannot both win.
pub fn create(dirs: &Dirs, spec: &ContainerSpec) -> Result<()> {
    std::fs::create_dir_all(dirs.data.join("containers")).map_err(|e| {
        Error::io(
            "cannot create the container directory",
            &e,
            format!("check that {} is writable", dirs.data.display()),
        )
    })?;
    let lock_path = dirs.data.join("containers.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| {
            Error::io(
                "cannot lock the container list",
                &e,
                "check the permissions of the openpod data directory",
            )
        })?;
    // Released when `lock` is dropped.
    lock.lock().map_err(|e| {
        Error::io(
            "cannot lock the container list",
            &e,
            "check the permissions of the openpod data directory",
        )
    })?;
    for other in list_ids(dirs)? {
        if read_config(dirs, &other).is_ok_and(|c| c.name == spec.name) {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("cannot create container {}", spec.name),
                format!(
                    "the name {} is in use by container {}",
                    spec.name,
                    id::short_id(&other)
                ),
                "choose another --name, or remove that container",
            ));
        }
    }
    write_config(dirs, spec)?;
    let state = State {
        status: Status::Created,
        pid: None,
        shim_pid: None,
        exit_code: None,
        created: now(),
        started: None,
        finished: None,
        cgroup: None,
        oom: None,
        ip: None,
        ports: Vec::new(),
    };
    write_state(dirs, &spec.id, &state)
}

const SHIM_LOCK: &str = "shim.lock";

/// The shim holds this lock for its whole life. Readers that can take it know the shim is
/// gone. Unlike a PID check, this cannot be fooled by a reused PID.
pub fn hold_shim_lock(dirs: &Dirs, id: &str) -> Result<std::fs::File> {
    // Released when the file is closed. Held already: another shim serves the container.
    try_shim_lock(dirs, id)?.ok_or_else(|| already_running(id))
}

fn already_running(id: &str) -> Error {
    Error::new(
        ErrorKind::Conflict,
        format!("cannot start container {}", id::short_id(id)),
        "it is already running",
        "stop it first, or use `openpod restart`",
    )
}

/// Wait up to `limit` for the last run's shim to let go of container `id`: it answers
/// `stop` and `wait` before it has cleaned up and ended. A container that runs is
/// refused at once.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn wait_for_previous_shim(dirs: &Dirs, id: &str, limit: std::time::Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        // Taken and dropped at once: free for the new shim.
        if try_shim_lock(dirs, id)?.is_some() {
            return Ok(());
        }
        if read_state(dirs, id).is_ok_and(|s| s.status == Status::Running) {
            return Err(already_running(id));
        }
        if std::time::Instant::now() >= deadline {
            // The new shim tries once more and reports the conflict.
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The shim lock, if no shim holds it.
fn try_shim_lock(dirs: &Dirs, id: &str) -> Result<Option<std::fs::File>> {
    let file = open_shim_lock(dirs, id)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(lock_error(id, e)),
    }
}

fn open_shim_lock(dirs: &Dirs, id: &str) -> Result<std::fs::File> {
    id::check(id)?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dirs.container_dir(id).join(SHIM_LOCK))
        .map_err(|e| lock_error(id, e))
}

fn lock_error(id: &str, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot check container {}", id::short_id(id)),
        &e,
        "check the permissions of the openpod data directory",
    )
}

/// Read the state, and fix it when it says "running" but the shim is gone (killed, or the
/// machine rebooted): the container counts as exited with an unknown exit code.
pub fn reconcile(dirs: &Dirs, id: &str) -> Result<State> {
    let state = read_state(dirs, id)?;
    let state = settle(dirs, id, state)?;
    // An ended container's cgroup may remain when an earlier removal found it busy (root
    // mode); every state check tries once more (design 6.12).
    #[cfg(target_os = "linux")]
    if state.status == Status::Exited {
        if let Some(path) = state.cgroup.as_deref() {
            if path.starts_with(crate::cgroup::ROOT) {
                crate::cgroup::try_remove_dir(std::path::Path::new(path));
            }
        }
    }
    Ok(state)
}

/// Whether container `id`'s shim still runs (it holds its lock), without changing state.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn shim_running(dirs: &Dirs, id: &str) -> bool {
    matches!(try_shim_lock(dirs, id), Ok(None))
}

/// `reconcile` for a state read earlier.
fn settle(dirs: &Dirs, id: &str, seen: State) -> Result<State> {
    if seen.status != Status::Running {
        return Ok(seen);
    }
    let Some(_lock) = try_shim_lock(dirs, id)? else {
        return Ok(seen);
    };
    // The shim is gone. Read again: it may have written the exit code just before it ended.
    let mut state = read_state(dirs, id)?;
    if state.status == Status::Running {
        state.status = Status::Exited;
        state.exit_code = None;
        state.finished = Some(now());
        write_state(dirs, id, &state)?;
        // The shim did not give back the container's address (or, rootless, its place
        // in the network session); do it now.
        if let Ok(spec) = read_config(dirs, id) {
            if let crate::network::NetworkMode::Bridge { network } = &spec.network {
                let _ = crate::network::release(dirs, network, id);
                crate::network::refresh_hosts(dirs, network);
                // SAFETY: geteuid has no preconditions.
                #[cfg(target_os = "linux")]
                if unsafe { libc::geteuid() } != 0 {
                    crate::linux::session::release(dirs, id);
                }
            }
        }
        // The shim did not remove the container's cgroup (root mode); do it now.
        #[cfg(target_os = "linux")]
        if let Some(path) = state.cgroup.as_deref() {
            if path.starts_with(crate::cgroup::ROOT) {
                crate::cgroup::remove_dir(std::path::Path::new(path));
            }
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    fn spec_named(id_char: char, name: &str) -> ContainerSpec {
        let mut s = spec_fixture();
        s.id = id_char.to_string().repeat(64);
        s.name = name.to_string();
        s
    }

    #[test]
    fn state_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let spec = spec_named('a', "web");
        create(&dirs, &spec).unwrap();
        assert_eq!(read_state(&dirs, &spec.id).unwrap().status, Status::Created);
        let st = State {
            status: Status::Exited,
            pid: Some(7),
            shim_pid: Some(8),
            exit_code: Some(3),
            created: 1,
            started: Some(2),
            finished: Some(3),
            cgroup: None,
            oom: None,
            ip: None,
            ports: Vec::new(),
        };
        write_state(&dirs, &spec.id, &st).unwrap();
        assert_eq!(read_state(&dirs, &spec.id).unwrap(), st);
    }

    #[test]
    fn resolve_by_name_full_id_and_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create(&dirs, &spec_named('a', "web")).unwrap();
        let mut other = spec_named('b', "db");
        other.id = format!("ab{}", "c".repeat(62));
        create(&dirs, &other).unwrap();
        let a = "a".repeat(64);
        assert_eq!(resolve(&dirs, "web").unwrap(), a);
        assert_eq!(resolve(&dirs, &a).unwrap(), a);
        assert_eq!(resolve(&dirs, "aaaa").unwrap(), a);
        assert_eq!(resolve(&dirs, "abcc").unwrap(), other.id);
        assert_eq!(resolve(&dirs, "aaa").unwrap_err().kind, ErrorKind::NotFound);
        let e = resolve(&dirs, "a").unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        let e = resolve(&dirs, "nope").unwrap_err();
        assert_eq!(e.what, "cannot find container nope");
    }

    #[test]
    fn ambiguous_prefix_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let mut one = spec_named('a', "one");
        one.id = format!("abcd1{}", "0".repeat(59));
        let mut two = spec_named('a', "two");
        two.id = format!("abcd2{}", "0".repeat(59));
        create(&dirs, &one).unwrap();
        create(&dirs, &two).unwrap();
        let e = resolve(&dirs, "abcd").unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert!(
            e.why.contains("matches more than one container"),
            "{}",
            e.why
        );
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create(&dirs, &spec_named('a', "web")).unwrap();
        let e = create(&dirs, &spec_named('b', "web")).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Conflict);
        assert_eq!(e.what, "cannot create container web");
        assert_eq!(
            e.why,
            format!("the name web is in use by container {}", "a".repeat(12))
        );
        assert!(!dirs.container_dir(&"b".repeat(64)).exists());
    }

    #[test]
    fn a_held_shim_lock_means_running() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let spec = spec_named('a', "web");
        create(&dirs, &spec).unwrap();
        let running = State {
            status: Status::Running,
            pid: Some(10),
            shim_pid: Some(11),
            exit_code: None,
            created: 1,
            started: Some(2),
            finished: None,
            cgroup: None,
            oom: None,
            ip: None,
            ports: Vec::new(),
        };
        write_state(&dirs, &spec.id, &running).unwrap();
        let lock = hold_shim_lock(&dirs, &spec.id).unwrap();
        assert_eq!(reconcile(&dirs, &spec.id).unwrap().status, Status::Running);
        drop(lock);
        let after = reconcile(&dirs, &spec.id).unwrap();
        assert_eq!((after.status, after.exit_code), (Status::Exited, None));
    }

    // The shim writes "exited" and then ends. A reader that read "running" just before
    // must not overwrite the real exit code.
    #[test]
    fn settle_keeps_an_exit_code_written_after_the_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let spec = spec_named('a', "web");
        create(&dirs, &spec).unwrap();
        let seen = State {
            status: Status::Running,
            pid: Some(10),
            shim_pid: Some(11),
            exit_code: None,
            created: 1,
            started: Some(2),
            finished: None,
            cgroup: None,
            oom: None,
            ip: None,
            ports: Vec::new(),
        };
        let done = State {
            status: Status::Exited,
            exit_code: Some(7),
            finished: Some(3),
            cgroup: None,
            oom: None,
            ip: None,
            ports: Vec::new(),
            ..seen.clone()
        };
        write_state(&dirs, &spec.id, &done).unwrap();
        assert_eq!(settle(&dirs, &spec.id, seen).unwrap().exit_code, Some(7));
        assert_eq!(read_state(&dirs, &spec.id).unwrap().exit_code, Some(7));
    }

    // Two writers at once must not leave a damaged file.
    #[test]
    fn concurrent_state_writes_stay_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let spec = spec_named('a', "web");
        create(&dirs, &spec).unwrap();
        let st = read_state(&dirs, &spec.id).unwrap();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..200 {
                        let _ = write_state(&dirs, &spec.id, &st);
                    }
                });
            }
            for _ in 0..200 {
                read_state(&dirs, &spec.id).unwrap();
            }
        });
    }

    #[test]
    fn reconcile_marks_a_dead_shim_as_exited() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let spec = spec_named('a', "web");
        create(&dirs, &spec).unwrap();
        let running = State {
            status: Status::Running,
            pid: Some(10),
            shim_pid: Some(11),
            exit_code: None,
            created: 1,
            started: Some(2),
            finished: None,
            cgroup: None,
            oom: None,
            ip: None,
            ports: Vec::new(),
        };
        write_state(&dirs, &spec.id, &running).unwrap();
        let fixed = reconcile(&dirs, &spec.id).unwrap();
        assert_eq!((fixed.status, fixed.exit_code), (Status::Exited, None));
        assert!(fixed.finished.is_some());
        assert_eq!(read_state(&dirs, &spec.id).unwrap().status, Status::Exited);
    }

    use super::*;
    use crate::ErrorKind;
    use crate::paths::Dirs;
    use crate::spec::spec_fixture;

    fn dirs_in(tmp: &std::path::Path) -> Dirs {
        Dirs {
            data: tmp.join("data"),
            runtime: tmp.join("run"),
            config: tmp.join("cfg"),
        }
    }

    #[test]
    fn config_round_trips_and_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (dirs, spec) = (dirs_in(tmp.path()), spec_fixture());
        write_config(&dirs, &spec).unwrap();
        assert_eq!(read_config(&dirs, &spec.id).unwrap(), spec);
        let mode = std::fs::metadata(dirs.container_dir(&spec.id))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let names: Vec<_> = std::fs::read_dir(dirs.container_dir(&spec.id))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.json")]);
    }

    #[test]
    fn missing_container_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "0123abcd".repeat(8);
        let e = read_config(&dirs_in(tmp.path()), &id).unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(e.what, format!("cannot load container {id}"));
    }

    #[test]
    fn invalid_ids_never_touch_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let keep = dirs.data.join("keep");
        std::fs::create_dir_all(&keep).unwrap();
        for bad in ["..", "../..", ""] {
            assert_eq!(
                remove_container_dir(&dirs, bad).unwrap_err().kind,
                ErrorKind::InvalidInput
            );
            assert_eq!(
                read_config(&dirs, bad).unwrap_err().kind,
                ErrorKind::InvalidInput
            );
        }
        let mut spec = spec_fixture();
        spec.id = "../escape".into();
        assert_eq!(
            write_config(&dirs, &spec).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert!(keep.exists());
    }

    #[test]
    fn remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let (dirs, spec) = (dirs_in(tmp.path()), spec_fixture());
        write_config(&dirs, &spec).unwrap();
        remove_container_dir(&dirs, &spec.id).unwrap();
        assert!(!dirs.container_dir(&spec.id).exists());
        remove_container_dir(&dirs, &spec.id).unwrap();
    }
}
