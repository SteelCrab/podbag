//! Which devices a root-mode container may open. cgroup v2 has no `devices.*` files: the
//! rules are a BPF program attached to the container's group, as runc and crun do.
//!
//! The rules are runc's defaults, which Docker uses: any device node may be made (`mknod`),
//! and only these may be read or written: /dev/null, zero, full, random, urandom, tty,
//! ptmx, the terminals in /dev/pts, and /dev/net/tun. Without them a container that holds
//! CAP_MKNOD (one of Docker's default capabilities) could make a node for the host's disk
//! and read it. Rootless containers need none of this: the kernel refuses device nodes to
//! a user namespace.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

const BPF_PROG_LOAD: libc::c_long = 5;
const BPF_PROG_ATTACH: libc::c_long = 8;
const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
const BPF_CGROUP_DEVICE: u32 = 6;
/// Groups below may add programs of their own; every one must allow an access.
const BPF_F_ALLOW_MULTI: u32 = 2;

const BLOCK: i32 = 1;
const CHAR: i32 = 2;
const MKNOD: i32 = 1;
const READ: i32 = 2;
const WRITE: i32 = 4;
const ALL: i32 = MKNOD | READ | WRITE;

/// A device rule: its type, major and minor (None: any), and the accesses it allows.
struct Rule(i32, Option<i32>, Option<i32>, i32);

const RULES: &[Rule] = &[
    Rule(CHAR, None, None, MKNOD),
    Rule(BLOCK, None, None, MKNOD),
    Rule(CHAR, Some(1), Some(3), ALL),    // null
    Rule(CHAR, Some(1), Some(5), ALL),    // zero
    Rule(CHAR, Some(1), Some(7), ALL),    // full
    Rule(CHAR, Some(1), Some(8), ALL),    // random
    Rule(CHAR, Some(1), Some(9), ALL),    // urandom
    Rule(CHAR, Some(5), Some(0), ALL),    // tty
    Rule(CHAR, Some(5), Some(2), ALL),    // ptmx
    Rule(CHAR, Some(136), None, ALL),     // pts/*
    Rule(CHAR, Some(10), Some(200), ALL), // net/tun
];

/// One BPF instruction (`struct bpf_insn`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Insn {
    code: u8,
    /// dst_reg in the low four bits, src_reg in the high four (little-endian bitfields).
    regs: u8,
    off: i16,
    imm: i32,
}

const LDX_W: u8 = 0x61; // BPF_LDX | BPF_MEM | BPF_W
const AND32_K: u8 = 0x54; // BPF_ALU | BPF_AND | BPF_K
const RSH32_K: u8 = 0x74; // BPF_ALU | BPF_RSH | BPF_K
const MOV32_X: u8 = 0xbc; // BPF_ALU | BPF_MOV | BPF_X
const MOV32_K: u8 = 0xb4; // BPF_ALU | BPF_MOV | BPF_K
const JNE_K: u8 = 0x55; // BPF_JMP | BPF_JNE | BPF_K
const JNE_X: u8 = 0x5d; // BPF_JMP | BPF_JNE | BPF_X
const EXIT: u8 = 0x95; // BPF_JMP | BPF_EXIT

fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
    #[cfg(target_endian = "little")]
    let regs = (src << 4) | (dst & 0x0f);
    #[cfg(target_endian = "big")]
    let regs = (dst << 4) | (src & 0x0f);
    Insn {
        code,
        regs,
        off,
        imm,
    }
}

/// The program: allow when a rule matches, refuse otherwise. Its input is
/// `struct bpf_cgroup_dev_ctx { u32 access_type; u32 major; u32 minor; }`, where
/// `access_type` holds the access in its high 16 bits and the device type in the low ones.
fn program() -> Vec<Insn> {
    let mut p = vec![
        insn(LDX_W, 2, 1, 0, 0),        // r2 = access_type
        insn(AND32_K, 2, 0, 0, 0xffff), // r2 = type
        insn(LDX_W, 3, 1, 0, 0),        // r3 = access_type
        insn(RSH32_K, 3, 0, 0, 16),     // r3 = access
        insn(LDX_W, 4, 1, 4, 0),        // r4 = major
        insn(LDX_W, 5, 1, 8, 0),        // r5 = minor
    ];
    for &Rule(kind, major, minor, access) in RULES {
        let mut block = vec![insn(JNE_K, 2, 0, 0, kind)];
        if access != ALL {
            // Allowed when every access asked for is in the rule's.
            block.push(insn(MOV32_X, 1, 3, 0, 0));
            block.push(insn(AND32_K, 1, 0, 0, access));
            block.push(insn(JNE_X, 1, 3, 0, 0));
        }
        if let Some(m) = major {
            block.push(insn(JNE_K, 4, 0, 0, m));
        }
        if let Some(m) = minor {
            block.push(insn(JNE_K, 5, 0, 0, m));
        }
        block.push(insn(MOV32_K, 0, 0, 0, 1));
        block.push(insn(EXIT, 0, 0, 0, 0));
        // A mismatch goes to the first instruction after the block.
        let len = block.len();
        for (i, ins) in block.iter_mut().enumerate() {
            if ins.code == JNE_K || ins.code == JNE_X {
                ins.off = (len - i - 1) as i16;
            }
        }
        p.extend(block);
    }
    p.push(insn(MOV32_K, 0, 0, 0, 0));
    p.push(insn(EXIT, 0, 0, 0, 0));
    p
}

