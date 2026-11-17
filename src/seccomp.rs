//! The default seccomp filter: a deny list of dangerous system calls, as classic BPF.
//! The program is built here for each CPU type; installing it is Linux-only.

/// CPU types openpod builds filters for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

/// The CPU type of this build, if a filter exists for it.
pub fn native() -> Option<Arch> {
    if cfg!(target_arch = "x86_64") {
        Some(Arch::X86_64)
    } else if cfg!(target_arch = "aarch64") {
        Some(Arch::Aarch64)
    } else {
        None
    }
}

/// One classic BPF instruction (`struct sock_filter`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// Filter results (`SECCOMP_RET_*`).
pub const RET_ALLOW: u32 = 0x7fff_0000;
pub const RET_ERRNO: u32 = 0x0005_0000;

const EPERM: u32 = 1;
const ENOSYS: u32 = 38;

/// `AUDIT_ARCH_*` values in `seccomp_data.arch`.
fn audit_arch(arch: Arch) -> u32 {
    match arch {
        Arch::X86_64 => 0xc000_003e,
        Arch::Aarch64 => 0xc000_00b7,
    }
}

/// Capability numbers (linux/capability.h) that lift a rule, as in Docker's profile.
const SYS_ADMIN: u64 = 1 << 21;
const SYS_TIME: u64 = 1 << 25;
const SYS_MODULE: u64 = 1 << 16;
const SYS_PTRACE: u64 = 1 << 19;
const SYS_BOOT: u64 = 1 << 22;
const SYS_PACCT: u64 = 1 << 20;
const SYS_RAWIO: u64 = 1 << 17;
const SYS_TTY_CONFIG: u64 = 1 << 26;
const SYSLOG: u64 = 1 << 34;
const PERFMON: u64 = 1 << 38;
const BPF: u64 = 1 << 39;
const DAC_READ_SEARCH: u64 = 1 << 2;
/// Refused whatever the capabilities.
const ALWAYS: u64 = 0;

/// System calls the filter refuses with EPERM: name, number on x86_64 and on aarch64
/// (`None`: the call does not exist there), and the capabilities that lift the rule (any
/// one of them). Docker's default profile refuses these too, or allows them only with
/// those capabilities.
const TABLE: &[(&str, Option<u32>, Option<u32>, u64)] = &[
    ("acct", Some(163), Some(89), SYS_PACCT),
    ("add_key", Some(248), Some(217), ALWAYS),
    ("adjtimex", Some(159), Some(171), SYS_TIME),
    ("bpf", Some(321), Some(280), SYS_ADMIN | BPF),
    ("clock_adjtime", Some(305), Some(266), SYS_TIME),
    ("clock_settime", Some(227), Some(112), SYS_TIME),
    ("create_module", Some(174), None, ALWAYS),
    ("delete_module", Some(176), Some(106), SYS_MODULE),
    ("finit_module", Some(313), Some(273), SYS_MODULE),
    ("fsconfig", Some(431), Some(431), SYS_ADMIN),
    ("fsmount", Some(432), Some(432), SYS_ADMIN),
    ("fsopen", Some(430), Some(430), SYS_ADMIN),
    ("fspick", Some(433), Some(433), SYS_ADMIN),
    ("get_kernel_syms", Some(177), None, ALWAYS),
    ("init_module", Some(175), Some(105), SYS_MODULE),
    ("io_uring_enter", Some(426), Some(426), ALWAYS),
    ("io_uring_register", Some(427), Some(427), ALWAYS),
    ("io_uring_setup", Some(425), Some(425), ALWAYS),
    ("ioperm", Some(173), None, SYS_RAWIO),
    ("iopl", Some(172), None, SYS_RAWIO),
    ("kexec_file_load", Some(320), Some(294), ALWAYS),
    ("kexec_load", Some(246), Some(104), ALWAYS),
    ("keyctl", Some(250), Some(219), ALWAYS),
    ("lookup_dcookie", Some(212), Some(18), SYS_ADMIN),
    ("mount", Some(165), Some(40), SYS_ADMIN),
    ("mount_setattr", Some(442), Some(442), SYS_ADMIN),
    ("move_mount", Some(429), Some(429), SYS_ADMIN),
    ("nfsservctl", Some(180), None, ALWAYS),
    ("open_by_handle_at", Some(304), Some(265), DAC_READ_SEARCH),
    ("open_tree", Some(428), Some(428), SYS_ADMIN),
    ("perf_event_open", Some(298), Some(241), SYS_ADMIN | PERFMON),
    ("pivot_root", Some(155), Some(41), SYS_ADMIN),
    ("process_vm_readv", Some(310), Some(270), SYS_PTRACE),
    ("process_vm_writev", Some(311), Some(271), SYS_PTRACE),
    ("query_module", Some(178), None, ALWAYS),
    ("quotactl", Some(179), Some(60), SYS_ADMIN),
    ("quotactl_fd", Some(443), Some(443), SYS_ADMIN),
    ("reboot", Some(169), Some(142), SYS_BOOT),
    ("request_key", Some(249), Some(218), ALWAYS),
    ("setns", Some(308), Some(268), SYS_ADMIN),
    ("settimeofday", Some(164), Some(170), SYS_TIME),
    ("swapoff", Some(168), Some(225), ALWAYS),
    ("swapon", Some(167), Some(224), ALWAYS),
    ("_sysctl", Some(156), None, ALWAYS),
    ("syslog", Some(103), Some(116), SYS_ADMIN | SYSLOG),
    ("umount2", Some(166), Some(39), SYS_ADMIN),
    ("unshare", Some(272), Some(97), SYS_ADMIN),
    ("uselib", Some(134), None, ALWAYS),
    ("userfaultfd", Some(323), Some(282), SYS_PTRACE),
    ("vhangup", Some(153), Some(58), SYS_TTY_CONFIG),
];

