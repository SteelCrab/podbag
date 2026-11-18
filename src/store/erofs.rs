//! EROFS images for composefs (design 6.3.2 and 6.3.3): an image's directory tree with
//! owners, modes and xattrs, but no file contents. Every regular file with content carries
//! overlayfs's metacopy and redirect xattrs, which name its object; overlayfs, with the
//! object store as a data-only layer, reads the contents from there.
//!
//! Only the parts of the format this needs are written: 4 KiB blocks, extended inodes,
//! inline xattrs, flat and tail-packed directories and symlinks, and chunk-based files
//! whose chunks are all holes. An inode never crosses a block boundary.

use super::layer::{EntryKind, unhex};
use super::object::ObjectStore;
use super::tree::Tree;
use crate::{Error, ErrorKind, Result};
use std::collections::{HashMap, HashSet, VecDeque};

pub const BLOCK_SIZE: usize = 4096;
const BLOCK_BITS: u32 = 12;
const SUPER_OFFSET: usize = 1024;
const SUPER_SIZE: usize = 128;
const MAGIC: u32 = 0xE0F5_E1E2;
const INCOMPAT_CHUNKED_FILE: u32 = 0x4;
/// Extended inodes only: 32-bit owners, 64-bit sizes.
const INODE_SIZE: usize = 64;
/// Inodes sit on 32-byte slots; a nid is the slot number.
const SLOT: usize = 32;
const DIRENT_SIZE: usize = 12;
const NAME_MAX: usize = 255;
const NULL_ADDR: u32 = u32::MAX;
const MAX_CHUNK_BITS: u32 = BLOCK_BITS + 31;
const XATTR_HEADER: usize = 12;

const FLAT_PLAIN: u16 = 0;
const FLAT_INLINE: u16 = 2;
const CHUNK_BASED: u16 = 4;

const XATTR_USER: u8 = 1;
const XATTR_TRUSTED: u8 = 4;
const XATTR_SECURITY: u8 = 6;

const S_IFMT: u16 = 0o170000;
const S_IFIFO: u16 = 0o010000;
const S_IFCHR: u16 = 0o020000;
const S_IFDIR: u16 = 0o040000;
const S_IFBLK: u16 = 0o060000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;

enum Kind {
    /// Entries sorted by name, `.` and `..` included.
    Dir {
        parent: usize,
        entries: Vec<(Vec<u8>, usize)>,
    },
    File {
        size: u64,
    },
    Symlink(Vec<u8>),
    /// A character or block device (the type is in the mode).
    Device(u32),
    Fifo,
}

struct Node {
    kind: Kind,
    mode: u16,
    uid: u32,
    gid: u32,
    nlink: u32,
    /// The encoded inline xattr body, empty when there are none.
    xattrs: Vec<u8>,
    xattr_count: u16,
    layout: u16,
    size: u64,
    /// Bytes after the xattrs in the metadata area: a tail or a chunk map.
    inline: usize,
    /// Whole data blocks after the metadata area.
    blocks: usize,
    pos: usize,
    blkaddr: u32,
    chunk_format: u16,
}

impl Node {
    fn new(kind: Kind, mode: u16, uid: u32, gid: u32) -> Node {
        Node {
            kind,
            mode,
            uid,
            gid,
            nlink: 1,
            xattrs: Vec::new(),
            xattr_count: 0,
            layout: FLAT_PLAIN,
            size: 0,
            inline: 0,
            blocks: 0,
            pos: 0,
            blkaddr: 0,
            chunk_format: 0,
        }
    }

    fn nid(&self) -> u64 {
        (self.pos / SLOT) as u64
    }

    fn file_type(&self) -> u8 {
        match self.mode & S_IFMT {
            S_IFREG => 1,
            S_IFDIR => 2,
            S_IFCHR => 3,
            S_IFBLK => 4,
            S_IFIFO => 5,
            _ => 7,
        }
    }
}

fn unstorable(why: String) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "cannot build the image's EROFS metadata",
        why,
        "the image cannot use composefs; set image_mount = \"hardlink\" in config.toml",
    )
}

