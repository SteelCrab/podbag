//! Volumes: `-v` and `--tmpfs` arguments, the mounts a container gets, and the named
//! volume store (`<data>/volumes/<name>/{data,meta.json}`). The logic does not depend on
//! the OS; mounting happens in `linux::mounts`.

use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

/// A named (or anonymous) volume in `<data>/volumes/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    /// The host directory that containers mount (`<data>/volumes/<name>/data`).
    pub mountpoint: String,
    /// Seconds since the epoch.
    pub created: u64,
    /// Made for one container (`-v /target` or an image's `Volumes`).
    pub anonymous: bool,
    /// Labels (Docker's `--label`; Compose finds a project's volumes by them).
    #[serde(default)]
    pub labels: crate::network::Labels,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    name: String,
    created: u64,
    anonymous: bool,
    /// Missing in the files of volumes made before volumes had labels.
    #[serde(default)]
    labels: crate::network::Labels,
}

const META: &str = "meta.json";

impl Volume {
    /// Create a volume, or return the one with this name. No name: a new anonymous-style
    /// name (64 hex digits), like `docker volume create`.
    pub fn create(name: Option<&str>) -> Result<Volume> {
        Self::create_with(name, Default::default())
    }

    /// As `create`, with labels for a new volume. A volume that exists is returned as it
    /// is: its labels do not change.
    pub fn create_with(name: Option<&str>, labels: crate::network::Labels) -> Result<Volume> {
        let dirs = crate::paths::dirs()?;
        let _lock = lock_shared(&dirs)?;
        create_labeled_in(&dirs, name, false, labels).map(|(v, _)| v)
    }

    pub fn get(name: &str) -> Result<Volume> {
        get_in(&crate::paths::dirs()?, name)
    }

    /// Every volume, by name.
    pub fn list() -> Result<Vec<Volume>> {
        list_in(&crate::paths::dirs()?)
    }

    /// Delete the volumes no container mounts: anonymous ones, or all with `all`. Returns
    /// their names and the bytes their files took.
    pub fn prune(all: bool) -> Result<(Vec<String>, u64)> {
        Self::prune_matching(all, &|_| true)
    }

    /// As `prune`, for the volumes that `pick` chooses.
    pub fn prune_matching(all: bool, pick: &dyn Fn(&Volume) -> bool) -> Result<(Vec<String>, u64)> {
        prune_matching_in(&crate::paths::dirs()?, all, pick)
    }

    /// Delete a volume and its files. A volume that any container (running or not) mounts
    /// is refused, even with `force`; `force` only makes a missing volume no error.
    pub fn remove(name: &str, force: bool) -> Result<()> {
        remove_in(&crate::paths::dirs()?, name, force)
    }
}

fn store_error(path: &std::path::Path, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot use {}", path.display()),
        &e,
        "check the permissions of the podbag data directory",
    )
}

/// `<data>/volumes`, private (0700) like the data directory above it: copied-up files keep
/// their set-user-ID bits, and other host users must not reach them.
fn volumes_dir(dirs: &Dirs) -> Result<std::path::PathBuf> {
    let dir = dirs.data.join("volumes");
    let fail = |e: std::io::Error| store_error(&dir, e);
    let mut builder = crate::store::perm::private_dir_builder();
    builder.recursive(true).create(&dir).map_err(fail)?;
    crate::store::perm::set_mode(&dirs.data, 0o700).map_err(fail)?;
    crate::store::perm::set_mode(&dir, 0o700).map_err(fail)?;
    Ok(dir)
}

fn volume_of(dirs: &Dirs, meta: Meta) -> Volume {
    Volume {
        mountpoint: dirs
            .volume_dir(&meta.name)
            .join("data")
            .to_string_lossy()
            .into_owned(),
        name: meta.name,
        created: meta.created,
        anonymous: meta.anonymous,
        labels: meta.labels,
    }
}

/// Create a volume unless it exists. Returns it and whether it is new.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn create_in(
    dirs: &Dirs,
    name: Option<&str>,
    anonymous: bool,
) -> Result<(Volume, bool)> {
    create_labeled_in(dirs, name, anonymous, Default::default())
}

