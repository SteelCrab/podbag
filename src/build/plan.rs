//! `COPY` and `ADD` as a plan: the tar entries of the new layer, with where each file's
//! content is read from. The build works the plan out (it needs the image's tree and the
//! context); the store helper writes it as a layer, as the store's owner who can read
//! every stored file.

use super::cache::SourceHash;
use super::ignore::{self, Ignore};
use crate::store::layer::{Entry, EntryKind, unhex};
use crate::store::tree::Tree;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// One entry of the new layer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    /// Relative to the image root.
    pub path: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub kind: PlanKind,
    /// (name, hex value)
    #[serde(default)]
    pub xattrs: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PlanKind {
    Dir,
    /// `size` bytes of the file at `from`.
    File {
        from: PathBuf,
        size: u64,
    },
    Symlink {
        target: String,
    },
    Fifo,
    Char {
        major: u32,
        minor: u32,
    },
    Block {
        major: u32,
        minor: u32,
    },
    /// The tar archive at `from` (maybe compressed), unpacked into `path` (ADD). `owner`
    /// replaces the archive's owners.
    Unpack {
        from: PathBuf,
        owner: Option<(u32, u32)>,
    },
}

/// One thing to copy, found in a source: `rel` is its path below the matched source
/// (empty for the source itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub rel: String,
    pub kind: PlanKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub xattrs: Vec<(String, String)>,
    /// What the cache hashes for the content: a digest, or a link target.
    pub content: String,
}

/// Where `COPY` reads from.
pub enum Source<'a> {
    /// The build context, minus what `.dockerignore` leaves out.
    Context { root: &'a Path, ignore: &'a Ignore },
    /// An image or an earlier stage: its tree, and how to find its stored files.
    Image {
        tree: &'a Tree,
        object: &'a dyn Fn(&str) -> PathBuf,
    },
}

fn fail(why: String) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "cannot copy files into the image",
        why,
        "check the source paths and the .dockerignore file",
    )
}

impl Source<'_> {
    /// The paths a source pattern names, relative to the source's root. A pattern without
    /// wildcards must exist; one with wildcards must match something.
    pub fn find(&self, pattern: &str) -> Result<Vec<String>> {
        let cleaned = ignore::clean(&format!("/{pattern}"));
        let cleaned = cleaned.trim_start_matches('/').to_string();
        if !ignore::has_wildcards(&cleaned) {
            let resolved = self.resolve(&cleaned)?;
            return match resolved {
                Some(p) => Ok(vec![p]),
                None => Err(fail(format!(
                    "{pattern}: not found{}",
                    match self {
                        Source::Context { .. } =>
                            " in the build context (or left out by .dockerignore)",
                        Source::Image { .. } => " in the image",
                    }
                ))),
            };
        }
        let mut found = Vec::new();
        match self {
            Source::Context { root, ignore } => walk(root, "", ignore, &mut |rel, _| {
                if ignore::glob(&cleaned, rel) {
                    found.push(rel.to_string());
                }
            })?,
            Source::Image { tree, .. } => {
                found.extend(
                    tree.entries
                        .keys()
                        .filter(|p| ignore::glob(&cleaned, p))
                        .cloned(),
                );
            }
        }
        // A match inside another match is copied with it.
        found.sort();
        let mut out: Vec<String> = Vec::new();
        for p in found {
            if !out.iter().any(|o| p.starts_with(&format!("{o}/"))) {
                out.push(p);
            }
        }
        if out.is_empty() {
            return Err(fail(format!("no source files match {pattern}")));
        }
        Ok(out)
    }

    /// The path with symbolic links followed (inside the source), or None when missing.
    fn resolve(&self, path: &str) -> Result<Option<String>> {
        match self {
            Source::Context { root, ignore } => {
                let (real, true) = resolve_with(path, &|p| {
                    let meta = std::fs::symlink_metadata(root.join(p)).ok()?;
                    Some(if meta.file_type().is_symlink() {
                        Node::Link(
                            std::fs::read_link(root.join(p))
                                .ok()?
                                .to_string_lossy()
                                .into_owned(),
                        )
                    } else if meta.is_dir() {
                        Node::Dir
                    } else {
                        Node::Other
                    })
                })?
                else {
                    return Ok(None);
                };
                Ok((real.is_empty() || !ignore.excludes(&real)).then_some(real))
            }
            Source::Image { tree, .. } => {
                let (real, exists) = resolve_with(path, &|p| node_in(tree, p))?;
                Ok(exists.then_some(real))
            }
        }
    }

    /// Everything below a found path (which may be a file), in order, the path itself first.
    pub fn items(&self, path: &str) -> Result<Vec<Item>> {
        match self {
            Source::Context { root, ignore } => context_items(root, ignore, path),
            Source::Image { tree, object } => Ok(image_items(tree, *object, path)),
        }
    }
}