/// Build the EROFS image of `tree`. `mtime` (seconds) is every inode's modification time.
pub fn build(tree: &Tree, mtime: u64) -> Result<Vec<u8>> {
    let mut nodes = collect(tree)?;
    let order = placement_order(&nodes);
    for &i in &order {
        lay_out(&mut nodes, i)?;
    }
    // Metadata: the root first, so its nid fits the superblock's 16-bit field.
    let mut pos = SUPER_OFFSET + SUPER_SIZE;
    for &i in &order {
        let need = INODE_SIZE + nodes[i].xattrs.len() + nodes[i].inline;
        pos = pos.next_multiple_of(SLOT);
        if pos % BLOCK_SIZE + need > BLOCK_SIZE {
            pos = pos.next_multiple_of(BLOCK_SIZE);
        }
        nodes[i].pos = pos;
        pos += need;
    }
    let mut block = pos.div_ceil(BLOCK_SIZE);
    for &i in &order {
        if nodes[i].blocks > 0 {
            nodes[i].blkaddr =
                u32::try_from(block).map_err(|_| unstorable("the metadata is too large".into()))?;
            block += nodes[i].blocks;
        }
    }
    let total_blocks =
        u32::try_from(block).map_err(|_| unstorable("the metadata is too large".into()))?;

    let mut img = vec![0u8; block * BLOCK_SIZE];
    let sb = SUPER_OFFSET;
    put32(&mut img, sb, MAGIC);
    img[sb + 12] = BLOCK_BITS as u8;
    put16(&mut img, sb + 14, nodes[order[0]].nid() as u16);
    put64(&mut img, sb + 16, order.len() as u64);
    put64(&mut img, sb + 24, mtime);
    put32(&mut img, sb + 36, total_blocks);
    put32(&mut img, sb + 80, INCOMPAT_CHUNKED_FILE);

    for (ino, &i) in order.iter().enumerate() {
        let data = node_data(&nodes, i);
        let n = &nodes[i];
        let p = n.pos;
        put16(&mut img, p, 1 | (n.layout << 1));
        put16(&mut img, p + 2, n.xattr_count);
        put16(&mut img, p + 4, n.mode);
        put64(&mut img, p + 8, n.size);
        let iu = match &n.kind {
            Kind::Device(rdev) => *rdev,
            Kind::File { .. } if n.layout == CHUNK_BASED => u32::from(n.chunk_format),
            _ => n.blkaddr,
        };
        put32(&mut img, p + 16, iu);
        put32(&mut img, p + 20, ino as u32 + 1);
        put32(&mut img, p + 24, n.uid);
        put32(&mut img, p + 28, n.gid);
        put64(&mut img, p + 32, mtime);
        put32(&mut img, p + 44, n.nlink);
        let x = p + INODE_SIZE;
        img[x..x + n.xattrs.len()].copy_from_slice(&n.xattrs);
        let inline_at = x + n.xattrs.len();
        if n.layout == CHUNK_BASED {
            // Every chunk is a hole: overlayfs reads the contents from the object.
            for entry in img[inline_at..inline_at + n.inline].chunks_exact_mut(4) {
                entry.copy_from_slice(&NULL_ADDR.to_le_bytes());
            }
        } else {
            let in_blocks = n.blocks * BLOCK_SIZE;
            let (head, tail) = data.split_at(in_blocks.min(data.len()));
            let start = n.blkaddr as usize * BLOCK_SIZE;
            img[start..start + head.len()].copy_from_slice(head);
            img[inline_at..inline_at + tail.len()].copy_from_slice(tail);
        }
    }
    Ok(img)
}