/// As `create_in`, with the labels of a new volume (one that exists keeps its own).
pub(crate) fn create_labeled_in(
    dirs: &Dirs,
    name: Option<&str>,
    anonymous: bool,
    labels: crate::network::Labels,
) -> Result<(Volume, bool)> {
    let name = match name {
        Some(n) => {
            check_name(n)?;
            n.to_string()
        }
        None => crate::id::new_id()?,
    };
    let dir = dirs.volume_dir(&name);
    volumes_dir(dirs)?;
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Made earlier, or being made by another process right now.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match get_in(dirs, &name) {
                    Err(e)
                        if e.kind == ErrorKind::NotFound
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    other => return other.map(|v| (v, false)),
                }
            }
        }
        Err(e) => return Err(store_error(&dir, e)),
    }
    let data = dir.join("data");
    std::fs::create_dir(&data)
        .and_then(|()| crate::store::perm::set_mode(&data, 0o755))
        .map_err(|e| store_error(&data, e))?;
    let meta = Meta {
        name,
        created: crate::state::now(),
        anonymous,
        labels,
    };
    let json = serde_json::to_vec(&meta).unwrap_or_default();
    let tmp = dir.join(".meta.json.tmp");
    std::fs::write(&tmp, json)
        .and_then(|()| std::fs::rename(&tmp, dir.join(META)))
        .map_err(|e| store_error(&dir, e))?;
    Ok((volume_of(dirs, meta), true))
}

pub(crate) fn get_in(dirs: &Dirs, name: &str) -> Result<Volume> {
    check_name(name)?;
    let path = dirs.volume_dir(name).join(META);
    let bytes = std::fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::new(
                ErrorKind::NotFound,
                format!("no such volume: {name}"),
                "it does not exist",
                "run `podbag volume ls` to list volumes",
            )
        } else {
            store_error(&path, e)
        }
    })?;
    let meta: Meta = serde_json::from_slice(&bytes).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            format!("cannot read volume {name}"),
            format!("{} is damaged: {e}", path.display()),
            "remove the volume with `podbag volume rm`",
        )
    })?;
    Ok(volume_of(dirs, meta))
}

pub(crate) fn list_in(dirs: &Dirs) -> Result<Vec<Volume>> {
    let dir = dirs.data.join("volumes");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(store_error(&dir, e)),
    };
    let mut volumes: Vec<Volume> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|name| get_in(dirs, &name).ok())
        .collect();
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(volumes)
}

/// Names of the containers whose settings mount volume `name`.
pub(crate) fn users_of(dirs: &Dirs, name: &str) -> Result<Vec<String>> {
    users_except(dirs, name, "")
}

/// Names of the containers other than `except_id` whose settings mount volume `name`.
fn users_except(dirs: &Dirs, name: &str, except_id: &str) -> Result<Vec<String>> {
    let mut users = Vec::new();
    for id in crate::state::list_ids(dirs)? {
        if id == except_id {
            continue;
        }
        let Ok(spec) = crate::state::read_config(dirs, &id) else {
            continue;
        };
        let uses = spec
            .mounts
            .iter()
            .any(|m| matches!(&m.source, MountSource::Volume { name: n, .. } if n == name));
        if uses {
            users.push(spec.name);
        }
    }
    Ok(users)
}

fn lock(dirs: &Dirs, exclusive: bool) -> Result<std::fs::File> {
    let path = volumes_dir(dirs)?.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| store_error(&path, e))?;
    let locked = if exclusive {
        file.lock()
    } else {
        file.lock_shared()
    };
    locked.map_err(|e| store_error(&path, e))?;
    Ok(file)
}

/// Held while containers are created with volumes: `volume rm` waits for it.
pub(crate) fn lock_shared(dirs: &Dirs) -> Result<std::fs::File> {
    lock(dirs, false)
}