enum Node {
    Dir,
    Link(String),
    Other,
}

fn node_in(tree: &Tree, path: &str) -> Option<Node> {
    if path.is_empty() {
        return Some(Node::Dir);
    }
    tree.entries.get(path).map(|e| match &e.kind {
        EntryKind::Dir => Node::Dir,
        EntryKind::Symlink { target } => Node::Link(target.clone()),
        _ => Node::Other,
    })
}

/// Follow symbolic links in `path` (relative to a root that links cannot leave), like
/// Docker's FollowSymlinkInScope. Returns the real path, and whether it exists: from the
/// first missing part on, the rest is taken as written.
fn resolve_with(path: &str, node: &dyn Fn(&str) -> Option<Node>) -> Result<(String, bool)> {
    let mut todo: Vec<String> = path
        .split('/')
        .filter(|p| !p.is_empty())
        .rev()
        .map(String::from)
        .collect();
    let mut done: Vec<String> = Vec::new();
    let mut hops = 0;
    let mut exists = true;
    while let Some(part) = todo.pop() {
        match part.as_str() {
            "." => continue,
            ".." => {
                done.pop();
                continue;
            }
            _ => {}
        }
        done.push(part);
        if !exists {
            continue;
        }
        match node(&done.join("/")) {
            None => exists = false,
            Some(Node::Link(target)) => {
                hops += 1;
                if hops > 40 {
                    return Err(fail(format!("too many symbolic links in {path}")));
                }
                done.pop();
                if target.starts_with('/') {
                    done.clear();
                }
                todo.extend(
                    target
                        .split('/')
                        .filter(|p| !p.is_empty())
                        .rev()
                        .map(String::from),
                );
            }
            Some(_) => {}
        }
    }
    Ok((done.join("/"), exists))
}

fn walk(
    root: &Path,
    rel: &str,
    ignore: &Ignore,
    on: &mut dyn FnMut(&str, &std::fs::Metadata),
) -> Result<()> {
    let dir = root.join(rel);
    let list = std::fs::read_dir(&dir).map_err(|e| read_error(&dir, e))?;
    let mut names: Vec<String> = list
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let Ok(meta) = std::fs::symlink_metadata(root.join(&child)) else {
            continue;
        };
        let left_out = ignore.excludes(&child);
        if !left_out {
            on(&child, &meta);
        }
        if meta.is_dir() && (!left_out || ignore.has_exceptions()) {
            walk(root, &child, ignore, on)?;
        }
    }
    Ok(())
}

fn read_error(path: &Path, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot read {} in the build context", path.display()),
        &e,
        "check its permissions",
    )
}

