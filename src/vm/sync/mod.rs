//! Opt-in macOS synchronization between a VirtioFS directory and a guest volume.
//!
//! A persisted baseline lets either tree change independently while the VM is off.
//! Divergent edits keep both versions. Every displaced regular file has a recovery copy.

mod tree;

use crate::{ErrorKind, paths::Dirs, volume::Volume};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tree::{Entry, Fingerprints, Snapshot, Tree};

const LABEL: &str = "dev.podbag.sync.id";
const INTERVAL: Duration = Duration::from_secs(15);
const USAGE: &str = "usage: podbag vm sync list|add NAME PATH|now NAME|remove NAME";
static SCANS: Mutex<BTreeMap<String, ScanCache>> = Mutex::new(BTreeMap::new());

#[derive(Default)]
struct ScanCache {
    host: Fingerprints,
    guest: Fingerprints,
}

#[derive(Debug, Deserialize, Serialize)]
struct Share {
    name: String,
    source: PathBuf,
    volume: String,
    id: String,
    #[serde(default)]
    baseline: Snapshot,
    #[serde(default)]
    conflicts: Vec<Conflict>,
    last_error: Option<String>,
    checked: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Conflict {
    path: String,
    reason: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status<'a> {
    name: &'a str,
    source: &'a Path,
    volume: &'a str,
    conflicts: &'a [Conflict],
    last_error: Option<&'a str>,
    checked: Option<u64>,
}

impl Share {
    fn status(&self) -> Status<'_> {
        Status {
            name: &self.name,
            source: &self.source,
            volume: &self.volume,
            conflicts: &self.conflicts,
            last_error: self.last_error.as_deref(),
            checked: self.checked,
        }
    }
}

pub(crate) fn is_cache_volume(volume: &Volume) -> bool {
    volume.labels.contains_key(LABEL)
}

pub(super) fn command(args: &[String]) -> Result<String, String> {
    let dirs = crate::paths::dirs().map_err(|e| e.to_string())?;
    let registry = Registry::new(&dirs);
    let mut cache = SCANS.lock().unwrap_or_else(|p| p.into_inner());
    let serialize = |share: &Share| serde_json::to_string_pretty(&share.status());
    let result = match args {
        [cmd] if cmd == "list" => {
            let shares = registry.list()?;
            let views: Vec<_> = shares.iter().map(Share::status).collect();
            serde_json::to_string_pretty(&views)
        }
        [cmd, name, path] if cmd == "add" => {
            let mut share = registry.add(name, Path::new(path))?;
            registry.update(&mut share, &mut cache)?;
            serialize(&share)
        }
        [cmd, name] if cmd == "now" => {
            let mut share = registry.get(name)?;
            registry.update(&mut share, &mut cache)?;
            serialize(&share)
        }
        [cmd, name] if cmd == "remove" => {
            let share = registry.get(name)?;
            std::fs::remove_file(registry.path(name)?).map_err(|e| e.to_string())?;
            registry.sync_directory().map_err(|e| e.to_string())?;
            cache.remove(&share.id);
            serialize(&share)
        }
        _ => return Err(USAGE.into()),
    };
    result.map_err(|e| e.to_string())
}

pub(super) fn watch(dirs: Dirs) {
    let registry = Registry::new(&dirs);
    loop {
        {
            let mut cache = SCANS.lock().unwrap_or_else(|p| p.into_inner());
            match registry.list() {
                Ok(shares) => {
                    for mut share in shares {
                        if let Err(e) = registry.update(&mut share, &mut cache) {
                            eprintln!(
                                "podbag: cannot record synchronized share {}: {e}",
                                share.name
                            );
                        }
                    }
                }
                Err(e) => eprintln!("podbag: cannot read synchronized shares: {e}"),
            }
        }
        std::thread::sleep(INTERVAL);
    }
}

struct Registry<'a> {
    dirs: &'a Dirs,
    root: PathBuf,
}

impl<'a> Registry<'a> {
    fn new(dirs: &'a Dirs) -> Self {
        Self {
            dirs,
            root: dirs.data.join("synchronized-shares"),
        }
    }

    fn path(&self, name: &str) -> Result<PathBuf, String> {
        crate::container::check_name(name).map_err(|e| e.to_string())?;
        if name.len() > 64 {
            return Err("a synchronized share name is at most 64 characters".into());
        }
        Ok(self.root.join(format!("{name}.json")))
    }

