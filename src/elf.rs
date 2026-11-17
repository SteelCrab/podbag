//! A small ELF reader: CPU type, program interpreter (`PT_INTERP`) and needed libraries
//! (`DT_NEEDED`). Image files are untrusted, so every offset and count is checked, and
//! only the ranges needed are read.

/// The largest single read the reader makes.
pub const MAX_READ: usize = 64 * 1024;
/// More program headers than this is not a real program.
const MAX_PHNUM: usize = 512;
const MAX_DYN_ENTRIES: usize = 4096;
const MAX_NAME: usize = 4096;
/// At most this many distinct needed libraries and library directories are kept: a
/// hostile file could otherwise name thousands of each and make lookups explode.
pub const MAX_NEEDED: usize = 256;
pub const MAX_RUNPATH: usize = 64;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Machine {
    X86_64,
    Aarch64,
    Other(u16),
}

impl Machine {
    fn from_e_machine(e: u16) -> Machine {
        match e {
            62 => Machine::X86_64,
            183 => Machine::Aarch64,
            other => Machine::Other(other),
        }
    }

    /// The CPU type this program was built for, as `uname -m` would say.
    pub fn name(&self) -> String {
        match self {
            Machine::X86_64 => "x86_64".into(),
            Machine::Aarch64 => "aarch64".into(),
            Machine::Other(3) => "i386".into(),
            Machine::Other(40) => "arm".into(),
            Machine::Other(243) => "riscv64".into(),
            Machine::Other(n) => format!("ELF machine {n}"),
        }
    }

    /// The CPU this openpod runs on.
    pub fn host() -> Machine {
        if cfg!(target_arch = "x86_64") {
            Machine::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Machine::Aarch64
        } else {
            Machine::Other(0)
        }
    }

    /// Whether a CPU of this kind runs programs built for `file`: its own, and the 32-bit
    /// programs of the same family (i386 on x86_64, arm on aarch64), which multilib
    /// images carry.
    pub fn runs(&self, file: Machine) -> bool {
        *self == file
            || matches!(
                (self, file),
                (Machine::X86_64, Machine::Other(3)) | (Machine::Aarch64, Machine::Other(40))
            )
    }

    /// The machine an OCI architecture name stands for (`amd64`, `arm64`).
    pub fn from_oci(arch: &str) -> Machine {
        match arch {
            "amd64" | "x86_64" => Machine::X86_64,
            "arm64" | "aarch64" => Machine::Aarch64,
            "386" => Machine::Other(3),
            "arm" => Machine::Other(40),
            "riscv64" => Machine::Other(243),
            _ => Machine::Other(0),
        }
    }
}

/// Which C library a program needs, from its interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Libc {
    Musl,
    Glibc,
    Unknown,
}

impl Libc {
    pub fn name(&self) -> &'static str {
        match self {
            Libc::Musl => "musl",
            Libc::Glibc => "glibc",
            Libc::Unknown => "an unknown C library",
        }
    }
}

pub fn libc_of_interp(interp: &str) -> Libc {
    let file = interp.rsplit('/').next().unwrap_or(interp);
    if file.starts_with("ld-musl") {
        Libc::Musl
    } else if file.starts_with("ld-linux") || file.starts_with("ld64.so") || file == "ld.so.1" {
        Libc::Glibc
    } else {
        Libc::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfInfo {
    pub machine: Machine,
    /// 32 or 64.
    pub bits: u8,
    /// The program interpreter (dynamic loader); `None` for static programs.
    pub interp: Option<String>,
    /// Shared libraries the program names (`DT_NEEDED`).
    pub needed: Vec<String>,
    /// Extra library directories (`DT_RUNPATH`/`DT_RPATH`), as written (`$ORIGIN` kept).
    pub runpath: Vec<String>,
}

struct Reader {
    big: bool,
}

impl Reader {
    fn u16(&self, b: &[u8], at: usize) -> Option<u16> {
        let a: [u8; 2] = b.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.big {
            u16::from_be_bytes(a)
        } else {
            u16::from_le_bytes(a)
        })
    }
    fn u32(&self, b: &[u8], at: usize) -> Option<u32> {
        let a: [u8; 4] = b.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.big {
            u32::from_be_bytes(a)
        } else {
            u32::from_le_bytes(a)
        })
    }
    fn u64(&self, b: &[u8], at: usize) -> Option<u64> {
        let a: [u8; 8] = b.get(at..at.checked_add(8)?)?.try_into().ok()?;
        Some(if self.big {
            u64::from_be_bytes(a)
        } else {
            u64::from_le_bytes(a)
        })
    }
    /// A 32- or 64-bit field.
    fn word(&self, b: &[u8], at: usize, wide: bool) -> Option<u64> {
        if wide {
            self.u64(b, at)
        } else {
            self.u32(b, at).map(u64::from)
        }
    }
}