#[cfg(unix)]
fn context_items(root: &Path, ignore: &Ignore, path: &str) -> Result<Vec<Item>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let item = |rel: &str, full: &Path, meta: &std::fs::Metadata| -> Result<Option<Item>> {
        let kind = meta.file_type();
        let (kind, content) = if kind.is_dir() {
            (PlanKind::Dir, String::new())
        } else if kind.is_symlink() {
            let target = std::fs::read_link(full).map_err(|e| read_error(full, e))?;
            let target = target.to_string_lossy().into_owned();
            (
                PlanKind::Symlink {
                    target: target.clone(),
                },
                target,
            )
        } else if kind.is_file() {
            (
                PlanKind::File {
                    from: full.to_path_buf(),
                    size: meta.len(),
                },
                file_digest(full)?,
            )
        } else if kind.is_fifo() {
            (PlanKind::Fifo, String::new())
        } else {
            // Sockets and devices are not copied from a context.
            return Ok(None);
        };
        Ok(Some(Item {
            rel: rel.to_string(),
            kind,
            mode: meta.mode() & 0o7777,
            uid: 0,
            gid: 0,
            xattrs: Vec::new(),
            content,
        }))
    };
    let top = root.join(path);
    let meta = std::fs::metadata(&top).map_err(|e| read_error(&top, e))?;
    let mut out: Vec<Item> = item("", &top, &meta)?.into_iter().collect();
    if meta.is_dir() {
        let mut err = None;
        walk(root, path, ignore, &mut |child, meta| {
            if err.is_some() {
                return;
            }
            let rel = child
                .strip_prefix(path)
                .unwrap_or(child)
                .trim_start_matches('/');
            match item(rel, &root.join(child), meta) {
                Ok(i) => out.extend(i),
                Err(e) => err = Some(e),
            }
        })?;
        if let Some(e) = err {
            return Err(e);
        }
    }
    Ok(out)
}

#[cfg(not(unix))]
fn context_items(_root: &Path, _ignore: &Ignore, _path: &str) -> Result<Vec<Item>> {
    Err(Error::unsupported_os())
}

fn image_items(tree: &Tree, object: &dyn Fn(&str) -> PathBuf, path: &str) -> Vec<Item> {
    let item = |rel: &str, e: &Entry| -> Item {
        let (kind, content) = match &e.kind {
            EntryKind::Dir => (PlanKind::Dir, String::new()),
            EntryKind::File { object: key, size } => (
                PlanKind::File {
                    from: object(key),
                    size: *size,
                },
                key.split('.').next().unwrap_or(key).to_string(),
            ),
            EntryKind::Symlink { target } => (
                PlanKind::Symlink {
                    target: target.clone(),
                },
                target.clone(),
            ),
            EntryKind::Char { major, minor } => (
                PlanKind::Char {
                    major: *major,
                    minor: *minor,
                },
                String::new(),
            ),
            EntryKind::Block { major, minor } => (
                PlanKind::Block {
                    major: *major,
                    minor: *minor,
                },
                String::new(),
            ),
            // Merged trees hold no hard links, whiteouts or markers.
            _ => (PlanKind::Fifo, String::new()),
        };
        Item {
            rel: rel.to_string(),
            kind,
            mode: e.mode,
            uid: e.uid,
            gid: e.gid,
            xattrs: e.xattrs.clone(),
            content,
        }
    };
    let root_dir = Entry {
        path: String::new(),
        kind: EntryKind::Dir,
        mode: 0o755,
        uid: 0,
        gid: 0,
        xattrs: Vec::new(),
    };
    let top = if path.is_empty() {
        Some(&root_dir)
    } else {
        tree.entries.get(path)
    };
    let Some(top) = top else {
        return Vec::new();
    };
    let mut out = vec![item("", top)];
    if top.kind == EntryKind::Dir {
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        for (p, e) in tree.entries.range(prefix.clone()..) {
            let Some(rel) = p.strip_prefix(&prefix) else {
                break;
            };
            out.push(item(rel, e));
        }
    }
    out
}