pub(crate) fn remove_in(dirs: &Dirs, name: &str, force: bool) -> Result<()> {
    check_name(name)?;
    let _lock = lock(dirs, true)?;
    match get_in(dirs, name) {
        Ok(_) => {}
        Err(e) if e.kind == ErrorKind::NotFound && force => return Ok(()),
        Err(e) => return Err(e),
    }
    let users = users_of(dirs, name)?;
    if !users.is_empty() {
        return Err(Error::new(
            ErrorKind::Conflict,
            format!("cannot remove volume {name}"),
            format!("it is used by container {}", users.join(", ")),
            "remove those containers first (`podbag rm`)",
        ));
    }
    remove_files(dirs, name)
}

#[cfg(test)]
pub(crate) fn prune_in(dirs: &Dirs, all: bool) -> Result<(Vec<String>, u64)> {
    prune_matching_in(dirs, all, &|_| true)
}

pub(crate) fn prune_matching_in(
    dirs: &Dirs,
    all: bool,
    pick: &dyn Fn(&Volume) -> bool,
) -> Result<(Vec<String>, u64)> {
    let mut gone = Vec::new();
    let mut bytes = 0;
    for v in list_in(dirs)? {
        if (!all && !v.anonymous) || !pick(&v) {
            continue;
        }
        let size = tree_size(&dirs.volume_dir(&v.name));
        // Checks the users again under the volume lock: one mounted meanwhile stays.
        match remove_in(dirs, &v.name, false) {
            Ok(()) => {
                gone.push(v.name);
                bytes += size;
            }
            Err(e) if matches!(e.kind, ErrorKind::Conflict | ErrorKind::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok((gone, bytes))
}

/// Bytes of the files under `path`, as far as they can be read.
fn tree_size(path: &std::path::Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| tree_size(&e.path()))
        .sum()
}

/// Remove the anonymous volumes of container `own_id` that no other container names
/// (Docker skips volumes still in use the same way).
#[cfg(unix)]
pub(crate) fn remove_anonymous<'a>(
    dirs: &Dirs,
    own_id: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let _lock = lock(dirs, true)?;
    let mut result = Ok(());
    for name in names {
        let removed = users_except(dirs, name, own_id).and_then(|users| {
            if users.is_empty() {
                remove_files(dirs, name)
            } else {
                Ok(())
            }
        });
        if result.is_ok() {
            result = removed;
        }
    }
    result
}

/// The anonymous volumes among `mounts`: they go away with their container.
pub fn anonymous_volumes(mounts: &[Mount]) -> impl Iterator<Item = &str> {
    mounts.iter().filter_map(|m| match &m.source {
        MountSource::Volume {
            name,
            anonymous: true,
            ..
        } => Some(name.as_str()),
        _ => None,
    })
}

/// Delete a volume's directory. Rootless, files that container users created belong to
/// subordinate IDs; the store helper removes them inside the user namespace.
pub(crate) fn remove_files(dirs: &Dirs, name: &str) -> Result<()> {
    let dir = dirs.volume_dir(name);
    match crate::store::remove_tree(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        #[cfg(target_os = "linux")]
        Err(_) => crate::helper::run_store_op(
            &crate::store::ops::StoreOp::RemoveVolume {
                name: name.to_string(),
            },
            false,
        )
        .map(drop),
        #[cfg(not(target_os = "linux"))]
        Err(e) => Err(store_error(&dir, e)),
    }
}

/// Whether a mount may be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Access {
    ReadWrite,
    ReadOnly,
}

/// A mount as the caller asked for it (`-v`, `--tmpfs`, the library builder, or an
/// image's `Volumes`). The library turns it into a [`Mount`] when the container starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MountRequest {
    /// A host path; relative paths are relative to the caller's working directory.
    Bind {
        source: String,
        target: String,
        access: Access,
    },
    /// A named volume, created on first use.
    Named {
        name: String,
        target: String,
        access: Access,
        /// Do not fill a new volume with the image's files at the target.
        nocopy: bool,
    },
    /// A volume of its own, removed with the container under `--rm` or `rm -v`.
    Anonymous { target: String, access: Access },
    /// A memory file system; `options` are mount options.
    Tmpfs { target: String, options: String },
}

impl MountRequest {
    pub fn target(&self) -> &str {
        match self {
            MountRequest::Bind { target, .. }
            | MountRequest::Named { target, .. }
            | MountRequest::Anonymous { target, .. }
            | MountRequest::Tmpfs { target, .. } => target,
        }
    }