/// `union bpf_attr` for BPF_PROG_LOAD, up to `expected_attach_type`.
#[repr(C)]
struct ProgLoad {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
}

/// `union bpf_attr` for BPF_PROG_ATTACH, up to `replace_bpf_fd`.
#[repr(C)]
struct ProgAttach {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
    replace_bpf_fd: u32,
}

/// Attach the device rules to the cgroup open at `group`. They hold for every process
/// that enters it or a group below it, until the group is removed.
pub fn restrict(group: BorrowedFd<'_>) -> std::io::Result<()> {
    let insns = program();
    let license = c"Apache";
    let mut name = [0u8; 16];
    name[..15].copy_from_slice(b"openpod_devices");
    let load = ProgLoad {
        prog_type: BPF_PROG_TYPE_CGROUP_DEVICE,
        insn_cnt: insns.len() as u32,
        insns: insns.as_ptr() as u64,
        license: license.as_ptr() as u64,
        log_level: 0,
        log_size: 0,
        log_buf: 0,
        kern_version: 0,
        prog_flags: 0,
        prog_name: name,
        prog_ifindex: 0,
        expected_attach_type: 0,
    };
    // SAFETY: the attribute points to live instructions and a NUL-terminated license for
    // the duration of the call; its size is given.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_LOAD,
            &load as *const ProgLoad,
            std::mem::size_of::<ProgLoad>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a new descriptor the kernel just gave us.
    let prog = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let attach = ProgAttach {
        target_fd: group.as_raw_fd() as u32,
        attach_bpf_fd: prog.as_raw_fd() as u32,
        attach_type: BPF_CGROUP_DEVICE,
        attach_flags: BPF_F_ALLOW_MULTI,
        replace_bpf_fd: 0,
    };
    // SAFETY: as above; the descriptors are open.
    let r = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_ATTACH,
            &attach as *const ProgAttach,
            std::mem::size_of::<ProgAttach>(),
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The attachment keeps the program; our descriptor goes.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the program on one access, as the kernel would (a small interpreter of the
    /// instructions it uses).
    fn allows(kind: i32, major: i32, minor: i32, access: i32) -> bool {
        let p = program();
        let mut r = [0i64; 11];
        let ctx = [((access << 16) | kind) as i64, major as i64, minor as i64];
        let mut pc = 0usize;
        loop {
            let i = p[pc];
            let (dst, src) = ((i.regs & 0x0f) as usize, (i.regs >> 4) as usize);
            pc += 1;
            match i.code {
                LDX_W => r[dst] = ctx[(i.off / 4) as usize],
                AND32_K => r[dst] &= i.imm as i64,
                RSH32_K => r[dst] >>= i.imm,
                MOV32_X => r[dst] = r[src],
                MOV32_K => r[dst] = i.imm as i64,
                JNE_K if r[dst] != i.imm as i64 => pc += i.off as usize,
                JNE_X if r[dst] != r[src] => pc += i.off as usize,
                JNE_K | JNE_X => {}
                EXIT => return r[0] == 1,
                other => panic!("unexpected instruction {other:#x}"),
            }
        }
    }

    #[test]
    fn the_usual_devices_open_and_others_do_not() {
        for (major, minor) in [
            (1, 3),
            (1, 5),
            (1, 7),
            (1, 8),
            (1, 9),
            (5, 0),
            (5, 2),
            (136, 7),
            (10, 200),
        ] {
            assert!(allows(CHAR, major, minor, READ | WRITE), "{major}:{minor}");
        }
        // The host's disk, a block device: made, never opened.
        assert!(allows(BLOCK, 253, 0, MKNOD));
        assert!(!allows(BLOCK, 253, 0, READ));
        assert!(!allows(BLOCK, 8, 0, READ | WRITE));
        // Other character devices: /dev/mem, /dev/kmsg, /dev/fuse.
        assert!(!allows(CHAR, 1, 1, READ));
        assert!(!allows(CHAR, 1, 11, WRITE));
        assert!(!allows(CHAR, 10, 229, READ | WRITE));
        assert!(allows(CHAR, 1, 1, MKNOD));
        // Asking for more than a rule allows is refused.
        assert!(!allows(BLOCK, 253, 0, MKNOD | READ));
    }
}