#[cfg(unix)]
fn file_digest(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).map_err(|e| read_error(path, e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| read_error(path, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(super::cache::hex(&h.finalize()))
}

/// How a `COPY` or `ADD` places its sources.
pub struct Placement<'a> {
    /// The destination as given, made absolute with the working directory.
    pub dest: String,
    /// The image's tree before the step.
    pub tree: &'a Tree,
    /// `--chown`, resolved to numbers.
    pub owner: Option<(u32, u32)>,
    /// `--chmod`.
    pub chmod: Option<u32>,
    /// More than one source: the destination must be a directory.
    pub many: bool,
}

/// What a step copies: entries of the new layer, and the digest of their content.
#[derive(Default, Debug)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
    hash: SourceHash,
    /// Directories this plan made, so each is made once.
    made: BTreeMap<String, ()>,
}

impl Plan {
    pub fn digest(self) -> String {
        self.hash.finish()
    }

    /// Place one found source with its items. `name` is the source's last path part.
    /// `unpack`: (ADD) the source is an archive to unpack into the destination.
    pub fn place(
        &mut self,
        at: &Placement<'_>,
        name: &str,
        items: Vec<Item>,
        unpack: bool,
    ) -> Result<()> {
        let ends_in_slash = at.dest.ends_with('/');
        if at.many && !ends_in_slash {
            return Err(fail(format!(
                "with more than one source, the destination must be a directory and end with /: {}",
                at.dest
            )));
        }
        let (dest, _) = resolve_with(at.dest.trim_start_matches('/'), &|p| node_in(at.tree, p))?;
        let dest_is_dir = matches!(node_in(at.tree, &dest), Some(Node::Dir));
        let Some(top) = items.first() else {
            return Ok(());
        };
        let source_is_dir = top.kind == PlanKind::Dir;
        let target = if source_is_dir || unpack || !(ends_in_slash || dest_is_dir) {
            dest.clone()
        } else if dest.is_empty() {
            name.to_string()
        } else {
            format!("{dest}/{name}")
        };
        if target.is_empty() && !source_is_dir && !unpack {
            return Err(fail(
                "cannot copy a file onto the root directory".to_string(),
            ));
        }
        let owner = |i: &Item| at.owner.unwrap_or((i.uid, i.gid));
        let mode = |i: &Item| match (at.chmod, &i.kind) {
            (Some(m), PlanKind::Dir | PlanKind::File { .. }) => m,
            _ => i.mode,
        };
        // The parents, and the target directory itself when it is new.
        let parent_end = if unpack {
            target.len()
        } else {
            target.rfind('/').unwrap_or(0)
        };
        let new_dir_owner = at.owner.unwrap_or((0, 0));
        self.parents(at.tree, &target[..parent_end], new_dir_owner);
        if unpack {
            let PlanKind::File { from, .. } = &top.kind else {
                return Err(fail(format!("{name} is not an archive")));
            };
            self.hash.add(&["unpack", &target, &top.content]);
            self.entries.push(PlanEntry {
                path: target,
                mode: 0o755,
                uid: new_dir_owner.0,
                gid: new_dir_owner.1,
                kind: PlanKind::Unpack {
                    from: from.clone(),
                    owner: at.owner,
                },
                xattrs: Vec::new(),
            });
            return Ok(());
        }
        for item in items {
            let path = if item.rel.is_empty() {
                target.clone()
            } else if target.is_empty() {
                item.rel.clone()
            } else {
                format!("{target}/{}", item.rel)
            };
            if path.is_empty() {
                continue;
            }
            if item.rel.is_empty() && source_is_dir && dest_is_dir {
                // An existing directory keeps its own owner and mode.
                continue;
            }
            let (uid, gid) = owner(&item);
            let mode = mode(&item);
            self.hash.add(&[
                &path,
                &format!("{:?}", std::mem::discriminant(&item.kind)),
                &format!("{mode:o}"),
                &format!("{uid}:{gid}"),
                &item.content,
            ]);
            if item.kind == PlanKind::Dir {
                self.made.insert(path.clone(), ());
            }
            self.entries.push(PlanEntry {
                path,
                mode,
                uid,
                gid,
                kind: item.kind,
                xattrs: item.xattrs,
            });
        }
        Ok(())
    }

