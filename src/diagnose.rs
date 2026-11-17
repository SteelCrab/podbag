//! Why a program that exists could not be started: a missing loader (a glibc program in a
//! musl image), a script whose interpreter is missing, or a program for another CPU
//! (design 6.9). Used when `execve` fails inside the container.

use crate::elf::Machine;
use std::path::Path;

/// How much of the program is read to explain a failure.
pub const HEAD: usize = 8 * 1024;

const ENOENT: i32 = 2;
const ENOEXEC: i32 = 8;
const EINVAL: i32 = 22;

/// Explain why `execve(program)` failed with `errno`, as (why, what to do). `read_head`
/// returns the start of a file (up to `HEAD` bytes), `exists` says whether a path is there
/// (both as the container sees them). `None`: nothing more to say than the plain error.
pub fn diagnose_exec(
    errno: i32,
    program: &Path,
    read_head: impl Fn(&Path) -> Option<Vec<u8>>,
    exists: impl Fn(&Path) -> bool,
    host: Machine,
) -> Option<(String, String)> {
    // Other errors (EACCES for a FIFO or a device...) need no look inside the file, and
    // opening such a file could block.
    if !matches!(errno, ENOENT | ENOEXEC | EINVAL) {
        return None;
    }
    let head = read_head(program)?;
    let shown = program.display();
    if let Some(line) = head.strip_prefix(b"#!") {
        if errno != ENOENT {
            return None;
        }
        let line = line.split(|&b| b == b'\n').next().unwrap_or(&[]);
        let line = String::from_utf8_lossy(line);
        let interp = line.split_whitespace().next()?;
        if !exists(Path::new(interp)) {
            return Some((
                format!("{shown} is a script for {interp}, which is not in the container"),
                format!("install {interp} in the image, or change the first line of {shown}"),
            ));
        }
        // The interpreter is there, but it may need a loader the image does not have
        // (a glibc Python in a musl image).
        let info = crate::elf::read_elf_bytes(&read_head(Path::new(interp))?)?;
        let (libc, loader, next) = missing_loader(&info, &exists)?;
        return Some((
            format!(
                "{shown} is a script for {interp}, which is built for {} and needs {loader}, which is not in the container",
                libc.name()
            ),
            next,
        ));
    }
    let info = crate::elf::read_elf_bytes(&head)?;
    match errno {
        ENOEXEC | EINVAL if info.machine != host => Some((
            format!(
                "{shown} is built for {}, but this computer is {}",
                info.machine.name(),
                host.name()
            ),
            format!(
                "use the image built for {}, or rebuild the program for it",
                host.name()
            ),
        )),
        ENOENT => {
            let (libc, loader, next) = missing_loader(&info, &exists)?;
            Some((
                format!(
                    "{shown} is built for {} and needs {loader}, which is not in the container",
                    libc.name()
                ),
                next,
            ))
        }
        _ => None,
    }
}