    fn list(&self) -> Result<Vec<Share>, String> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.to_string()),
        };
        let mut shares = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.path().extension().and_then(|ext| ext.to_str()) == Some("json") {
                shares.push(read_share(&entry.path())?);
            }
        }
        shares.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(shares)
    }

    fn get(&self, name: &str) -> Result<Share, String> {
        read_share(&self.path(name)?)
    }

    fn add(&self, name: &str, source: &Path) -> Result<Share, String> {
        let path = self.path(name)?;
        if path.exists() {
            return Err(format!("synchronized share {name} already exists"));
        }
        if !valid_source(source) {
            return Err("choose a project folder under /Users".into());
        }
        // The Mac supplies a canonical path. Refuse it if a folder became a link.
        Tree::open(source).map_err(|e| format!("cannot open {}: {e}", source.display()))?;
        for share in self.list()? {
            if source.starts_with(&share.source) || share.source.starts_with(source) {
                return Err(format!(
                    "the source overlaps synchronized share {}",
                    share.name
                ));
            }
        }
        let volume = format!("podbag-sync-{name}");
        match crate::volume::get_in(self.dirs, &volume) {
            Ok(_) => return Err(format!("volume {volume} already exists")),
            Err(e) if e.kind == ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        let id = crate::id::new_id().map_err(|e| e.to_string())?;
        let labels = [(LABEL.into(), id.clone())].into();
        let created =
            Volume::create_in_store(self.dirs, Some(&volume), labels).map_err(|e| e.to_string())?;
        if created.labels.get(LABEL) != Some(&id) {
            return Err(format!("volume {volume} already exists"));
        }
        let share = Share {
            name: name.into(),
            source: source.to_owned(),
            volume,
            id,
            baseline: Snapshot::new(),
            conflicts: Vec::new(),
            last_error: None,
            checked: None,
        };
        self.save(&share)?;
        Ok(share)
    }

    fn update(
        &self,
        share: &mut Share,
        cache: &mut BTreeMap<String, ScanCache>,
    ) -> Result<(), String> {
        let scans = cache.entry(share.id.clone()).or_default();
        share.last_error = self.sync(share, scans).err();
        share.checked = Some(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        self.save(share)
    }

    fn sync(&self, share: &mut Share, scans: &mut ScanCache) -> Result<(), String> {
        if !valid_source(&share.source) {
            return Err("a synchronized source must be a project folder under /Users".into());
        }
        // A removed cache must never be interpreted as a user deleting all its files.
        let _volume_lock = crate::volume::lock_shared(self.dirs).map_err(|e| e.to_string())?;
        let volume = crate::volume::get_in(self.dirs, &share.volume).map_err(|e| e.to_string())?;
        if volume.labels.get(LABEL) != Some(&share.id) {
            return Err(format!("the cache volume {} was replaced", share.volume));
        }
        let host =
            Tree::open(&share.source).map_err(|e| format!("cannot open host folder: {e}"))?;
        let guest = Tree::open(Path::new(&volume.mountpoint))
            .map_err(|e| format!("cannot open cache volume: {e}"))?;
        let (baseline, conflicts) =
            synchronize(&share.baseline, &host, &guest, scans).map_err(|e| e.to_string())?;
        share.baseline = baseline;
        share.conflicts = conflicts;
        Ok(())
    }

    fn save(&self, share: &Share) -> Result<(), String> {
        let path = self.path(&share.name)?;
        let save = || -> io::Result<()> {
            let mut builder = crate::store::perm::private_dir_builder();
            builder.recursive(true).create(&self.root)?;
            let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
            serde_json::to_writer(&mut file, share)?;
            file.write_all(b"\n")?;
            file.as_file().sync_all()?;
            file.persist(&path)?;
            self.sync_directory()
        };
        save().map_err(|e| format!("{}: {e}", path.display()))
    }

    fn sync_directory(&self) -> io::Result<()> {
        File::open(&self.root)?.sync_all()
    }
}

fn valid_source(source: &Path) -> bool {
    source.strip_prefix("/Users").is_ok_and(|relative| {
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
    })
}

fn read_share(path: &Path) -> Result<Share, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_reader(file).map_err(|e| format!("{}: {e}", path.display()))
}

struct Change<'a> {
    path: &'a str,
    target: &'a Tree,
    source: &'a Tree,
    expected: Option<&'a Entry>,
    desired: Option<&'a Entry>,
}

fn synchronize(
    baseline: &Snapshot,
    host: &Tree,
    guest: &Tree,
    scans: &mut ScanCache,
) -> io::Result<(Snapshot, Vec<Conflict>)> {
    let hosts = host.snapshot(&mut scans.host)?;
    let guests = guest.snapshot(&mut scans.guest)?;
    let paths: BTreeSet<_> = baseline
        .keys()
        .chain(hosts.keys())
        .chain(guests.keys())
        .collect();
    let mut next = baseline.clone();
    let mut conflicts = Vec::new();
    let mut blocked = Vec::<&Path>::new();
    let mut changes = Vec::new();
    for path in paths {
        let location = Path::new(path);
        if blocked.iter().any(|root| location.starts_with(root)) {
            continue;
        }
        let before = baseline.get(path);
        let local = hosts.get(path);
        let cached = guests.get(path);
        if matches!(local, Some(Entry::Unsupported)) || matches!(cached, Some(Entry::Unsupported)) {
            report(
                &mut conflicts,
                path,
                "only regular files and directories are synchronized",
            );
            blocked.push(location);
            continue;
        }
        if local == cached {
            record(&mut next, path, local);
            continue;
        }
        let (target, source, expected, desired) = if local == before {
            (host, guest, local, cached)
        } else if cached == before {
            (guest, host, cached, local)
        } else {
            report(&mut conflicts, path, "both sides changed");
            blocked.push(location);
            continue;
        };
        if let (Some(expected), Some(desired)) = (expected, desired)
            && std::mem::discriminant(expected) != std::mem::discriminant(desired)
        {
            report(&mut conflicts, path, "the file type changed");
            blocked.push(location);
            continue;
        }
        changes.push(Change {
            path,
            target,
            source,
            expected,
            desired,
        });
    }
    // Make parents first. Remove directories after their children.
    changes.sort_by_key(|change| {
        let depth = change.path.bytes().filter(|b| *b == b'/').count();
        match (change.expected, change.desired) {
            (Some(Entry::Directory(_)), None) => (2, usize::MAX - depth),
            (_, Some(Entry::Directory(_))) => (0, depth),
            _ => (1, depth),
        }
    });
    for change in changes {
        if let Err(error) =
            change
                .target
                .apply(change.path, change.expected, change.source, change.desired)
        {
            report(&mut conflicts, change.path, &error.to_string());
        } else {
            record(&mut next, change.path, change.desired);
        }
    }
    Ok((next, conflicts))
}