    /// A directory for each part of `dir` that the tree and this plan lack.
    fn parents(&mut self, tree: &Tree, dir: &str, owner: (u32, u32)) {
        let mut path = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(part);
            if node_in(tree, &path).is_some() || self.made.contains_key(&path) {
                continue;
            }
            self.made.insert(path.clone(), ());
            self.hash.add(&["mkdir", &path]);
            self.entries.push(PlanEntry {
                path: path.clone(),
                mode: 0o755,
                uid: owner.0,
                gid: owner.1,
                kind: PlanKind::Dir,
                xattrs: Vec::new(),
            });
        }
    }

    /// `WORKDIR` of a missing directory: make it.
    pub fn mkdir(&mut self, tree: &Tree, dir: &str) {
        self.parents(tree, dir.trim_start_matches('/'), (0, 0));
    }
}

/// How an `ADD` source file is packed, from its first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packing {
    Tar,
    Gzip,
    Zstd,
    Bzip2,
    Xz,
    /// Not an archive: copied as it is.
    None,
}

pub fn packing(head: &[u8]) -> Packing {
    match head {
        [0x1f, 0x8b, ..] => Packing::Gzip,
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Packing::Zstd,
        [b'B', b'Z', b'h', ..] => Packing::Bzip2,
        [0xfd, b'7', b'z', b'X', b'Z', 0, ..] => Packing::Xz,
        h if h.len() >= 262 && &h[257..262] == b"ustar" => Packing::Tar,
        _ => Packing::None,
    }
}

/// The first bytes of a file, enough for [`packing`].
pub fn head_of(path: &Path) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 512];
    let mut f = std::fs::File::open(path).map_err(|e| read_error(path, e))?;
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err(read_error(path, e)),
        }
    }
    buf.truncate(n);
    Ok(buf)
}

/// Write a plan as a layer tar.
pub fn write_plan(entries: &[PlanEntry], out: &mut dyn Write) -> Result<()> {
    let werr =
        |e: std::io::Error| Error::io("cannot write the step's layer", &e, "check the disk space");
    let mut b = tar::Builder::new(out);
    for e in entries {
        if let PlanKind::Unpack { from, owner } = &e.kind {
            unpack_into(&mut b, from, &e.path, *owner)?;
            continue;
        }
        let mut h = tar::Header::new_gnu();
        h.set_mode(e.mode & 0o7777);
        h.set_uid(u64::from(e.uid));
        h.set_gid(u64::from(e.gid));
        h.set_mtime(0);
        h.set_size(0);
        if !e.xattrs.is_empty() {
            let values: Vec<(String, Vec<u8>)> = e
                .xattrs
                .iter()
                .map(|(k, v)| (format!("SCHILY.xattr.{k}"), unhex(v)))
                .collect();
            b.append_pax_extensions(values.iter().map(|(k, v)| (k.as_str(), v.as_slice())))
                .map_err(werr)?;
        }
        let path = e.path.as_str();
        let empty = std::io::empty();
        let done = match &e.kind {
            PlanKind::Dir => {
                h.set_entry_type(tar::EntryType::Directory);
                b.append_data(&mut h, format!("{path}/"), empty)
            }
            PlanKind::File { from, size } => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(*size);
                let file = open_plain(from)?;
                b.append_data(&mut h, path, Exact(file, *size))
            }
            PlanKind::Symlink { target } => {
                h.set_entry_type(tar::EntryType::Symlink);
                b.append_link(&mut h, path, target)
            }
            PlanKind::Fifo => {
                h.set_entry_type(tar::EntryType::Fifo);
                b.append_data(&mut h, path, empty)
            }
            PlanKind::Char { major, minor } | PlanKind::Block { major, minor } => {
                h.set_entry_type(if matches!(e.kind, PlanKind::Char { .. }) {
                    tar::EntryType::Char
                } else {
                    tar::EntryType::Block
                });
                h.set_device_major(*major).map_err(werr)?;
                h.set_device_minor(*minor).map_err(werr)?;
                b.append_data(&mut h, path, empty)
            }
            PlanKind::Unpack { .. } => unreachable!("handled above"),
        };
        done.map_err(werr)?;
    }
    b.finish().map_err(werr)
}