/// A NUL-terminated string at the start of `b`.
fn c_string(b: &[u8]) -> Option<String> {
    let end = b.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&b[..end]).into_owned())
}

/// Read an ELF file through `read_at(offset, length)`, which returns the bytes there (fewer
/// at the end of the file) or `None` past it. `None`: not an ELF file, or a damaged one.
pub fn read_elf(read_at: &dyn Fn(u64, usize) -> Option<Vec<u8>>) -> Option<ElfInfo> {
    let h = read_at(0, 64)?;
    if h.len() < 20 || h[..4] != *b"\x7fELF" {
        return None;
    }
    let wide = match h[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let r = Reader {
        big: match h[5] {
            1 => false,
            2 => true,
            _ => return None,
        },
    };
    let machine = Machine::from_e_machine(r.u16(&h, 18)?);
    let bits = if wide { 64 } else { 32 };
    let mut info = ElfInfo {
        machine,
        bits,
        interp: None,
        needed: Vec::new(),
        runpath: Vec::new(),
    };
    let (phoff, phentsize, phnum) = if wide {
        (r.u64(&h, 32), r.u16(&h, 54), r.u16(&h, 56))
    } else {
        (r.u32(&h, 28).map(u64::from), r.u16(&h, 42), r.u16(&h, 44))
    };
    let (Some(phoff), Some(phentsize), Some(phnum)) = (phoff, phentsize, phnum) else {
        return Some(info); // a header without program headers
    };
    let (phentsize, phnum) = (usize::from(phentsize), usize::from(phnum));
    if phnum == 0 {
        return Some(info);
    }
    let min_entry = if wide { 56 } else { 32 };
    if phnum > MAX_PHNUM || phentsize < min_entry {
        return None;
    }
    let table = read_at(phoff, phnum * phentsize)?;
    if table.len() < phnum * phentsize {
        return None;
    }
    let mut loads = Vec::new();
    let mut dynamic = None;
    for i in 0..phnum {
        let at = i * phentsize;
        let ty = r.u32(&table, at)?;
        let (offset, vaddr, filesz) = if wide {
            (
                r.u64(&table, at + 8)?,
                r.u64(&table, at + 16)?,
                r.u64(&table, at + 32)?,
            )
        } else {
            (
                u64::from(r.u32(&table, at + 4)?),
                u64::from(r.u32(&table, at + 8)?),
                u64::from(r.u32(&table, at + 16)?),
            )
        };
        match ty {
            PT_LOAD => loads.push((vaddr, offset, filesz)),
            PT_INTERP => {
                let len = usize::try_from(filesz).ok()?.min(MAX_NAME);
                info.interp = read_at(offset, len).as_deref().and_then(c_string);
            }
            PT_DYNAMIC => dynamic = Some((offset, filesz)),
            _ => {}
        }
    }
    let Some((offset, filesz)) = dynamic else {
        return Some(info);
    };
    let entry = if wide { 16 } else { 8 };
    let len = usize::try_from(filesz).unwrap_or(MAX_READ).min(MAX_READ);
    let Some(table) = read_at(offset, len) else {
        return Some(info);
    };
    let mut names = Vec::new();
    let mut paths = Vec::new();
    let mut strtab = None;
    for i in 0..(table.len() / entry).min(MAX_DYN_ENTRIES) {
        let at = i * entry;
        let (Some(tag), Some(val)) = (
            r.word(&table, at, wide),
            r.word(&table, at + entry / 2, wide),
        ) else {
            break;
        };
        match tag {
            DT_NULL => break,
            DT_NEEDED => names.push(val),
            DT_RPATH | DT_RUNPATH => paths.push(val),
            DT_STRTAB => strtab = Some(val),
            _ => {}
        }
    }
    // DT_STRTAB is an address: find the file offset of the segment that holds it.
    let Some(strtab) = strtab.and_then(|addr| {
        loads
            .iter()
            .find(|(vaddr, _, size)| addr >= *vaddr && addr - vaddr < *size)
            .map(|(vaddr, off, _)| addr - vaddr + off)
    }) else {
        return Some(info);
    };
    let string = |off: u64, max: usize| {
        strtab
            .checked_add(off)
            .and_then(|at| read_at(at, max))
            .as_deref()
            .and_then(c_string)
            .filter(|s| !s.is_empty())
    };
    // Many entries may point at the same string: read each offset once.
    let distinct = |mut offsets: Vec<u64>| {
        let mut seen = std::collections::HashSet::new();
        offsets.retain(|o| seen.insert(*o));
        offsets
    };
    let mut seen = std::collections::HashSet::new();
    info.needed = distinct(names)
        .into_iter()
        .filter_map(|n| string(n, 256))
        .filter(|n| seen.insert(n.clone()))
        .take(MAX_NEEDED)
        .collect();
    let mut seen = std::collections::HashSet::new();
    for p in distinct(paths) {
        let Some(p) = string(p, MAX_NAME) else {
            continue;
        };
        for dir in p.split(':').filter(|d| !d.is_empty()) {
            if info.runpath.len() < MAX_RUNPATH && seen.insert(dir.to_string()) {
                info.runpath.push(dir.to_string());
            }
        }
    }
    Some(info)
}

/// `read_elf` over bytes in memory.
pub fn read_elf_bytes(bytes: &[u8]) -> Option<ElfInfo> {
    read_elf(&|off, len| {
        let start = usize::try_from(off).ok()?;
        if start >= bytes.len() {
            return None;
        }
        let end = start.saturating_add(len).min(bytes.len());
        Some(bytes[start..end].to_vec())
    })
}

#[cfg(test)]
/// A 64-bit little-endian ELF: one PT_LOAD over the whole file, then optionally
/// PT_INTERP and PT_DYNAMIC (DT_NEEDED entries and DT_STRTAB).
pub(crate) fn elf64(machine: u16, interp: Option<&str>, needed: &[&str]) -> Vec<u8> {
    elf64_runpath(machine, interp, needed, None)
}

/// `elf64` with a DT_RUNPATH entry.
#[cfg(test)]
pub(crate) fn elf64_runpath(
    machine: u16,
    interp: Option<&str>,
    needed: &[&str],
    runpath: Option<&str>,
) -> Vec<u8> {
    let base: u64 = 0x40_0000;
    let dynamic = !needed.is_empty() || runpath.is_some();
    let phnum = 1 + interp.is_some() as u64 + dynamic as u64;
    let mut data = Vec::new();
    let data_start = 64 + 56 * phnum;
    let interp_off = data_start;
    if let Some(i) = interp {
        data.extend_from_slice(i.as_bytes());
        data.push(0);
    }
    let strtab_off = data_start + data.len() as u64;
    let mut name_offs = Vec::new();
    data.push(0);
    for n in needed {
        name_offs.push(data.len() as u64 - (strtab_off - data_start));
        data.extend_from_slice(n.as_bytes());
        data.push(0);
    }
    let runpath_off = data.len() as u64 - (strtab_off - data_start);
    if let Some(r) = runpath {
        data.extend_from_slice(r.as_bytes());
        data.push(0);
    }
    while data.len() % 8 != 0 {
        data.push(0);
    }
    let dyn_off = data_start + data.len() as u64;
    for off in &name_offs {
        data.extend_from_slice(&1i64.to_le_bytes());
        data.extend_from_slice(&off.to_le_bytes());
    }
    if runpath.is_some() {
        data.extend_from_slice(&29i64.to_le_bytes());
        data.extend_from_slice(&runpath_off.to_le_bytes());
    }
    data.extend_from_slice(&5i64.to_le_bytes());
    data.extend_from_slice(&(base + strtab_off).to_le_bytes());
    data.extend_from_slice(&[0u8; 16]);
    let dyn_len = data_start + data.len() as u64 - dyn_off;
    let total = data_start + data.len() as u64;

    let mut b = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0];
    b.resize(16, 0);
    b.extend_from_slice(&2u16.to_le_bytes()); // e_type: EXEC
    b.extend_from_slice(&machine.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&base.to_le_bytes()); // e_entry
    b.extend_from_slice(&64u64.to_le_bytes()); // e_phoff
    b.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    b.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    b.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
    b.extend_from_slice(&56u16.to_le_bytes()); // e_phentsize
    b.extend_from_slice(&(phnum as u16).to_le_bytes());
    b.extend_from_slice(&[0u8; 6]); // shentsize, shnum, shstrndx
    let ph = |b: &mut Vec<u8>, ty: u32, off: u64, len: u64| {
        b.extend_from_slice(&ty.to_le_bytes());
        b.extend_from_slice(&5u32.to_le_bytes());
        b.extend_from_slice(&off.to_le_bytes());
        b.extend_from_slice(&(base + off).to_le_bytes());
        b.extend_from_slice(&(base + off).to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
        b.extend_from_slice(&0x1000u64.to_le_bytes());
    };
    ph(&mut b, 1, 0, total);
    if let Some(i) = interp {
        ph(&mut b, 3, interp_off, i.len() as u64 + 1);
    }
    if dynamic {
        ph(&mut b, 2, dyn_off, dyn_len);
    }
    b.extend_from_slice(&data);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_interpreter_needed_and_machine() {
        let bytes = elf64(
            183,
            Some("/lib/ld-musl-aarch64.so.1"),
            &["libc.musl-aarch64.so.1", "libz.so.1"],
        );
        let info = read_elf_bytes(&bytes).unwrap();
        assert_eq!(info.machine, Machine::Aarch64);
        assert_eq!(info.bits, 64);
        assert_eq!(info.interp.as_deref(), Some("/lib/ld-musl-aarch64.so.1"));
        assert_eq!(info.needed, ["libc.musl-aarch64.so.1", "libz.so.1"]);
    }

    #[test]
    fn static_programs_have_no_interpreter() {
        let info = read_elf_bytes(&elf64(62, None, &[])).unwrap();
        assert_eq!(info.machine, Machine::X86_64);
        assert_eq!(info.interp, None);
        assert!(info.needed.is_empty());
    }

    #[test]
    fn other_classes_and_byte_orders() {
        // 32-bit little-endian ARM, header only.
        let mut b = vec![0x7f, b'E', b'L', b'F', 1, 1, 1, 0];
        b.resize(52, 0);
        b[18..20].copy_from_slice(&40u16.to_le_bytes());
        let info = read_elf_bytes(&b).unwrap();
        assert_eq!((info.machine, info.bits), (Machine::Other(40), 32));
        // 64-bit big-endian (s390x = 22), header only.
        let mut b = vec![0x7f, b'E', b'L', b'F', 2, 2, 1, 0];
        b.resize(64, 0);
        b[18..20].copy_from_slice(&22u16.to_be_bytes());
        assert_eq!(read_elf_bytes(&b).unwrap().machine, Machine::Other(22));
    }

    #[test]
    fn truncated_and_hostile_elf_files_are_rejected() {
        assert!(read_elf_bytes(b"#!/bin/sh\necho hi\n").is_none());
        assert!(read_elf_bytes(&[0x7f, b'E', b'L', b'F']).is_none());
        let good = elf64(62, Some("/lib64/ld-linux-x86-64.so.2"), &["libc.so.6"]);
        // Program headers cut off.
        assert!(read_elf_bytes(&good[..100]).is_none());
        // A huge program header count.
        let mut many = good.clone();
        many[56..58].copy_from_slice(&65535u16.to_le_bytes());
        assert!(read_elf_bytes(&many).is_none());
        // Program header offset far out of range.
        let mut far = good.clone();
        far[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(read_elf_bytes(&far).is_none());
        // Dynamic section cut off: the interpreter is still known, no libraries.
        let cut = &good[..good.len() - 24];
        let info = read_elf_bytes(cut).unwrap();
        assert_eq!(info.interp.as_deref(), Some("/lib64/ld-linux-x86-64.so.2"));
        // A string offset out of range is skipped.
        let mut bad_str = good.clone();
        let n = bad_str.len();
        // The first dynamic entry (DT_NEEDED) value sits 48 bytes before the end.
        bad_str[n - 40..n - 32].copy_from_slice(&0xffff_ffffu64.to_le_bytes());
        let info = read_elf_bytes(&bad_str).unwrap();
        assert!(info.needed.is_empty(), "{:?}", info.needed);
    }

    #[test]
    fn reads_only_small_ranges() {
        let bytes = elf64(
            183,
            Some("/lib/ld-musl-aarch64.so.1"),
            &["libc.musl-aarch64.so.1"],
        );
        let biggest = std::cell::Cell::new(0usize);
        let read_at = |off: u64, len: usize| {
            biggest.set(biggest.get().max(len));
            let start = usize::try_from(off).ok()?;
            bytes
                .get(start..start.checked_add(len)?.min(bytes.len()))
                .map(<[u8]>::to_vec)
        };
        read_elf(&read_at).unwrap();
        assert!(biggest.get() <= MAX_READ, "{}", biggest.get());
    }

    #[test]
    fn library_lists_are_deduplicated_and_bounded() {
        let names: Vec<String> = (0..1000).map(|i| format!("libm{i}.so")).collect();
        let mut needed: Vec<&str> = names.iter().map(String::as_str).collect();
        needed.extend(["libm1.so", "libm1.so"]);
        let dirs: Vec<String> = (0..300).map(|i| format!("/d/{i}")).collect();
        let runpath = format!("{}:/d/1:/d/1", dirs.join(":"));
        let info = read_elf_bytes(&elf64_runpath(183, None, &needed, Some(&runpath))).unwrap();
        assert_eq!(info.needed.len(), MAX_NEEDED, "{}", info.needed.len());
        assert_eq!(info.runpath.len(), MAX_RUNPATH, "{}", info.runpath.len());
        let few = read_elf_bytes(&elf64_runpath(
            183,
            None,
            &["a.so", "b.so", "a.so"],
            Some("/x:/y:/x"),
        ))
        .unwrap();
        assert_eq!(few.needed, ["a.so", "b.so"]);
        assert_eq!(few.runpath, ["/x", "/y"]);
    }

    #[test]
    fn libc_from_interpreter_names() {
        assert_eq!(libc_of_interp("/lib/ld-musl-x86_64.so.1"), Libc::Musl);
        assert_eq!(libc_of_interp("/lib64/ld-linux-x86-64.so.2"), Libc::Glibc);
        assert_eq!(libc_of_interp("/lib/ld-linux-aarch64.so.1"), Libc::Glibc);
        assert_eq!(libc_of_interp("/lib64/ld64.so.2"), Libc::Glibc);
        assert_eq!(libc_of_interp("/system/bin/linker64"), Libc::Unknown);
    }
}