fn record(baseline: &mut Snapshot, path: &str, value: Option<&Entry>) {
    if let Some(entry) = value {
        baseline.insert(path.into(), entry.clone());
    } else {
        baseline.remove(path);
    }
}

fn report(conflicts: &mut Vec<Conflict>, path: &str, reason: &str) {
    conflicts.push(Conflict {
        path: path.into(),
        reason: reason.into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Pair {
        host: tempfile::TempDir,
        guest: tempfile::TempDir,
    }

    impl Pair {
        fn new() -> Self {
            Self {
                host: tempfile::tempdir().unwrap(),
                guest: tempfile::tempdir().unwrap(),
            }
        }

        fn sync(&self, baseline: &Snapshot) -> (Snapshot, Vec<Conflict>) {
            let left = Tree::open(self.host.path()).unwrap();
            let right = Tree::open(self.guest.path()).unwrap();
            synchronize(baseline, &left, &right, &mut ScanCache::default()).unwrap()
        }
    }

    #[test]
    fn rename_delete_and_permissions_survive_reconciliation_after_reload() {
        let pair = Pair::new();
        let host = pair.host.path();
        let guest = pair.guest.path();
        std::fs::create_dir(host.join("sub")).unwrap();
        let file = host.join("sub").join("file");
        std::fs::write(&file, "first").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
        let (base, conflicts) = pair.sync(&Snapshot::new());
        assert!(conflicts.is_empty(), "{conflicts:?}");
        let base: Snapshot = serde_json::from_slice(&serde_json::to_vec(&base).unwrap()).unwrap();
        std::fs::rename(guest.join("sub"), guest.join("other")).unwrap();
        let (base, conflicts) = pair.sync(&base);
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(!host.join("sub").exists());
        let moved = host.join("other/file");
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "first");
        assert_eq!(
            moved.metadata().unwrap().permissions().mode() & 0o777,
            0o750
        );
        let recovery = std::fs::read_dir(host.join(".podbag-sync-recovery"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(recovery.join("file")).unwrap(),
            "first"
        );
        assert_eq!(
            std::fs::read_to_string(recovery.join("path.txt")).unwrap(),
            "sub/file\n"
        );
        std::fs::remove_file(moved).unwrap();
        let (_, conflicts) = pair.sync(&base);
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(!guest.join("other/file").exists());
    }

    #[test]
    fn divergent_edits_and_links_are_preserved() {
        let pair = Pair::new();
        let host = pair.host.path().join("app");
        let guest = pair.guest.path().join("app");
        std::fs::write(&host, "common").unwrap();
        let (base, _) = pair.sync(&Snapshot::new());
        std::fs::write(&host, "host edit").unwrap();
        std::fs::write(&guest, "guest edit").unwrap();
        symlink("/etc/passwd", pair.host.path().join("link")).unwrap();
        let (next, conflicts) = pair.sync(&base);
        assert_eq!(base, next);
        assert_eq!(conflicts.len(), 2, "{conflicts:?}");
        assert_eq!(std::fs::read_to_string(host).unwrap(), "host edit");
        assert_eq!(std::fs::read_to_string(guest).unwrap(), "guest edit");
        assert!(!pair.guest.path().join("link").exists());
    }

    #[test]
    fn a_concurrent_target_change_is_kept() {
        let pair = Pair::new();
        let host = pair.host.path().join("file");
        let guest = pair.guest.path().join("file");
        std::fs::write(&host, "original").unwrap();
        let left = Tree::open(pair.host.path()).unwrap();
        let right = Tree::open(pair.guest.path()).unwrap();
        let stale = left.snapshot(&mut Fingerprints::default()).unwrap();
        std::fs::write(&host, "newer version").unwrap();
        let expected = stale.get("file").unwrap();
        assert!(left.apply("file", Some(expected), &right, None).is_err());
        assert_eq!(std::fs::read_to_string(host).unwrap(), "newer version");
        assert!(!guest.exists());
    }
}