/// Open a file without following a symbolic link in its last part.
fn open_plain(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|e| read_error(path, e))
}

/// The entries of the archive at `from`, below `dest`.
fn unpack_into(
    b: &mut tar::Builder<&mut dyn Write>,
    from: &Path,
    dest: &str,
    owner: Option<(u32, u32)>,
) -> Result<()> {
    let name = from.display().to_string();
    let bad = |why: String| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("cannot unpack {name}"),
            why,
            "check the archive",
        )
    };
    let file = open_plain(from)?;
    let reader: Box<dyn Read> = match packing(&head_of(from)?) {
        Packing::Tar | Packing::None => Box::new(file),
        Packing::Gzip => Box::new(flate2::read::MultiGzDecoder::new(file)),
        Packing::Zstd => Box::new(crate::store::zstd::ZstdFrames::new(
            std::io::BufReader::new(file),
        )),
        Packing::Bzip2 | Packing::Xz => {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("cannot unpack {name}"),
                "podbag reads tar archives that are plain, gzip or zstd, not bzip2 or xz",
                "unpack it in a RUN step, or recompress it with gzip",
            ));
        }
    };
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|e| bad(e.to_string()))?;
    for item in entries {
        let mut item = item.map_err(|e| bad(e.to_string()))?;
        let Some(rel) = crate::store::layer::normalize_path(&item.path_bytes())? else {
            continue;
        };
        let base = rel.rsplit('/').next().unwrap_or(&rel);
        if base.starts_with(".wh.") {
            continue;
        }
        let path = if dest.is_empty() {
            rel.clone()
        } else {
            format!("{dest}/{rel}")
        };
        let mut h = item.header().clone();
        if let Some((uid, gid)) = owner {
            h.set_uid(u64::from(uid));
            h.set_gid(u64::from(gid));
        }
        let kind = h.entry_type();
        let werr = |e: std::io::Error| bad(e.to_string());
        if let Some(exts) = item.pax_extensions().map_err(werr)? {
            let xattrs: Vec<(String, Vec<u8>)> = exts
                .flatten()
                .filter_map(|e| {
                    let k = e.key().ok()?;
                    k.starts_with("SCHILY.xattr.")
                        .then(|| (k.to_string(), e.value_bytes().to_vec()))
                })
                .collect();
            if !xattrs.is_empty() {
                b.append_pax_extensions(xattrs.iter().map(|(k, v)| (k.as_str(), v.as_slice())))
                    .map_err(werr)?;
            }
        }
        match kind {
            tar::EntryType::Link => {
                let target = item
                    .link_name_bytes()
                    .map(|l| l.into_owned())
                    .unwrap_or_default();
                let target = crate::store::layer::normalize_path(&target)?.unwrap_or_default();
                let target = if dest.is_empty() {
                    target
                } else {
                    format!("{dest}/{target}")
                };
                h.set_size(0);
                b.append_link(&mut h, &path, target).map_err(werr)?;
            }
            tar::EntryType::Symlink => {
                let target = item
                    .link_name_bytes()
                    .map(|l| String::from_utf8_lossy(&l).into_owned())
                    .unwrap_or_default();
                h.set_size(0);
                b.append_link(&mut h, &path, target).map_err(werr)?;
            }
            tar::EntryType::Directory => {
                h.set_size(0);
                b.append_data(&mut h, format!("{path}/"), std::io::empty())
                    .map_err(werr)?;
            }
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader => {}
            _ => {
                let size = h.size().map_err(werr)?;
                b.append_data(&mut h, &path, Exact(&mut item, size))
                    .map_err(werr)?;
            }
        }
    }
    Ok(())
}

