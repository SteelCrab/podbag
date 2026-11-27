//! A share tree. Directory descriptors keep each operation inside the selected tree.

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const RECOVERY: &str = ".podbag-sync-recovery";
const STAGE_PREFIX: &str = ".podbag-sync-tmp-";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Entry {
    Directory(u32),
    File(u32, String),
    Unsupported,
}

pub(super) type Snapshot = BTreeMap<String, Entry>;

#[derive(Default)]
pub(super) struct Fingerprints(BTreeMap<String, (Stamp, Entry)>);

#[derive(PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: i64,
    mode: u32,
    changed: (i64, u32),
    modified: (i64, u32),
}

impl From<&fs::Stat> for Stamp {
    fn from(meta: &fs::Stat) -> Self {
        Self {
            device: meta.st_dev,
            inode: meta.st_ino,
            size: meta.st_size,
            mode: meta.st_mode,
            changed: (meta.st_ctime, meta.st_ctime_nsec as u32),
            modified: (meta.st_mtime, meta.st_mtime_nsec as u32),
        }
    }
}

pub(super) struct Tree(OwnedFd);

impl Tree {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let root = fs::open("/", OFlags::PATH | OFlags::CLOEXEC, Mode::empty())?;
        let relative = path.strip_prefix("/").map_err(io::Error::other)?;
        Ok(Self(open(&root, relative, OFlags::DIRECTORY)?))
    }

    pub(super) fn snapshot(&self, previous: &mut Fingerprints) -> io::Result<Snapshot> {
        let mut out = Snapshot::new();
        let mut current = Fingerprints::default();
        self.scan("", &mut out, previous, &mut current)?;
        *previous = current;
        Ok(out)
    }

    fn scan(
        &self,
        prefix: &str,
        out: &mut Snapshot,
        previous: &Fingerprints,
        current: &mut Fingerprints,
    ) -> io::Result<()> {
        let dir = open(&self.0, Path::new(prefix), OFlags::DIRECTORY)?;
        for entry in std::fs::read_dir(fd_path(&dir))? {
            let name = entry?.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| io::Error::other("a filename is not UTF-8"))?;
            if name == RECOVERY || name.starts_with(STAGE_PREFIX) {
                continue;
            }
            let key = if prefix.is_empty() {
                name.into()
            } else {
                format!("{prefix}/{name}")
            };
            let Some(meta) = lookup(&dir, OsStr::new(name))? else {
                continue;
            };
            let entry = match FileType::from_raw_mode(meta.st_mode) {
                FileType::Directory => {
                    self.scan(&key, out, previous, current)?;
                    Entry::Directory(meta.st_mode & 0o777)
                }
                FileType::RegularFile => {
                    let stamp = Stamp::from(&meta);
                    let entry = match previous.0.get(&key) {
                        Some((old, entry)) if old == &stamp => entry.clone(),
                        _ => match read_fingerprint(&dir, OsStr::new(name)) {
                            Ok(file) => file,
                            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                            Err(e) => return Err(e),
                        },
                    };
                    current.0.insert(key.clone(), (stamp, entry.clone()));
                    entry
                }
                _ => Entry::Unsupported,
            };
            out.insert(key, entry);
        }
        Ok(())
    }

    fn parent<'a>(&self, path: &'a str) -> io::Result<(OwnedFd, &'a str)> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        Ok((open(&self.0, Path::new(parent), OFlags::DIRECTORY)?, name))
    }

    pub(super) fn apply(
        &self,
        path: &str,
        expected: Option<&Entry>,
        source: &Tree,
        desired: Option<&Entry>,
    ) -> io::Result<()> {
        let actual = match source.parent(path) {
            Ok((parent, name)) => inspect(&parent, OsStr::new(name))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if actual.as_ref() != desired {
            return Err(changed());
        }
        let (parent, name) = self.parent(path)?;
        if inspect(&parent, OsStr::new(name))?.as_ref() != expected {
            return Err(changed());
        }
        match (expected, desired) {
            (None | Some(Entry::Directory(_)), Some(Entry::Directory(mode))) => {
                if expected.is_none() {
                    fs::mkdirat(&parent, name, Mode::from_raw_mode(*mode | 0o700))?;
                }
                let dir = open(&parent, Path::new(name), OFlags::DIRECTORY)?;
                fs::fchmod(dir, Mode::from_raw_mode(*mode))?;
            }
            (Some(Entry::Directory(_)), None) => {
                // A concurrent writer can make the directory non-empty. Keep its files.
                fs::unlinkat(&parent, name, AtFlags::REMOVEDIR)?;
            }
            (None | Some(Entry::File(..)), Some(Entry::File(mode, expected_hash))) => {
                let (from, from_name) = source.parent(path)?;
                let mut from = File::from(phase(
                    open_regular(&from, OsStr::new(from_name)),
                    "open source",
                )?);
                let mut stage = tempfile::Builder::new()
                    .prefix(STAGE_PREFIX)
                    .tempfile_in(fd_path(&parent))?;
                let hash = transfer(&mut from, stage.as_file_mut())?;
                if &hash != expected_hash || from.metadata()?.permissions().mode() & 0o777 != *mode
                {
                    return Err(changed());
                }
                stage
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(*mode))?;
                stage.as_file().sync_all()?;
                if let Some(expected) = expected {
                    self.archive(path, &parent, name, expected)?;
                }
                // Never replace a path another writer created since the last check.
                phase(
                    fs::linkat(fs::CWD, stage.path(), &parent, name, AtFlags::empty()),
                    "publish staged file",
                )?;
            }
            (Some(expected @ Entry::File(..)), None) => {
                self.archive(path, &parent, name, expected)?;
            }
            (None, None) => {}
            _ => return Err(io::Error::other("file type changed")),
        }
        File::from(parent).sync_all()?;
        Ok(())
    }

    fn archive(
        &self,
        path: &str,
        parent: &OwnedFd,
        name: &str,
        expected: &Entry,
    ) -> io::Result<()> {
        let mode = Mode::from_raw_mode(0o700);
        match fs::mkdirat(&self.0, RECOVERY, mode) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        let recovery = open(&self.0, Path::new(RECOVERY), OFlags::DIRECTORY)?;
        let backup = tempfile::Builder::new()
            .prefix("previous-")
            .tempdir_in(fd_path(&recovery))?;
        let destination = backup.path().join("path.txt");
        let mut record = File::create(destination)?;
        writeln!(&mut record, "{path}")?;
        record.sync_all()?;
        let dir = open(
            &recovery,
            Path::new(backup.path().file_name().unwrap()),
            OFlags::DIRECTORY,
        )?;
        // Persist the recovery directory before moving any file into it.
        File::from(dup(&dir)?).sync_all()?;
        File::from(dup(&recovery)?).sync_all()?;
        let _ = backup.keep();
        phase(
            fs::renameat(parent, name, &dir, "file"),
            "archive destination",
        )?;
        File::from(dup(&dir)?).sync_all()?;
        File::from(dup(parent)?).sync_all()?;
        if phase(inspect(&dir, OsStr::new("file")), "verify recovery copy")?.as_ref()
            != Some(expected)
        {
            // Put the displaced version back when the path is still vacant. The
            // recovery copy survives even if a concurrent writer already filled it.
            let _ = fs::linkat(&dir, "file", parent, name, AtFlags::empty());
            return Err(changed());
        }
        Ok(())
    }
}