/// The program's loader when the image lacks it: its C library, its path, what to do.
fn missing_loader(
    info: &crate::elf::ElfInfo,
    exists: &impl Fn(&Path) -> bool,
) -> Option<(crate::elf::Libc, String, String)> {
    let loader = info.interp.clone()?;
    if exists(Path::new(&loader)) {
        return None;
    }
    let libc = crate::elf::libc_of_interp(&loader);
    let next = match libc {
        crate::elf::Libc::Glibc => {
            "use a glibc-based image (such as debian or ubuntu), or build the program for musl or as a static program"
        }
        crate::elf::Libc::Musl => {
            "use an Alpine-based image, or build the program for glibc or as a static program"
        }
        crate::elf::Libc::Unknown => {
            "add the loader to the image, or build the program as a static program"
        }
    };
    Some((
        libc,
        loader,
        format!("{next}; `openpod check-image` lists such programs"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::elf64;

    const ENOENT: i32 = 2;
    const ENOEXEC: i32 = 8;
    const EACCES: i32 = 13;

    fn files<'a>(list: &'a [(&'a str, Vec<u8>)]) -> impl Fn(&Path) -> Option<Vec<u8>> + 'a {
        move |p: &Path| {
            list.iter()
                .find(|(name, _)| Path::new(name) == p)
                .map(|(_, data)| data.clone())
        }
    }

    #[test]
    fn a_glibc_program_without_its_loader() {
        let prog = elf64(183, Some("/lib/ld-linux-aarch64.so.1"), &["libc.so.6"]);
        let list = [("/app", prog)];
        let (why, next) = diagnose_exec(
            ENOENT,
            Path::new("/app"),
            files(&list),
            |_| false,
            Machine::Aarch64,
        )
        .unwrap();
        assert!(
            why.contains("glibc") && why.contains("/lib/ld-linux-aarch64.so.1"),
            "{why}"
        );
        assert!(
            next.contains("glibc-based image") || next.contains("musl"),
            "{next}"
        );
    }

    #[test]
    fn a_script_without_its_interpreter() {
        let list = [("/run.py", b"#!/nonexistent/python3 -u\nprint(1)\n".to_vec())];
        let (why, next) = diagnose_exec(
            ENOENT,
            Path::new("/run.py"),
            files(&list),
            |_| false,
            Machine::X86_64,
        )
        .unwrap();
        assert!(why.contains("/nonexistent/python3"), "{why}");
        assert!(!why.contains("-u"), "{why}");
        assert!(
            next.contains("first line") || next.contains("install"),
            "{next}"
        );
        // With the interpreter present, there is nothing to explain.
        let found = diagnose_exec(
            ENOENT,
            Path::new("/run.py"),
            files(&list),
            |p| p == Path::new("/nonexistent/python3"),
            Machine::X86_64,
        );
        assert_eq!(found, None);
    }

    #[test]
    fn a_program_for_another_cpu() {
        let list = [("/x", elf64(62, Some("/lib/ld-musl-x86_64.so.1"), &[]))];
        let (why, next) = diagnose_exec(
            ENOEXEC,
            Path::new("/x"),
            files(&list),
            |_| true,
            Machine::Aarch64,
        )
        .unwrap();
        assert!(why.contains("x86_64") && why.contains("aarch64"), "{why}");
        assert!(next.contains("aarch64"), "{next}");
        // Some kernels answer EINVAL instead.
        assert!(
            diagnose_exec(
                22,
                Path::new("/x"),
                files(&list),
                |_| true,
                Machine::Aarch64
            )
            .is_some()
        );
    }

    #[test]
    fn other_errors_never_open_the_program() {
        // EACCES for a FIFO or a device: opening it to look could block forever.
        let read = |_: &Path| -> Option<Vec<u8>> { panic!("the program was opened") };
        assert_eq!(
            diagnose_exec(EACCES, Path::new("/fifo"), read, |_| true, Machine::X86_64),
            None
        );
    }

    #[test]
    fn a_script_whose_interpreter_needs_a_missing_loader() {
        let python = elf64(183, Some("/lib/ld-linux-aarch64.so.1"), &["libc.so.6"]);
        let list = [
            (
                "/app/venv/bin/tool",
                b"#!/app/venv/bin/python\nimport x\n".to_vec(),
            ),
            ("/app/venv/bin/python", python),
        ];
        let (why, next) = diagnose_exec(
            ENOENT,
            Path::new("/app/venv/bin/tool"),
            files(&list),
            |p| p == Path::new("/app/venv/bin/python"),
            Machine::Aarch64,
        )
        .unwrap();
        assert!(
            why.contains("/app/venv/bin/tool") && why.contains("/app/venv/bin/python"),
            "{why}"
        );
        assert!(
            why.contains("glibc") && why.contains("/lib/ld-linux-aarch64.so.1"),
            "{why}"
        );
        assert!(next.contains("glibc-based image"), "{next}");
    }

    #[test]
    fn diagnosis_falls_back_to_the_plain_error() {
        // Unreadable file, missing file, a working loader, an unrelated error: no diagnosis.
        let none = |_: &Path| None;
        assert_eq!(
            diagnose_exec(ENOENT, Path::new("/a"), none, |_| false, Machine::X86_64),
            None
        );
        let prog = elf64(62, Some("/lib64/ld-linux-x86-64.so.2"), &[]);
        let list = [("/b", prog)];
        assert_eq!(
            diagnose_exec(
                ENOENT,
                Path::new("/b"),
                files(&list),
                |_| true,
                Machine::X86_64
            ),
            None
        );
        assert_eq!(
            diagnose_exec(
                EACCES,
                Path::new("/b"),
                files(&list),
                |_| false,
                Machine::X86_64
            ),
            None
        );
        let junk = [("/c", b"\x7fELF garbage".to_vec())];
        assert_eq!(
            diagnose_exec(
                ENOEXEC,
                Path::new("/c"),
                files(&junk),
                |_| true,
                Machine::X86_64
            ),
            None
        );
    }
}