    fn target_mut(&mut self) -> &mut String {
        match self {
            MountRequest::Bind { target, .. }
            | MountRequest::Named { target, .. }
            | MountRequest::Anonymous { target, .. }
            | MountRequest::Tmpfs { target, .. } => target,
        }
    }
}

/// Where a mount comes from, with the host path the library resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MountSource {
    Bind {
        path: String,
    },
    Volume {
        name: String,
        anonymous: bool,
        /// The volume's `data` directory.
        path: String,
    },
    Tmpfs {
        options: String,
    },
}

/// A mount in a container's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: MountSource,
    pub target: String,
    pub read_only: bool,
    /// Fill the (empty, new) volume with the image's files at the target first.
    #[serde(default)]
    pub copy_up: bool,
}

fn invalid(what: String, why: &str, next: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, what, why, next)
}

/// A `-v` value: `SOURCE:TARGET[:OPTIONS]` or `TARGET` alone (an anonymous volume). A
/// source that starts with `/` or `.` is a host path; anything else names a volume.
/// Options: `ro`, `rw`, `nocopy`; `cached`, `delegated` and `consistent` are accepted and
/// do nothing, as on Linux Docker.
pub fn parse_volume_arg(s: &str) -> Result<MountRequest> {
    let bad = |why: &str| {
        invalid(
            format!("invalid volume {s:?}"),
            why,
            "write it like /host/dir:/dir, name:/dir, or /dir, with :ro to make it read-only",
        )
    };
    let parts: Vec<&str> = s.split(':').collect();
    let (source, target, options) = match parts.as_slice() {
        [target] => (None, *target, None),
        [source, target] => (Some(*source), *target, None),
        [source, target, options] => (Some(*source), *target, Some(*options)),
        _ => return Err(bad("it has more than three parts")),
    };
    let target = normalize_target(target)?;
    let mut access = Access::ReadWrite;
    let mut nocopy = false;
    for option in options.unwrap_or("").split(',').filter(|o| !o.is_empty()) {
        match option {
            "ro" => access = Access::ReadOnly,
            "rw" => access = Access::ReadWrite,
            "nocopy" => nocopy = true,
            "cached" | "delegated" | "consistent" => {}
            "z" | "Z" => return Err(bad("SELinux relabeling (z, Z) is not supported")),
            "shared" | "rshared" | "slave" | "rslave" | "private" | "rprivate" => {
                return Err(bad("mount propagation options are not supported"));
            }
            other => return Err(bad(&format!("unknown option {other:?}"))),
        }
    }
    let Some(source) = source else {
        return Ok(MountRequest::Anonymous { target, access });
    };
    if source.is_empty() {
        return Err(bad("the source is empty"));
    }
    if source.starts_with('/') || source.starts_with('.') {
        if nocopy {
            return Err(bad("nocopy applies to volumes, not host paths"));
        }
        return Ok(MountRequest::Bind {
            source: source.to_string(),
            target,
            access,
        });
    }
    check_name(source)?;
    Ok(MountRequest::Named {
        name: source.to_string(),
        target,
        access,
        nocopy,
    })
}

/// The tmpfs flags Docker sets unless asked otherwise, with the option that undoes each.
const TMPFS_DEFAULTS: [(&str, &str); 3] =
    [("nosuid", "suid"), ("nodev", "dev"), ("noexec", "exec")];