/// Exactly `.1` bytes: zeros when the source ends early.
struct Exact<R>(R, u64);

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let want = buf.len().min(usize::try_from(self.1).unwrap_or(usize::MAX));
        if want == 0 {
            return Ok(0);
        }
        let mut n = self.0.read(&mut buf[..want])?;
        if n == 0 {
            buf[..want].fill(0);
            n = want;
        }
        self.1 -= n as u64;
        Ok(n)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entry(path: &str, kind: EntryKind) -> Entry {
        Entry {
            path: path.into(),
            kind,
            mode: 0o755,
            uid: 0,
            gid: 0,
            xattrs: vec![],
        }
    }

    /// An image with /usr/bin, /lib -> usr/lib, /usr/lib, /etc/conf (a file).
    fn image() -> Tree {
        let mut t = Tree::default();
        for e in [
            entry("etc", EntryKind::Dir),
            entry(
                "etc/conf",
                EntryKind::File {
                    object: format!("{}.0.0.644", "a".repeat(64)),
                    size: 3,
                },
            ),
            entry(
                "lib",
                EntryKind::Symlink {
                    target: "usr/lib".into(),
                },
            ),
            entry("usr", EntryKind::Dir),
            entry("usr/bin", EntryKind::Dir),
            entry("usr/lib", EntryKind::Dir),
        ] {
            t.entries.insert(e.path.clone(), e);
        }
        t
    }

    fn context() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        std::fs::create_dir_all(r.join("src/sub")).unwrap();
        std::fs::write(r.join("src/a.txt"), "a").unwrap();
        std::fs::write(r.join("src/sub/b.txt"), "b").unwrap();
        std::fs::write(r.join("src/skip.log"), "x").unwrap();
        std::fs::write(r.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(r.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("run.sh", r.join("link")).unwrap();
        tmp
    }

    fn paths(plan: &Plan) -> Vec<String> {
        plan.entries.iter().map(|e| e.path.clone()).collect()
    }

    fn copy(dest: &str, sources: &[&str], ignore: &str) -> Result<Plan> {
        let ctx = context();
        let ignore = Ignore::parse(ignore);
        let tree = image();
        let source = Source::Context {
            root: ctx.path(),
            ignore: &ignore,
        };
        let mut found = Vec::new();
        for s in sources {
            found.extend(source.find(s)?);
        }
        let at = Placement {
            dest: dest.to_string(),
            tree: &tree,
            owner: None,
            chmod: None,
            many: found.len() > 1,
        };
        let mut plan = Plan::default();
        for f in found {
            let name = f.rsplit('/').next().unwrap_or(&f).to_string();
            let items = source.items(&f)?;
            plan.place(&at, &name, items, false)?;
        }
        Ok(plan)
    }

    #[test]
    fn a_file_to_a_new_path_or_into_a_directory() {
        assert_eq!(
            paths(&copy("/opt/app/run", &["run.sh"], "").unwrap()),
            ["opt", "opt/app", "opt/app/run"]
        );
        assert_eq!(
            paths(&copy("/usr/bin", &["run.sh"], "").unwrap()),
            ["usr/bin/run.sh"]
        );
        assert_eq!(
            paths(&copy("/srv/", &["run.sh"], "").unwrap()),
            ["srv", "srv/run.sh"]
        );
        // Through the image's link: /lib is usr/lib.
        assert_eq!(
            paths(&copy("/lib/", &["run.sh"], "").unwrap()),
            ["usr/lib/run.sh"]
        );
        assert_eq!(
            paths(&copy("/lib/new/", &["run.sh"], "").unwrap()),
            ["usr/lib/new", "usr/lib/new/run.sh"]
        );
        let p = copy("/x", &["run.sh"], "").unwrap();
        assert_eq!(p.entries[0].mode, 0o755);
        assert_eq!((p.entries[0].uid, p.entries[0].gid), (0, 0));
    }

    #[test]
    fn a_directory_copies_its_contents() {
        assert_eq!(
            paths(&copy("/app", &["src"], "*.log\nsrc/*.log").unwrap()),
            ["app", "app/a.txt", "app/sub", "app/sub/b.txt"]
        );
        // Into an existing directory: it keeps its own metadata.
        assert_eq!(
            paths(&copy("/usr", &["src/sub"], "").unwrap()),
            ["usr/b.txt"]
        );
    }

    #[test]
    fn wildcards_links_and_errors() {
        assert_eq!(
            paths(&copy("/d/", &["src/*.txt", "run.sh"], "").unwrap()),
            ["d", "d/a.txt", "d/run.sh"]
        );
        // A top-level link is followed.
        let p = copy("/x", &["link"], "").unwrap();
        assert!(matches!(p.entries[0].kind, PlanKind::File { .. }));
        let e = copy("/d", &["src/a.txt", "run.sh"], "").unwrap_err();
        assert!(e.why.contains("end with /"), "{e}");
        let e = copy("/d/", &["nope"], "").unwrap_err();
        assert!(e.why.contains("not found"), "{e}");
        let e = copy("/d/", &["run.sh"], "run.sh").unwrap_err();
        assert!(e.why.contains(".dockerignore"), "{e}");
        let e = copy("/d/", &["*.nothing"], "").unwrap_err();
        assert!(e.why.contains("no source files"), "{e}");
    }

    #[test]
    fn digests_follow_content_not_time() {
        let a = copy("/x", &["src"], "").unwrap().digest();
        let b = copy("/x", &["src"], "").unwrap().digest();
        assert_eq!(a, b);
        assert_ne!(a, copy("/y", &["src"], "").unwrap().digest());
    }

    #[test]
    fn image_sources_keep_xattrs_and_owners_and_glob() {
        let tree = image();
        let object = |k: &str| PathBuf::from(format!("/objects/{k}"));
        let source = Source::Image {
            tree: &tree,
            object: &object,
        };
        assert_eq!(source.find("/lib").unwrap(), ["usr/lib"]);
        assert_eq!(source.find("/etc/c*").unwrap(), ["etc/conf"]);
        let items = source.items("etc").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].rel, "conf");
        assert_eq!(items[1].content, "a".repeat(64));
        assert!(
            matches!(&items[1].kind, PlanKind::File { from, .. } if from.to_string_lossy().ends_with("0.0.644"))
        );
    }

    #[test]
    fn plans_write_as_tars_and_unpack_archives() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), "hello").unwrap();
        let mut inner = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o600);
        h.set_uid(5);
        h.set_gid(5);
        h.set_size(2);
        inner.append_data(&mut h, "in/side", &b"hi"[..]).unwrap();
        let archive = inner.into_inner().unwrap();
        let gz = {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&archive).unwrap();
            e.finish().unwrap()
        };
        std::fs::write(tmp.path().join("a.tgz"), &gz).unwrap();
        assert_eq!(
            packing(&head_of(&tmp.path().join("a.tgz")).unwrap()),
            Packing::Gzip
        );
        assert_eq!(packing(&archive), Packing::Tar);
        assert_eq!(packing(b"plain text"), Packing::None);
        let plan = vec![
            PlanEntry {
                path: "x/f".into(),
                mode: 0o644,
                uid: 1,
                gid: 2,
                kind: PlanKind::File {
                    from: tmp.path().join("f"),
                    size: 5,
                },
                xattrs: vec![],
            },
            PlanEntry {
                path: "opt".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
                kind: PlanKind::Unpack {
                    from: tmp.path().join("a.tgz"),
                    owner: Some((7, 8)),
                },
                xattrs: vec![],
            },
        ];
        let mut out = Vec::new();
        write_plan(&plan, &mut out).unwrap();
        let mut a = tar::Archive::new(&out[..]);
        let got: Vec<(String, u64, String)> = a
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let mut body = String::new();
                e.read_to_string(&mut body).unwrap();
                (
                    e.path().unwrap().display().to_string(),
                    e.header().uid().unwrap(),
                    body,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("x/f".to_string(), 1, "hello".to_string()),
                ("opt/in/side".to_string(), 7, "hi".to_string())
            ]
        );
    }
}