fn open(fd: &OwnedFd, relative: &Path, flags: OFlags) -> io::Result<OwnedFd> {
    let relative = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    Ok(fs::openat2(
        fd,
        relative,
        flags | OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )?)
}

fn open_regular(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let fd = open(parent, Path::new(name), OFlags::empty())?;
    if FileType::from_raw_mode(fs::fstat(&fd)?.st_mode) != FileType::RegularFile {
        return Err(changed());
    }
    Ok(fd)
}

fn fd_path(fd: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

fn dup(fd: &OwnedFd) -> io::Result<OwnedFd> {
    Ok(rustix::io::dup(fd)?)
}

fn lookup(parent: &OwnedFd, name: &OsStr) -> io::Result<Option<fs::Stat>> {
    match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(meta) => Ok(Some(meta)),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn inspect(parent: &OwnedFd, name: &OsStr) -> io::Result<Option<Entry>> {
    let Some(meta) = lookup(parent, name)? else {
        return Ok(None);
    };
    Ok(Some(match FileType::from_raw_mode(meta.st_mode) {
        FileType::Directory => Entry::Directory(meta.st_mode & 0o777),
        FileType::RegularFile => match read_fingerprint(parent, name) {
            Ok(file) => file,
            // Some shared files disappear between the stat and the open after a Mac edit.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        },
        _ => Entry::Unsupported,
    }))
}

fn read_fingerprint(parent: &OwnedFd, name: &OsStr) -> io::Result<Entry> {
    let mut file = File::from(open_regular(parent, name)?);
    let before = file.metadata()?;
    let hash = transfer(&mut file, &mut io::sink())?;
    let after = file.metadata()?;
    if before.modified()? != after.modified()?
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || before.len() != after.len()
        || before.permissions().mode() != after.permissions().mode()
    {
        return Err(changed());
    }
    Ok(Entry::File(after.permissions().mode() & 0o777, hash))
}

fn transfer(from: &mut File, to: &mut impl Write) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect());
        }
        hash.update(&buf[..n]);
        to.write_all(&buf[..n])?;
    }
}

fn phase<T, E: Into<io::Error>>(result: Result<T, E>, message: &str) -> io::Result<T> {
    result.map_err(|error| {
        let error = error.into();
        io::Error::new(error.kind(), format!("{message}: {error}"))
    })
}

fn changed() -> io::Error {
    io::Error::other("the file changed during synchronization")
}