/// The names of the refused system calls.
pub const DENIED: [&str; TABLE.len()] = {
    let mut names = [""; TABLE.len()];
    let mut i = 0;
    while i < TABLE.len() {
        names[i] = TABLE[i].0;
        i += 1;
    }
    names
};

/// The refused system calls that exist on `arch`, with their numbers (with no extra
/// capabilities granted).
pub fn numbers(arch: Arch) -> Vec<(&'static str, u32)> {
    TABLE
        .iter()
        .filter_map(|&(name, x86, arm, _)| {
            let nr = match arch {
                Arch::X86_64 => x86,
                Arch::Aarch64 => arm,
            };
            Some((name, nr?))
        })
        .collect()
}

/// `clone3` has the same number everywhere. Its flags sit in memory, out of the filter's
/// reach, so it gets ENOSYS and the C library falls back to `clone` (as with Docker).
const CLONE3: u32 = 435;

fn clone_number(arch: Arch) -> u32 {
    match arch {
        Arch::X86_64 => 56,
        Arch::Aarch64 => 220,
    }
}

/// `clone` flags that create namespaces: NEWNS, NEWCGROUP, NEWUTS, NEWIPC, NEWUSER,
/// NEWPID, NEWNET.
const NAMESPACE_FLAGS: u32 =
    0x0002_0000 | 0x0200_0000 | 0x0400_0000 | 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000;

/// On x86_64, numbers from here on are x32 system calls.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

// `struct seccomp_data` offsets: nr, arch, then args[0] (low half on little-endian CPUs).
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARG0_LOW: u32 = 16;

fn load(offset: u32) -> Insn {
    Insn {
        code: 0x20,
        jt: 0,
        jf: 0,
        k: offset,
    } // BPF_LD | BPF_W | BPF_ABS
}

fn jeq(k: u32, jt: u8, jf: u8) -> Insn {
    Insn {
        code: 0x15,
        jt,
        jf,
        k,
    } // BPF_JMP | BPF_JEQ | BPF_K
}

fn jge(k: u32, jt: u8, jf: u8) -> Insn {
    Insn {
        code: 0x35,
        jt,
        jf,
        k,
    } // BPF_JMP | BPF_JGE | BPF_K
}

fn jset(k: u32, jt: u8, jf: u8) -> Insn {
    Insn {
        code: 0x45,
        jt,
        jf,
        k,
    } // BPF_JMP | BPF_JSET | BPF_K
}