/// One node per directory, symlink, device and FIFO; one per object for regular files,
/// so paths that share an object share an inode, like hard links to it.
fn collect(tree: &Tree) -> Result<Vec<Node>> {
    let mut nodes = vec![Node::new(
        Kind::Dir {
            parent: 0,
            entries: Vec::new(),
        },
        S_IFDIR | 0o755,
        0,
        0,
    )];
    let mut dirs: HashMap<&str, usize> = HashMap::from([("", 0)]);
    let mut by_object: HashMap<&str, usize> = HashMap::new();
    for (path, e) in &tree.entries {
        let (parent_path, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        if name.len() > NAME_MAX {
            return Err(unstorable(format!(
                "the name of {path} is longer than {NAME_MAX} bytes"
            )));
        }
        let Some(&parent) = dirs.get(parent_path) else {
            return Err(Error::new(
                ErrorKind::Internal,
                "cannot build the image's EROFS metadata",
                format!("{path} has no parent directory in the merged tree"),
                "report this bug",
            ));
        };
        let perm = (e.mode & 0o7777) as u16;
        let mut xattrs = image_xattrs(path, &e.xattrs)?;
        let index = match &e.kind {
            EntryKind::Dir => {
                nodes.push(Node::new(
                    Kind::Dir {
                        parent,
                        entries: Vec::new(),
                    },
                    S_IFDIR | perm,
                    e.uid,
                    e.gid,
                ));
                dirs.insert(path, nodes.len() - 1);
                nodes.len() - 1
            }
            EntryKind::File { object, size } => {
                if let Some(&i) = by_object.get(object.as_str()) {
                    nodes[i].nlink += 1;
                    i
                } else {
                    let redirect =
                        (*size > 0).then(|| format!("/{}", ObjectStore::relative_path(object)));
                    if let Some(r) = &redirect {
                        xattrs.push((XATTR_TRUSTED, b"overlay.metacopy".to_vec(), Vec::new()));
                        xattrs.push((
                            XATTR_TRUSTED,
                            b"overlay.redirect".to_vec(),
                            r.as_bytes().to_vec(),
                        ));
                    }
                    nodes.push(Node::new(
                        Kind::File { size: *size },
                        S_IFREG | perm,
                        e.uid,
                        e.gid,
                    ));
                    by_object.insert(object, nodes.len() - 1);
                    nodes.len() - 1
                }
            }
            EntryKind::Symlink { target } => {
                if target.is_empty() {
                    return Err(unstorable(format!(
                        "the symbolic link {path} has no target"
                    )));
                }
                nodes.push(Node::new(
                    Kind::Symlink(target.as_bytes().to_vec()),
                    S_IFLNK | 0o777,
                    e.uid,
                    e.gid,
                ));
                nodes.len() - 1
            }
            EntryKind::Char { major, minor } | EntryKind::Block { major, minor } => {
                let kind = if matches!(e.kind, EntryKind::Char { .. }) {
                    S_IFCHR
                } else {
                    S_IFBLK
                };
                let rdev = (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
                nodes.push(Node::new(Kind::Device(rdev), kind | perm, e.uid, e.gid));
                nodes.len() - 1
            }
            EntryKind::Fifo => {
                nodes.push(Node::new(Kind::Fifo, S_IFIFO | perm, e.uid, e.gid));
                nodes.len() - 1
            }
            // merge() resolves hard links and applies whiteouts and opaque markers.
            EntryKind::Hardlink { .. } | EntryKind::Whiteout | EntryKind::Opaque => continue,
        };
        if nodes[index].xattrs.is_empty() && !xattrs.is_empty() {
            let (body, count) = encode_xattrs(path, &xattrs)?;
            nodes[index].xattrs = body;
            nodes[index].xattr_count = count;
        }
        if let Kind::Dir { entries, .. } = &mut nodes[parent].kind {
            entries.push((name.as_bytes().to_vec(), index));
        }
    }
    // `.` and `..`, the sort, and directory link counts.
    let subdirs: Vec<(usize, usize)> = nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| match &n.kind {
            Kind::Dir { parent, .. } if i != 0 => Some((i, *parent)),
            _ => None,
        })
        .collect();
    for (_, parent) in &subdirs {
        nodes[*parent].nlink += 1;
    }
    for (i, n) in nodes.iter_mut().enumerate() {
        if let Kind::Dir { parent, entries } = &mut n.kind {
            n.nlink += 1;
            entries.push((b".".to_vec(), i));
            entries.push((b"..".to_vec(), *parent));
            entries.sort();
        }
    }
    Ok(nodes)
}

/// Image xattrs as (name index, name without its prefix, value). The layer reader keeps
/// only `user.*` and `security.*`; `trusted.*` would be read by overlayfs as its own.
fn image_xattrs(path: &str, xattrs: &[(String, String)]) -> Result<Vec<(u8, Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    for (name, value) in xattrs {
        let (index, rest) = if let Some(rest) = name.strip_prefix("user.") {
            (XATTR_USER, rest)
        } else if let Some(rest) = name.strip_prefix("security.") {
            (XATTR_SECURITY, rest)
        } else {
            return Err(unstorable(format!(
                "{path} has the extended attribute {name}, which an EROFS image for composefs cannot hold"
            )));
        };
        if rest.is_empty() {
            return Err(unstorable(format!(
                "{path} has an extended attribute without a name ({name})"
            )));
        }
        out.push((index, rest.as_bytes().to_vec(), unhex(value)));
    }
    out.sort();
    Ok(out)
}

fn encode_xattrs(path: &str, list: &[(u8, Vec<u8>, Vec<u8>)]) -> Result<(Vec<u8>, u16)> {
    let mut body = vec![0u8; XATTR_HEADER];
    for (index, name, value) in list {
        let too_big = || unstorable(format!("an extended attribute of {path} is too large"));
        let name_len = u8::try_from(name.len()).map_err(|_| too_big())?;
        let value_len = u16::try_from(value.len()).map_err(|_| too_big())?;
        body.push(name_len);
        body.push(*index);
        body.extend(value_len.to_le_bytes());
        body.extend(name);
        body.extend(value);
        body.resize(body.len().next_multiple_of(4), 0);
    }
    let count = u16::try_from((body.len() - XATTR_HEADER) / 4 + 1)
        .map_err(|_| unstorable(format!("the extended attributes of {path} are too large")))?;
    Ok((body, count))
}

/// Breadth-first from the root; each node once (shared file inodes at their first path).
fn placement_order(nodes: &[Node]) -> Vec<usize> {
    let mut order = vec![0];
    let mut seen = HashSet::from([0]);
    let mut queue = VecDeque::from([0]);
    while let Some(d) = queue.pop_front() {
        if let Kind::Dir { entries, .. } = &nodes[d].kind {
            for (_, child) in entries {
                if seen.insert(*child) {
                    order.push(*child);
                    if matches!(nodes[*child].kind, Kind::Dir { .. }) {
                        queue.push_back(*child);
                    }
                }
            }
        }
    }
    order
}

/// The bytes of a directory or symlink. Directory nids come from the nodes' positions,
/// so the length is the same before and after placement.
fn node_data(nodes: &[Node], i: usize) -> Vec<u8> {
    match &nodes[i].kind {
        Kind::Symlink(target) => target.clone(),
        Kind::Dir { entries, .. } => {
            let mut out = Vec::new();
            let mut start = 0;
            while start < entries.len() {
                // As many entries as fit in one block.
                let mut end = start;
                let mut used = 0;
                while end < entries.len() && used + DIRENT_SIZE + entries[end].0.len() <= BLOCK_SIZE
                {
                    used += DIRENT_SIZE + entries[end].0.len();
                    end += 1;
                }
                let block_start = out.len();
                let mut nameoff = (end - start) * DIRENT_SIZE;
                for (name, child) in &entries[start..end] {
                    out.extend(nodes[*child].nid().to_le_bytes());
                    out.extend((nameoff as u16).to_le_bytes());
                    out.push(nodes[*child].file_type());
                    out.push(0);
                    nameoff += name.len();
                }
                for (name, _) in &entries[start..end] {
                    out.extend(name);
                }
                if end < entries.len() {
                    out.resize(block_start + BLOCK_SIZE, 0);
                }
                start = end;
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Choose the data layout, size, inline bytes and block count of node `i`.
fn lay_out(nodes: &mut [Node], i: usize) -> Result<()> {
    let len = node_data(nodes, i).len();
    let n = &mut nodes[i];
    let base = INODE_SIZE + n.xattrs.len();
    match &n.kind {
        Kind::File { size, .. } if *size > 0 => {
            let bits = (u64::BITS - (size - 1).leading_zeros()).clamp(BLOCK_BITS, MAX_CHUNK_BITS);
            let count = usize::try_from(size.div_ceil(1u64 << bits)).unwrap_or(usize::MAX);
            n.layout = CHUNK_BASED;
            n.size = *size;
            n.chunk_format = (bits - BLOCK_BITS) as u16;
            n.inline = count.saturating_mul(4);
        }
        Kind::Dir { .. } | Kind::Symlink(_) => {
            let nblocks = len.div_ceil(BLOCK_SIZE);
            let tail = len - (nblocks - 1) * BLOCK_SIZE;
            n.size = len as u64;
            if tail < BLOCK_SIZE && base + tail <= BLOCK_SIZE {
                n.layout = FLAT_INLINE;
                n.inline = tail;
                n.blocks = nblocks - 1;
            } else {
                n.layout = FLAT_PLAIN;
                n.blocks = nblocks;
            }
        }
        Kind::File { .. } | Kind::Device(_) | Kind::Fifo => {}
    }
    if base.saturating_add(n.inline) > BLOCK_SIZE {
        return Err(unstorable(
            "a file's extended attributes are too large (more than 4 KiB with the inode)".into(),
        ));
    }
    Ok(())
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Directory entry names that test the kernel's rules: names that sort before `.`,
/// spaces, newlines, non-ASCII bytes, 255-byte names, prefixes of each other, and 3000
/// more, so the directory spans many blocks.
#[cfg(test)]
pub(crate) fn odd_names() -> Vec<String> {
    let mut names: Vec<String> = [
        "-x", " x", "!", "#a", "a\nb", "a b", "a", "aa", "a.", "ÿ", "é\t", "~", "Z", "_",
    ]
    .iter()
    .map(|n| n.to_string())
    .collect();
    names.push("z".repeat(255));
    names.push(format!("{}é", "y".repeat(253)));
    for i in 0..3000u32 {
        let len = 1 + (i as usize * 13) % 200;
        let mut name = format!("f{i:x}");
        while name.len() < len {
            name.push(['.', '-', ' ', 'q'][i as usize % 4]);
        }
        names.push(name);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::store::layer::{Entry, EntryKind, hex};
    use crate::store::tree::{Tree, merge};
    use std::collections::BTreeMap;

    // ---- A reader that walks an image the way the kernel does, checking its rules. ----

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Item {
        mode: u16,
        uid: u32,
        gid: u32,
        size: u64,
        nlink: u32,
        nid: u64,
        layout: u16,
        target: Option<Vec<u8>>,
        rdev: Option<(u32, u32)>,
        xattrs: Vec<(String, Vec<u8>)>,
        chunks: Vec<u32>,
        dir_blocks: usize,
    }

    fn u16_at(b: &[u8], o: usize) -> u16 {
        u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
    }
    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    const PLAIN: u16 = 0;
    const INLINE: u16 = 2;
    const CHUNKS: u16 = 4;

    struct Raw {
        item: Item,
        /// Directory or symlink contents.
        data: Vec<u8>,
    }

    fn inode(img: &[u8], nid: u64) -> Raw {
        let pos = nid as usize * 32;
        let format = u16_at(img, pos);
        assert_eq!(format & 1, 1, "nid {nid}: only extended inodes are written");
        let layout = (format >> 1) & 7;
        let icount = u16_at(img, pos + 2) as usize;
        let mode = u16_at(img, pos + 4);
        let size = u64_at(img, pos + 8);
        let iu = u32_at(img, pos + 16);
        let uid = u32_at(img, pos + 24);
        let gid = u32_at(img, pos + 28);
        let nlink = u32_at(img, pos + 44);
        let xsize = if icount == 0 {
            0
        } else {
            12 + (icount - 1) * 4
        };
        let mut xattrs = Vec::new();
        if icount > 0 {
            let base = pos + 64;
            assert_eq!(img[base + 4], 0, "no shared xattrs");
            let mut at = base + 12;
            while at < base + xsize {
                let len = img[at] as usize;
                let index = img[at + 1];
                let vlen = u16_at(img, at + 2) as usize;
                let name = String::from_utf8(img[at + 4..at + 4 + len].to_vec()).unwrap();
                let value = img[at + 4 + len..at + 4 + len + vlen].to_vec();
                let prefix = match index {
                    1 => "user.",
                    4 => "trusted.",
                    6 => "security.",
                    other => panic!("nid {nid}: unexpected xattr index {other}"),
                };
                xattrs.push((format!("{prefix}{name}"), value));
                at += (4 + len + vlen).div_ceil(4) * 4;
            }
            assert_eq!(
                at,
                base + xsize,
                "nid {nid}: xattr count matches the entries"
            );
        }
        let inline_at = pos + 64 + xsize;
        let block = |o: usize| o / BLOCK_SIZE;
        let mut chunks = Vec::new();
        let mut data = Vec::new();
        let mut meta_end = inline_at;
        match layout {
            CHUNKS => {
                let bits = 12 + u32::from(iu as u16 & 0x1f);
                assert_eq!(iu & 0x20, 0, "block-map chunks, not indexes");
                let count = size.div_ceil(1u64 << bits) as usize;
                let map = inline_at.div_ceil(4) * 4;
                for i in 0..count {
                    chunks.push(u32_at(img, map + 4 * i));
                }
                meta_end = map + 4 * count;
            }
            PLAIN | INLINE => {
                let size = size as usize;
                let nblocks = size.div_ceil(BLOCK_SIZE);
                let full = if layout == INLINE {
                    nblocks.saturating_sub(1)
                } else {
                    nblocks
                };
                let start = iu as usize * BLOCK_SIZE;
                let from_blocks = (full * BLOCK_SIZE).min(size);
                if from_blocks > 0 {
                    data.extend_from_slice(&img[start..start + from_blocks]);
                }
                if layout == INLINE {
                    let tail = size - from_blocks;
                    assert!(tail < BLOCK_SIZE || size == 0);
                    data.extend_from_slice(&img[inline_at..inline_at + tail]);
                    meta_end = inline_at + tail;
                }
            }
            other => panic!("nid {nid}: unexpected layout {other}"),
        }
        assert!(
            meta_end == pos + 64 || block(pos) == block(meta_end - 1),
            "nid {nid}: the inode at {pos} crosses a block boundary (ends at {meta_end})"
        );
        let kind = mode & 0o170000;
        let rdev = (kind == 0o020000 || kind == 0o060000)
            .then(|| (((iu >> 8) & 0xfff), ((iu & 0xff) | ((iu >> 12) & 0xfff00))));
        let target = (kind == 0o120000).then(|| data.clone());
        Raw {
            item: Item {
                mode,
                uid,
                gid,
                size,
                nlink,
                nid,
                layout,
                target,
                rdev,
                xattrs,
                chunks,
                dir_blocks: 0,
            },
            data,
        }
    }

    fn file_type(mode: u16) -> u8 {
        match mode & 0o170000 {
            0o100000 => 1,
            0o040000 => 2,
            0o020000 => 3,
            0o060000 => 4,
            0o010000 => 5,
            0o120000 => 7,
            other => panic!("mode type {other:o}"),
        }
    }

    /// Directory entries with the kernel's rules for names.
    fn entries(data: &[u8], size: usize) -> (Vec<(Vec<u8>, u64, u8)>, usize) {
        let mut out = Vec::new();
        let mut blocks = 0;
        let mut pos = 0;
        while pos < size {
            blocks += 1;
            let maxsize = (size - pos).min(BLOCK_SIZE);
            let blk = &data[pos..pos + maxsize];
            let first = u16_at(blk, 8) as usize;
            assert!(
                first >= 12 && first < BLOCK_SIZE && first % 12 == 0,
                "nameoff {first}"
            );
            let n = first / 12;
            for i in 0..n {
                let nid = u64_at(blk, 12 * i);
                let nameoff = u16_at(blk, 12 * i + 8) as usize;
                let ft = blk[12 * i + 10];
                let len = if i + 1 < n {
                    u16_at(blk, 12 * (i + 1) + 8) as usize - nameoff
                } else {
                    blk[nameoff..maxsize]
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(maxsize - nameoff)
                };
                assert!(len > 0 && len <= 255 && nameoff + len <= maxsize);
                out.push((blk[nameoff..nameoff + len].to_vec(), nid, ft));
            }
            pos += BLOCK_SIZE;
        }
        for w in out.windows(2) {
            assert!(w[0].0 < w[1].0, "names out of order: {:?}", w);
        }
        (out, blocks)
    }

    fn read(img: &[u8]) -> BTreeMap<String, Item> {
        assert_eq!(img.len() % BLOCK_SIZE, 0);
        let sb = &img[1024..1152];
        assert_eq!(u32_at(sb, 0), 0xE0F5_E1E2, "magic");
        assert_eq!(sb[12], 12, "block size bits");
        assert_eq!(u32_at(sb, 8), 0, "no compat features (no checksum)");
        assert_eq!(u32_at(sb, 80) & !0x4, 0, "only the chunked-file feature");
        assert_eq!(u32_at(sb, 36) as usize * BLOCK_SIZE, img.len(), "blocks");
        let root = u64::from(u16_at(sb, 14));
        let mut out = BTreeMap::new();
        walk(img, root, root, String::new(), &mut out);
        assert_eq!(
            u64_at(sb, 16) as usize,
            out.values()
                .map(|i| i.nid)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            "inos"
        );
        out
    }

    fn walk(img: &[u8], nid: u64, parent: u64, path: String, out: &mut BTreeMap<String, Item>) {
        let Raw { mut item, data } = inode(img, nid);
        if item.mode & 0o170000 == 0o040000 {
            let (list, blocks) = entries(&data, item.size as usize);
            item.dir_blocks = blocks;
            let mut dots = 0;
            for (name, child, ft) in list {
                match name.as_slice() {
                    b"." => {
                        assert_eq!(child, nid);
                        dots += 1;
                    }
                    b".." => {
                        assert_eq!(child, parent);
                        dots += 1;
                    }
                    _ => {
                        let name = String::from_utf8(name).unwrap();
                        let p = if path.is_empty() {
                            name
                        } else {
                            format!("{path}/{name}")
                        };
                        let child_mode = u16_at(img, child as usize * 32 + 4);
                        assert_eq!(ft, file_type(child_mode), "{p}: dirent type");
                        walk(img, child, nid, p, out);
                    }
                }
            }
            assert_eq!(dots, 2, "{path:?}: . and ..");
        }
        out.insert(path, item);
    }

    // ---- Trees for the tests. ----

    fn key(n: u32) -> String {
        format!("ab{n:062x}.0.0.755")
    }

    fn entry(path: &str, kind: EntryKind, mode: u32) -> Entry {
        Entry {
            path: path.into(),
            kind,
            mode,
            uid: 0,
            gid: 0,
            xattrs: vec![],
        }
    }

    fn file(path: &str, n: u32, size: u64) -> Entry {
        entry(
            path,
            EntryKind::File {
                object: key(n),
                size,
            },
            0o755,
        )
    }

    fn tree(entries: Vec<Entry>) -> Tree {
        merge(&[entries]).unwrap()
    }

    fn built(entries: Vec<Entry>) -> BTreeMap<String, Item> {
        read(&build(&tree(entries), 1_700_000_000).unwrap())
    }

    fn xattr(items: &BTreeMap<String, Item>, path: &str, name: &str) -> Option<Vec<u8>> {
        items[path]
            .xattrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn an_empty_tree_is_a_root_directory() {
        let img = build(&Tree::default(), 0).unwrap();
        assert_eq!(img.len() % BLOCK_SIZE, 0);
        let items = read(&img);
        assert_eq!(items.len(), 1);
        let root = &items[""];
        assert_eq!(root.mode, 0o40755);
        assert_eq!((root.uid, root.gid, root.nlink, root.nid), (0, 0, 2, 36));
    }

    #[test]
    fn files_point_to_their_objects() {
        let items = built(vec![file("bin/sh", 1, 1234)]);
        let sh = &items["bin/sh"];
        assert_eq!(sh.mode, 0o100755);
        assert_eq!(sh.size, 1234);
        assert_eq!(sh.layout, CHUNKS);
        assert_eq!(sh.chunks, vec![u32::MAX]);
        assert_eq!(
            sh.xattrs,
            vec![
                ("trusted.overlay.metacopy".to_string(), vec![]),
                (
                    "trusted.overlay.redirect".to_string(),
                    format!("/ab/{}", key(1)).into_bytes()
                ),
            ]
        );
        assert_eq!(items["bin"].mode, 0o40755);
    }

    #[test]
    fn empty_files_carry_no_redirect() {
        let items = built(vec![file("empty", 1, 0)]);
        assert_eq!(items["empty"].layout, PLAIN);
        assert_eq!(items["empty"].size, 0);
        assert!(items["empty"].xattrs.is_empty());
    }

    #[test]
    fn paths_with_one_object_share_an_inode() {
        let items = built(vec![file("a", 1, 10), file("d/b", 1, 10), file("c", 2, 10)]);
        assert_eq!(items["a"].nid, items["d/b"].nid);
        assert_eq!(items["a"].nlink, 2);
        assert_ne!(items["c"].nid, items["a"].nid);
        assert_eq!(items["c"].nlink, 1);
        // The root holds one subdirectory.
        assert_eq!(items[""].nlink, 3);
    }

    #[test]
    fn links_devices_and_fifos_keep_their_kind() {
        let items = built(vec![
            entry(
                "l",
                EntryKind::Symlink {
                    target: "../target".into(),
                },
                0o755,
            ),
            entry("null", EntryKind::Char { major: 1, minor: 3 }, 0o666),
            entry("sda", EntryKind::Block { major: 8, minor: 0 }, 0o660),
            entry(
                "big",
                EntryKind::Char {
                    major: 300,
                    minor: 70000,
                },
                0o600,
            ),
            entry("fifo", EntryKind::Fifo, 0o600),
        ]);
        assert_eq!(items["l"].mode, 0o120777);
        assert_eq!(items["l"].target.as_deref(), Some(&b"../target"[..]));
        assert_eq!(items["null"].mode, 0o20666);
        assert_eq!(items["null"].rdev, Some((1, 3)));
        assert_eq!(items["sda"].mode, 0o60660);
        assert_eq!(items["sda"].rdev, Some((8, 0)));
        assert_eq!(items["big"].rdev, Some((300, 70000)));
        assert_eq!(items["fifo"].mode, 0o10600);
        assert_eq!(items["fifo"].size, 0);
    }

    #[test]
    fn owners_modes_and_xattrs_are_kept() {
        let cap: Vec<u8> = (0..20).collect();
        let mut f = file("usr/bin/ping", 1, 99);
        f.uid = 100_000;
        f.gid = 100_001;
        f.mode = 0o4755;
        f.xattrs = vec![
            ("security.capability".into(), hex(&cap)),
            ("user.note".into(), hex(b"hello")),
        ];
        let mut tmp = entry("tmp", EntryKind::Dir, 0o1777);
        tmp.uid = 7;
        let items = built(vec![f, tmp]);
        let ping = &items["usr/bin/ping"];
        assert_eq!(ping.mode, 0o104755);
        assert_eq!((ping.uid, ping.gid), (100_000, 100_001));
        assert_eq!(
            xattr(&items, "usr/bin/ping", "security.capability"),
            Some(cap)
        );
        assert_eq!(
            xattr(&items, "usr/bin/ping", "user.note"),
            Some(b"hello".to_vec())
        );
        assert!(xattr(&items, "usr/bin/ping", "trusted.overlay.redirect").is_some());
        assert_eq!(items["tmp"].mode, 0o41777);
        assert_eq!(items["tmp"].uid, 7);
    }

    #[test]
    fn a_big_directory_spans_blocks_in_name_order() {
        let mut list = Vec::new();
        for i in 0..3000u32 {
            let len = 1 + (i as usize * 7) % 255;
            let mut name: String = format!("{i:x}");
            while name.len() < len {
                name.push(match i % 3 {
                    0 => 'x',
                    1 => ' ',
                    _ => '\n',
                });
            }
            name.truncate(len.max(format!("{i:x}").len()));
            list.push(file(&format!("big/{name}"), i % 5, 1));
        }
        let names: std::collections::BTreeSet<String> =
            list.iter().map(|e| e.path.clone()).collect();
        let items = built(list);
        let got: std::collections::BTreeSet<String> = items
            .keys()
            .filter(|k| k.starts_with("big/"))
            .cloned()
            .collect();
        assert_eq!(got, names);
        assert!(
            items["big"].dir_blocks >= 2,
            "{:?}",
            items["big"].dir_blocks
        );
    }

    #[test]
    fn long_symlinks_use_data_blocks() {
        // 4090 bytes cannot sit next to a 64-byte inode in one 4096-byte block.
        let long = "a/".repeat(2045);
        let short = "b".repeat(100);
        let items = built(vec![
            entry(
                "long",
                EntryKind::Symlink {
                    target: long.clone(),
                },
                0o777,
            ),
            entry(
                "short",
                EntryKind::Symlink {
                    target: short.clone(),
                },
                0o777,
            ),
        ]);
        assert_eq!(items["long"].layout, PLAIN);
        assert_eq!(items["long"].target.as_deref(), Some(long.as_bytes()));
        assert_eq!(items["short"].layout, INLINE);
        assert_eq!(items["short"].target.as_deref(), Some(short.as_bytes()));
    }

    #[test]
    fn huge_files_need_one_chunk() {
        let items = built(vec![file("five", 1, 5 << 30), file("huge", 2, 1 << 44)]);
        assert_eq!(items["five"].chunks.len(), 1);
        assert_eq!(items["five"].size, 5 << 30);
        assert_eq!(items["huge"].chunks, vec![u32::MAX, u32::MAX]);
    }

    fn padded_tree() -> Vec<Entry> {
        let mut list = Vec::new();
        for i in 0..600u32 {
            let mut f = file(&format!("d{}/f{i}", i % 50), i, 10 + u64::from(i));
            f.xattrs = vec![("user.pad".into(), hex(&vec![b'p'; 300]))];
            list.push(f);
        }
        list
    }

    #[test]
    fn inodes_never_cross_a_block_boundary() {
        // The reader asserts the rule for every inode.
        let items = built(padded_tree());
        assert_eq!(items.len(), 1 + 50 + 600);
    }

    #[test]
    fn entries_that_cannot_be_stored_are_rejected() {
        let mut bad_xattr = file("x", 1, 1);
        bad_xattr.xattrs = vec![("trusted.x".into(), hex(b"1"))];
        let mut big_xattr = file("y", 1, 1);
        big_xattr.xattrs = vec![("user.big".into(), hex(&vec![0u8; 5000]))];
        for (what, list) in [
            ("long name", vec![file(&"n".repeat(256), 1, 1)]),
            (
                "empty symlink",
                vec![entry(
                    "l",
                    EntryKind::Symlink {
                        target: String::new(),
                    },
                    0o777,
                )],
            ),
            ("trusted xattr", vec![bad_xattr]),
            ("huge xattr", vec![big_xattr]),
        ] {
            let err = build(&tree(list), 0).unwrap_err();
            assert_eq!(err.kind, ErrorKind::InvalidInput, "{what}: {err:?}");
        }
    }

    #[test]
    fn built_images_pass_fsck_erofs() {
        let Some(fsck) = std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("fsck.erofs"))
                .find(|f| f.is_file())
        }) else {
            eprintln!("fsck.erofs is not installed: skipped");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut list = padded_tree();
        for (i, name) in odd_names().iter().enumerate() {
            list.push(file(&format!("odd/{name}"), i as u32, 7));
        }
        list.push(file(&format!("deep/{}leaf", "d/".repeat(250)), 1, 3));
        list.push(entry(
            "link",
            EntryKind::Symlink {
                target: "d1/f1".into(),
            },
            0o777,
        ));
        list.push(entry("fifo", EntryKind::Fifo, 0o644));
        list.push(file("empty", 9999, 0));
        let path = tmp.path().join("t.erofs");
        std::fs::write(&path, build(&tree(list), 1).unwrap()).unwrap();
        let out = std::process::Command::new(fsck)
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