/// A `--tmpfs` value: `TARGET[:OPTIONS]`.
pub fn parse_tmpfs_arg(s: &str) -> Result<MountRequest> {
    let (target, options) = s.split_once(':').unwrap_or((s, ""));
    let target = normalize_target(target)?;
    let mut user = Vec::new();
    for option in options.split(',').filter(|o| !o.is_empty()) {
        let valid = match option.split_once('=') {
            Some((key, value)) => {
                matches!(key, "size" | "mode" | "nr_inodes" | "uid" | "gid") && !value.is_empty()
            }
            None => matches!(
                option,
                "ro" | "rw" | "exec" | "noexec" | "suid" | "nosuid" | "dev" | "nodev"
            ),
        };
        if !valid {
            return Err(invalid(
                format!("invalid tmpfs option {option:?}"),
                "allowed: size=, mode=, nr_inodes=, uid=, gid=, ro, rw, exec, noexec, suid, nosuid, dev, nodev",
                "write it like --tmpfs /run:size=64m,mode=1777",
            ));
        }
        user.push(option);
    }
    let mut all: Vec<&str> = TMPFS_DEFAULTS
        .iter()
        .filter(|(_, undo)| !user.contains(undo))
        .map(|(flag, _)| *flag)
        .collect();
    for option in user {
        if !all.contains(&option) {
            all.push(option);
        }
    }
    Ok(MountRequest::Tmpfs {
        target,
        options: all.join(","),
    })
}

/// Docker's volume name rule: a letter or digit, then letters, digits, `_`, `.` or `-`;
/// at least two characters.
pub fn check_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid = bytes.len() >= 2
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if valid {
        return Ok(());
    }
    Err(invalid(
        format!("invalid volume name {name:?}"),
        "a volume name starts with a letter or digit and has only letters, digits, _, . and -",
        "choose a name like data or app_cache",
    ))
}

/// A mount target: an absolute path, not `/`, without `..`, and not in `/proc` or `/sys`
/// (the kernel's views there must stay as podbag set them up).
pub fn check_target(target: &str) -> Result<()> {
    let bad = |why: &str| {
        invalid(
            format!("invalid mount target {target:?}"),
            why,
            "give an absolute path inside the container, such as /data",
        )
    };
    if !target.starts_with('/') {
        return Err(bad("it is not an absolute path"));
    }
    let parts: Vec<&str> = target.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(bad("the container's root cannot be a mount target"));
    }
    if parts.contains(&"..") {
        return Err(bad("it contains .."));
    }
    if matches!(parts[0], "proc" | "sys") {
        return Err(bad("/proc and /sys cannot hold mounts"));
    }
    Ok(())
}

/// A checked target without repeated or trailing slashes.
fn normalize_target(target: &str) -> Result<String> {
    check_target(target)?;
    let parts: Vec<&str> = target
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    Ok(format!("/{}", parts.join("/")))
}