fn ret(k: u32) -> Insn {
    Insn {
        code: 0x06,
        jt: 0,
        jf: 0,
        k,
    } // BPF_RET | BPF_K
}

/// The default filter for `arch`, for a container that keeps the capabilities `caps`:
/// rules lifted by a granted capability are left out, as Docker does. Every jump is short
/// (at most 4), so the program has no offsets near the 255 limit.
pub fn deny_filter(arch: Arch, caps: u64) -> Vec<Insn> {
    let mut p = vec![
        load(OFFSET_ARCH),
        jeq(audit_arch(arch), 1, 0),
        ret(RET_ERRNO | ENOSYS),
        load(OFFSET_NR),
    ];
    if arch == Arch::X86_64 {
        p.extend([jge(X32_SYSCALL_BIT, 0, 1), ret(RET_ERRNO | ENOSYS)]);
    }
    for &(_, x86, arm, lifted_by) in TABLE {
        let nr = match arch {
            Arch::X86_64 => x86,
            Arch::Aarch64 => arm,
        };
        if let Some(nr) = nr.filter(|_| caps & lifted_by == 0) {
            p.extend([jeq(nr, 0, 1), ret(RET_ERRNO | EPERM)]);
        }
    }
    // With CAP_SYS_ADMIN, new namespaces are allowed (Docker's rule for clone and clone3).
    if caps & SYS_ADMIN == 0 {
        p.extend([
            jeq(clone_number(arch), 0, 4),
            load(OFFSET_ARG0_LOW),
            jset(NAMESPACE_FLAGS, 0, 1),
            ret(RET_ERRNO | EPERM),
            ret(RET_ALLOW),
            jeq(CLONE3, 0, 1),
            ret(RET_ERRNO | ENOSYS),
        ]);
    }
    p.push(ret(RET_ALLOW));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUDIT_X86_64: u32 = 0xc000003e;
    const AUDIT_AARCH64: u32 = 0xc00000b7;
    const EPERM: u32 = 1;
    const ENOSYS: u32 = 38;

    fn audit(arch: Arch) -> u32 {
        match arch {
            Arch::X86_64 => AUDIT_X86_64,
            Arch::Aarch64 => AUDIT_AARCH64,
        }
    }

    /// Run a filter on one system call, like the kernel does.
    fn eval(prog: &[Insn], nr: u32, arch: u32, arg0: u64) -> u32 {
        let load = |offset: u32| match offset {
            0 => nr,
            4 => arch,
            16 => arg0 as u32,
            20 => (arg0 >> 32) as u32,
            other => panic!("load of offset {other}"),
        };
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let i = prog[pc];
            let jump = |taken: bool| pc + 1 + if taken { i.jt } else { i.jf } as usize;
            pc = match i.code {
                0x20 => {
                    acc = load(i.k);
                    pc + 1
                }
                0x15 => jump(acc == i.k),
                0x35 => jump(acc >= i.k),
                0x45 => jump(acc & i.k != 0),
                0x05 => pc + 1 + i.k as usize,
                0x06 => return i.k,
                other => panic!("unknown instruction {other:#x}"),
            };
        }
    }

    #[test]
    fn denies_listed_calls_with_eperm() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let p = deny_filter(arch, crate::caps::DEFAULT);
            for (name, nr) in numbers(arch) {
                assert_eq!(
                    eval(&p, nr, audit(arch), 0),
                    RET_ERRNO | EPERM,
                    "{arch:?} {name}"
                );
            }
        }
        // kexec_load
        let x86 = eval(
            &deny_filter(Arch::X86_64, crate::caps::DEFAULT),
            246,
            AUDIT_X86_64,
            0,
        );
        assert_eq!(x86, RET_ERRNO | EPERM);
        let arm = eval(
            &deny_filter(Arch::Aarch64, crate::caps::DEFAULT),
            104,
            AUDIT_AARCH64,
            0,
        );
        assert_eq!(arm, RET_ERRNO | EPERM);
    }

    #[test]
    fn allows_other_calls() {
        let x86 = deny_filter(Arch::X86_64, crate::caps::DEFAULT);
        for nr in [0, 1, 59, 117] {
            // read, write, execve, setresuid
            assert_eq!(eval(&x86, nr, AUDIT_X86_64, 0), RET_ALLOW, "x86_64 {nr}");
        }
        let arm = deny_filter(Arch::Aarch64, crate::caps::DEFAULT);
        for nr in [63, 64, 221, 147] {
            assert_eq!(eval(&arm, nr, AUDIT_AARCH64, 0), RET_ALLOW, "aarch64 {nr}");
        }
    }

    #[test]
    fn other_arch_gets_enosys() {
        // i386 execve on an x86_64 filter.
        let p = deny_filter(Arch::X86_64, crate::caps::DEFAULT);
        assert_eq!(eval(&p, 11, 0x40000003, 0), RET_ERRNO | ENOSYS);
        assert_eq!(
            eval(
                &deny_filter(Arch::Aarch64, crate::caps::DEFAULT),
                221,
                AUDIT_X86_64,
                0
            ),
            RET_ERRNO | ENOSYS
        );
    }

    #[test]
    fn x32_calls_are_denied() {
        let p = deny_filter(Arch::X86_64, crate::caps::DEFAULT);
        assert_eq!(
            eval(&p, 0x4000_0000 | 246, AUDIT_X86_64, 0),
            RET_ERRNO | ENOSYS
        );
        assert_eq!(eval(&p, 0x4000_0000, AUDIT_X86_64, 0), RET_ERRNO | ENOSYS);
    }

    #[test]
    fn clone_with_namespace_flags_is_denied() {
        for (arch, clone) in [(Arch::X86_64, 56), (Arch::Aarch64, 220)] {
            let p = deny_filter(arch, crate::caps::DEFAULT);
            let newuser = 0x1000_0000;
            assert_eq!(eval(&p, clone, audit(arch), newuser), RET_ERRNO | EPERM);
            let newnet = 0x4000_0000;
            assert_eq!(eval(&p, clone, audit(arch), newnet), RET_ERRNO | EPERM);
            // A thread or a fork: SIGCHLD | CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID.
            assert_eq!(eval(&p, clone, audit(arch), 0x0120_0011), RET_ALLOW);
            assert_eq!(eval(&p, clone, audit(arch), 0x003d_0f00), RET_ALLOW);
        }
    }

    #[test]
    fn clone3_gets_enosys() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            assert_eq!(
                eval(
                    &deny_filter(arch, crate::caps::DEFAULT),
                    435,
                    audit(arch),
                    0
                ),
                RET_ERRNO | ENOSYS
            );
        }
    }

    #[test]
    fn granted_capabilities_lift_their_calls() {
        let cap = |name: &str| 1u64 << crate::caps::number(name).unwrap();
        let def = crate::caps::DEFAULT;
        for (arch, mount, keyctl, settime, clone) in [
            (Arch::X86_64, 165, 250, 164, 56),
            (Arch::Aarch64, 40, 219, 170, 220),
        ] {
            let a = audit(arch);
            let admin = deny_filter(arch, def | cap("SYS_ADMIN"));
            assert_eq!(eval(&admin, mount, a, 0), RET_ALLOW, "{arch:?} mount");
            assert_eq!(
                eval(&admin, clone, a, 0x1000_0000),
                RET_ALLOW,
                "{arch:?} clone"
            );
            assert_eq!(eval(&admin, 435, a, 0), RET_ALLOW, "{arch:?} clone3");
            assert_eq!(
                eval(&admin, keyctl, a, 0),
                RET_ERRNO | EPERM,
                "{arch:?} keyctl"
            );
            assert_eq!(eval(&admin, settime, a, 0), RET_ERRNO | EPERM);
            let time = deny_filter(arch, def | cap("SYS_TIME"));
            assert_eq!(
                eval(&time, settime, a, 0),
                RET_ALLOW,
                "{arch:?} settimeofday"
            );
            assert_eq!(eval(&time, mount, a, 0), RET_ERRNO | EPERM);
        }
        let syslog = deny_filter(Arch::Aarch64, def | cap("SYSLOG"));
        assert_eq!(eval(&syslog, 116, AUDIT_AARCH64, 0), RET_ALLOW);
    }

    #[test]
    fn io_uring_is_denied_even_with_every_capability() {
        let all = (1u64 << (crate::caps::LAST + 1)) - 1;
        for arch in [Arch::X86_64, Arch::Aarch64] {
            for nr in [425, 426, 427] {
                assert_eq!(
                    eval(&deny_filter(arch, all), nr, audit(arch), 0),
                    RET_ERRNO | EPERM,
                    "{arch:?} {nr}"
                );
            }
        }
    }

    #[test]
    fn name_to_handle_at_is_allowed_like_docker() {
        let x86 = deny_filter(Arch::X86_64, crate::caps::DEFAULT);
        assert_eq!(eval(&x86, 303, AUDIT_X86_64, 0), RET_ALLOW);
        let arm = deny_filter(Arch::Aarch64, crate::caps::DEFAULT);
        assert_eq!(eval(&arm, 264, AUDIT_AARCH64, 0), RET_ALLOW);
    }

    #[test]
    fn every_denied_name_has_a_number_somewhere() {
        for name in DENIED {
            let found = [Arch::X86_64, Arch::Aarch64]
                .iter()
                .any(|&a| numbers(a).iter().any(|(n, _)| *n == name));
            assert!(found, "{name}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn numbers_match_libc() {
        let arch = native().unwrap();
        let libc_number = |name: &str| -> Option<libc::c_long> {
            Some(match name {
                "acct" => libc::SYS_acct,
                "add_key" => libc::SYS_add_key,
                "adjtimex" => libc::SYS_adjtimex,
                "bpf" => libc::SYS_bpf,
                "clock_adjtime" => libc::SYS_clock_adjtime,
                "clock_settime" => libc::SYS_clock_settime,
                "delete_module" => libc::SYS_delete_module,
                "finit_module" => libc::SYS_finit_module,
                "fsconfig" => libc::SYS_fsconfig,
                "fsmount" => libc::SYS_fsmount,
                "fsopen" => libc::SYS_fsopen,
                "fspick" => libc::SYS_fspick,
                "io_uring_enter" => libc::SYS_io_uring_enter,
                "io_uring_register" => libc::SYS_io_uring_register,
                "io_uring_setup" => libc::SYS_io_uring_setup,
                "init_module" => libc::SYS_init_module,
                "kexec_file_load" => libc::SYS_kexec_file_load,
                "kexec_load" => libc::SYS_kexec_load,
                "keyctl" => libc::SYS_keyctl,
                "lookup_dcookie" => libc::SYS_lookup_dcookie,
                "mount" => libc::SYS_mount,
                "mount_setattr" => libc::SYS_mount_setattr,
                "move_mount" => libc::SYS_move_mount,
                "name_to_handle_at" => libc::SYS_name_to_handle_at,
                "open_by_handle_at" => libc::SYS_open_by_handle_at,
                "open_tree" => libc::SYS_open_tree,
                "perf_event_open" => libc::SYS_perf_event_open,
                "pivot_root" => libc::SYS_pivot_root,
                "process_vm_readv" => libc::SYS_process_vm_readv,
                "process_vm_writev" => libc::SYS_process_vm_writev,
                "quotactl" => libc::SYS_quotactl,
                "reboot" => libc::SYS_reboot,
                "request_key" => libc::SYS_request_key,
                "setns" => libc::SYS_setns,
                "settimeofday" => libc::SYS_settimeofday,
                "swapoff" => libc::SYS_swapoff,
                "swapon" => libc::SYS_swapon,
                "syslog" => libc::SYS_syslog,
                "umount2" => libc::SYS_umount2,
                "unshare" => libc::SYS_unshare,
                "userfaultfd" => libc::SYS_userfaultfd,
                "vhangup" => libc::SYS_vhangup,
                _ => return None,
            })
        };
        let mut checked = 0;
        for (name, nr) in numbers(arch) {
            if let Some(expected) = libc_number(name) {
                assert_eq!(nr as libc::c_long, expected, "{name}");
                checked += 1;
            }
        }
        assert!(checked >= 39, "{checked}");
        assert_eq!(CLONE3 as libc::c_long, libc::SYS_clone3);
        assert_eq!(clone_number(arch) as libc::c_long, libc::SYS_clone);
    }
}