/// The requests plus an anonymous volume for each of the image's `Volumes` that no request
/// mounts over. Two requests on one target are an error.
pub fn with_image_volumes(
    requests: Vec<MountRequest>,
    image_volumes: &[String],
) -> Result<Vec<MountRequest>> {
    let mut all = Vec::with_capacity(requests.len() + image_volumes.len());
    for mut request in requests {
        let target = normalize_target(request.target())?;
        if all.iter().any(|r: &MountRequest| r.target() == target) {
            return Err(invalid(
                format!("two mounts on {target}"),
                "each container path can hold one mount",
                "remove one of the -v or --tmpfs options for that path",
            ));
        }
        *request.target_mut() = target;
        all.push(request);
    }
    for volume in image_volumes {
        let target = normalize_target(volume).map_err(|mut e| {
            e.what = format!("the image declares a volume that cannot be mounted: {volume}");
            e
        })?;
        if !all.iter().any(|r| r.target() == target) {
            all.push(MountRequest::Anonymous {
                target,
                access: Access::ReadWrite,
            });
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn dirs_in(tmp: &Path) -> Dirs {
        Dirs {
            data: tmp.join("data"),
            runtime: tmp.join("run"),
            config: tmp.join("cfg"),
        }
    }

    #[test]
    fn a_volume_keeps_its_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let labels: crate::network::Labels = [("a", "b"), ("empty", "")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let (made, new) = create_labeled_in(&dirs, Some("db"), false, labels.clone()).unwrap();
        assert!(new);
        assert_eq!(made.labels, labels);
        assert_eq!(get_in(&dirs, "db").unwrap().labels, labels);
        // A volume that exists is returned as it is.
        let other: crate::network::Labels =
            [("x".to_string(), "y".to_string())].into_iter().collect();
        let (again, new) = create_labeled_in(&dirs, Some("db"), false, other).unwrap();
        assert!(!new);
        assert_eq!(again.labels, labels);
        assert!(
            create_in(&dirs, Some("plain"), false)
                .unwrap()
                .0
                .labels
                .is_empty()
        );
        // Only the volumes that the caller picks are pruned.
        let (gone, _) = prune_matching_in(&dirs, true, &|v| v.labels.contains_key("a")).unwrap();
        assert_eq!(gone, ["db"]);
        assert!(get_in(&dirs, "plain").is_ok());
    }

    #[test]
    fn records_without_labels_load() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, Some("old"), false).unwrap();
        // As an earlier version wrote it.
        let path = dirs.volume_dir("old").join(META);
        let mut meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(meta.as_object_mut().unwrap().remove("labels").is_some());
        std::fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
        let old = get_in(&dirs, "old").unwrap();
        assert!(old.labels.is_empty());
        assert_eq!(old.name, "old");
    }

    #[test]
    fn create_list_get_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let (v, new) = create_in(&dirs, Some("db"), false).unwrap();
        assert!(new);
        assert_eq!(v.name, "db");
        assert!(Path::new(&v.mountpoint).is_dir());
        assert!(
            v.mountpoint.ends_with("volumes/db/data"),
            "{}",
            v.mountpoint
        );
        let (again, new) = create_in(&dirs, Some("db"), false).unwrap();
        assert!(!new);
        assert_eq!(again, v);
        let (anon, _) = create_in(&dirs, None, true).unwrap();
        assert_eq!(anon.name.len(), 64);
        assert!(anon.anonymous);
        assert_eq!(list_in(&dirs).unwrap().len(), 2);
        assert_eq!(get_in(&dirs, "db").unwrap(), v);
        remove_in(&dirs, "db", false).unwrap();
        assert_eq!(get_in(&dirs, "db").unwrap_err().kind, ErrorKind::NotFound);
        assert_eq!(
            remove_in(&dirs, "db", false).unwrap_err().kind,
            ErrorKind::NotFound
        );
        remove_in(&dirs, "db", true).unwrap();
        assert_eq!(list_in(&dirs).unwrap(), [anon]);
    }

    // Copied-up files keep their set-user-ID bits; other host users must not reach them.
    #[cfg(unix)]
    #[test]
    fn volume_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, Some("db"), false).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dirs.data.join("volumes")), 0o700);
        assert_eq!(mode(&dirs.data), 0o700);
    }

    fn user_of(dirs: &Dirs, volume: &str) {
        let mut spec = crate::spec::spec_fixture();
        spec.name = "web".into();
        spec.mounts = vec![Mount {
            source: MountSource::Volume {
                name: volume.into(),
                anonymous: false,
                path: String::new(),
            },
            target: "/data".into(),
            read_only: false,
            copy_up: false,
        }];
        crate::state::create(dirs, &spec).unwrap();
    }

    #[test]
    fn prune_takes_unused_anonymous_volumes_or_all_unused() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, Some("named"), false).unwrap();
        let (anon, _) = create_in(&dirs, None, true).unwrap();
        create_in(&dirs, Some("busy"), false).unwrap();
        std::fs::write(dirs.volume_dir(&anon.name).join("data/f"), b"12345").unwrap();
        user_of(&dirs, "busy");
        let (gone, bytes) = prune_in(&dirs, false).unwrap();
        assert_eq!(gone, [anon.name.clone()]);
        assert!(bytes >= 5, "{bytes}");
        let (gone, _) = prune_in(&dirs, true).unwrap();
        assert_eq!(gone, ["named"]);
        let left: Vec<String> = list_in(&dirs)
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(left, ["busy"]);
    }

    #[test]
    fn volume_in_use_is_not_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        create_in(&dirs, Some("db"), false).unwrap();
        let mut spec = crate::spec::spec_fixture();
        spec.name = "web".into();
        spec.mounts = vec![Mount {
            source: MountSource::Volume {
                name: "db".into(),
                anonymous: false,
                path: String::new(),
            },
            target: "/data".into(),
            read_only: false,
            copy_up: false,
        }];
        crate::state::create(&dirs, &spec).unwrap();
        for force in [false, true] {
            let e = remove_in(&dirs, "db", force).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Conflict);
            assert!(e.why.contains("web"), "{}", e.why);
        }
        assert_eq!(users_of(&dirs, "db").unwrap(), ["web"]);
        assert!(users_of(&dirs, "other").unwrap().is_empty());
    }

    #[test]
    fn invalid_names_are_rejected_before_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        assert_eq!(
            create_in(&dirs, Some("../x"), false).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(
            remove_in(&dirs, "../containers", true).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert!(!tmp.path().join("data/x").exists());
    }

    #[test]
    fn bind_named_and_anonymous_like_docker() {
        let rw = Access::ReadWrite;
        assert_eq!(
            parse_volume_arg("/host/a:/data").unwrap(),
            MountRequest::Bind {
                source: "/host/a".into(),
                target: "/data".into(),
                access: rw
            }
        );
        assert_eq!(
            parse_volume_arg("./src:/work:ro").unwrap(),
            MountRequest::Bind {
                source: "./src".into(),
                target: "/work".into(),
                access: Access::ReadOnly
            }
        );
        assert_eq!(
            parse_volume_arg("pgdata:/var/lib/pg").unwrap(),
            MountRequest::Named {
                name: "pgdata".into(),
                target: "/var/lib/pg".into(),
                access: rw,
                nocopy: false
            }
        );
        assert_eq!(
            parse_volume_arg("cache:/c:ro,nocopy").unwrap(),
            MountRequest::Named {
                name: "cache".into(),
                target: "/c".into(),
                access: Access::ReadOnly,
                nocopy: true
            }
        );
        assert_eq!(
            parse_volume_arg("/data").unwrap(),
            MountRequest::Anonymous {
                target: "/data".into(),
                access: rw
            }
        );
        assert!(parse_volume_arg("/a:/b:delegated").is_ok());
    }

    #[test]
    fn bad_volume_args_are_rejected() {
        for bad in [
            "",
            "a:/b:z",
            "a:/b:rshared",
            "ab:b",
            "x:/",
            "a:/b:/c:ro",
            "-bad:/x",
            "ab:/b:ro,rw2",
            "/a:/proc/x",
        ] {
            assert_eq!(
                parse_volume_arg(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn targets_are_checked() {
        for ok in ["/data", "/var/lib/x", "/dev/shm", "/procx"] {
            check_target(ok).unwrap();
        }
        for bad in [
            "",
            "data",
            "/",
            "/a/../b",
            "/proc",
            "/proc/sys",
            "/sys/x",
            "/sys",
        ] {
            assert_eq!(
                check_target(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn volume_names_follow_docker() {
        for ok in ["db", "a1", "my_vol.2-x"] {
            check_name(ok).unwrap();
        }
        for bad in ["a", "_x", ".x", "a/b", "a b", ""] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tmpfs_options_are_checked() {
        assert_eq!(
            parse_tmpfs_arg("/run").unwrap(),
            MountRequest::Tmpfs {
                target: "/run".into(),
                options: "nosuid,nodev,noexec".into()
            }
        );
        assert_eq!(
            parse_tmpfs_arg("/scratch:size=64m,mode=1777,exec").unwrap(),
            MountRequest::Tmpfs {
                target: "/scratch".into(),
                options: "nosuid,nodev,size=64m,mode=1777,exec".into()
            }
        );
        for bad in ["/x:foo=1", "relative", "/proc/x", "/x:size"] {
            assert!(parse_tmpfs_arg(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn image_volumes_become_anonymous_unless_overridden() {
        let user = vec![parse_volume_arg("mine:/data").unwrap()];
        let all = with_image_volumes(user, &["/data".into(), "/logs".into()]).unwrap();
        assert_eq!(all.len(), 2);
        assert!(matches!(&all[1], MountRequest::Anonymous { target, .. } if target == "/logs"));
        let dup = vec![
            parse_volume_arg("aa:/x").unwrap(),
            parse_volume_arg("bb:/x/").unwrap(),
        ];
        assert_eq!(
            with_image_volumes(dup, &[]).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
    }
}
