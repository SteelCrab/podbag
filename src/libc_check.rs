//! `openpod check-image`: find programs and libraries in an image that cannot run there —
//! built for another C library (their loader is missing) or another CPU (design 6.9).
//! Files are read, never executed.

use crate::elf::{Libc, Machine};
use std::path::{Path, PathBuf};

/// What is wrong with one file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Problem {
    /// Its loader is not in the image: built for another C library (or the image lacks one).
    MissingInterpreter { interp: String, libc: Libc },
    /// Built for another CPU.
    WrongMachine { file: Machine, image: Machine },
    /// A library it needs is in none of the usual places (a warning: it may come from
    /// somewhere openpod cannot see, such as a path set at run time).
    MissingLibrary { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    /// The path inside the image.
    pub path: String,
    pub problem: Problem,
    /// What to do about it.
    pub fix: String,
}

impl Finding {
    /// One line: what is wrong.
    pub fn text(&self) -> String {
        match &self.problem {
            Problem::MissingInterpreter { interp, libc } => {
                format!(
                    "needs {} (loader {interp}), which the image does not have",
                    libc.name()
                )
            }
            Problem::WrongMachine { file, image } => format!(
                "is built for {}, but the image is for {}",
                file.name(),
                image.name()
            ),
            Problem::MissingLibrary { name } => format!("needs {name}, which was not found"),
        }
    }

    /// Problems that stop the file from running; a missing library is only a warning.
    pub fn is_error(&self) -> bool {
        !matches!(self.problem, Problem::MissingLibrary { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    /// The C libraries whose loader the image has.
    pub image_libc: Vec<Libc>,
    /// By path.
    pub findings: Vec<Finding>,
    /// ELF files read.
    pub files_checked: usize,
}

impl Report {
    pub fn has_problems(&self) -> bool {
        self.findings.iter().any(Finding::is_error)
    }
}

/// How many symbolic links one path may pass through.
const MAX_LINKS: u32 = 40;
/// How much of a loader configuration file is read.
const MAX_CONFIG: u64 = 64 * 1024;
/// How many library directories the configuration files may add.
const MAX_CONF_DIRS: usize = 256;

/// Open a regular file of the image for reading. Never waits (a FIFO would), never
/// follows a link in the last component, and refuses devices and other special files:
/// the image is untrusted, and as root a device would be the host's.
fn open_regular(path: &Path) -> Option<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

/// The start of a loader configuration file, as text.
fn read_config(path: &Path) -> String {
    use std::io::Read;
    let mut buf = Vec::new();
    if let Some(file) = open_regular(path) {
        file.take(MAX_CONFIG).read_to_end(&mut buf).ok();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Resolve `path` as the image sees it: absolute links start again at `root`, and `..`
/// never leaves it. Returns the host path, if it exists.
fn resolve_in_root(root: &Path, path: &str) -> Option<PathBuf> {
    let mut todo: std::collections::VecDeque<String> = path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    let mut done: Vec<String> = Vec::new();
    let mut links = 0;
    while let Some(part) = todo.pop_front() {
        match part.as_str() {
            "." => continue,
            ".." => {
                done.pop();
                continue;
            }
            _ => {}
        }
        let here = root.join(done.join("/")).join(&part);
        let meta = std::fs::symlink_metadata(&here).ok()?;
        if meta.file_type().is_symlink() {
            links += 1;
            if links > MAX_LINKS {
                return None;
            }
            let target = std::fs::read_link(&here).ok()?;
            let target = target.to_string_lossy();
            if target.starts_with('/') {
                done.clear();
            }
            for p in target.split('/').filter(|p| !p.is_empty()).rev() {
                todo.push_front(p.to_string());
            }
            continue;
        }
        done.push(part);
    }
    Some(root.join(done.join("/")))
}

/// The loaders the image has, by C library.
fn image_libcs(root: &Path) -> Vec<Libc> {
    let mut found = Vec::new();
    for dir in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
        let Some(Ok(entries)) = resolve_in_root(root, dir).map(std::fs::read_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let libc = crate::elf::libc_of_interp(&name);
            if libc != Libc::Unknown && !found.contains(&libc) {
                found.push(libc);
            }
        }
    }
    found.sort_by_key(|l| l.name());
    found
}

/// Where the image's loaders look for libraries: the usual directories, the multiarch
/// ones, `ld.so.conf` (glibc) and `ld-musl-*.path` (musl), plus `extra`.
fn library_dirs(root: &Path, extra: &[String]) -> Vec<String> {
    let mut dirs: Vec<String> = [
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/lib/x86_64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/lib/aarch64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        // 32-bit libraries of multilib images.
        "/lib32",
        "/usr/lib32",
        "/lib/i386-linux-gnu",
        "/usr/lib/i386-linux-gnu",
        "/lib/arm-linux-gnueabihf",
        "/usr/lib/arm-linux-gnueabihf",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // Every path is resolved inside the image: a link must not lead to the host's files.
    let names_in = |dir: &str| -> Vec<String> {
        resolve_in_root(root, dir)
            .and_then(|d| std::fs::read_dir(d).ok())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut conf = vec!["/etc/ld.so.conf".to_string()];
    conf.extend(
        names_in("/etc/ld.so.conf.d")
            .into_iter()
            .map(|n| format!("/etc/ld.so.conf.d/{n}")),
    );
    conf.extend(
        names_in("/etc")
            .into_iter()
            .filter(|n| n.starts_with("ld-musl-") && n.ends_with(".path"))
            .map(|n| format!("/etc/{n}")),
    );
    let conf: Vec<PathBuf> = conf
        .iter()
        .filter_map(|c| resolve_in_root(root, c))
        .collect();
    let mut added = 0;
    for file in conf {
        let text = read_config(&file);
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.starts_with("include") {
                continue;
            }
            for dir in line.split([':', ' ', '\t']).filter(|d| d.starts_with('/')) {
                if added < MAX_CONF_DIRS && !dirs.iter().any(|d| d == dir) {
                    dirs.push(dir.to_string());
                    added += 1;
                }
            }
        }
    }
    for dir in extra.iter().take(MAX_CONF_DIRS) {
        if !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    dirs
}

/// Read an ELF file in bounded ranges.
fn read_file(path: &Path) -> Option<crate::elf::ElfInfo> {
    let file = open_regular(path)?;
    let read_at = |off: u64, len: usize| -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len.min(crate::elf::MAX_READ)];
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            file.read_at(&mut buf, off).ok()?
        };
        #[cfg(not(unix))]
        let n = {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = &file;
            f.seek(SeekFrom::Start(off)).ok()?;
            f.read(&mut buf).ok()?
        };
        if n == 0 {
            return None;
        }
        buf.truncate(n);
        Some(buf)
    };
    crate::elf::read_elf(&read_at)
}

/// Programs and libraries: executable files, or files whose name has `.so`.
fn candidates(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            // Kernel and device trees are not part of images, but skip them if present.
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if !matches!(rel.to_str(), Some("proc" | "sys" | "dev")) {
                candidates(root, &path, out);
            }
        } else if meta.is_file() && meta.len() >= 64 {
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let name = entry.file_name().to_string_lossy().into_owned();
            if executable || name.contains(".so") {
                out.push(path);
            }
        }
    }
}

/// Check every program and library under `root` (an image's files) for `arch`.
/// `ld_paths` are extra library directories (the image's `LD_LIBRARY_PATH`).
pub fn check_tree(root: &Path, arch: Machine, ld_paths: &[String]) -> Report {
    let mut files = Vec::new();
    candidates(root, root, &mut files);
    files.sort();
    let lib_dirs = library_dirs(root, ld_paths);
    let image_libc = image_libcs(root);
    let mut findings = Vec::new();
    let mut files_checked = 0;
    // "Is <dir>/<name> in the image?", asked for every library of every program.
    let mut known: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for file in files {
        let Some(info) = read_file(&file) else {
            continue;
        };
        files_checked += 1;
        let rel = format!(
            "/{}",
            file.strip_prefix(root).unwrap_or(&file).to_string_lossy()
        );
        let push = |findings: &mut Vec<Finding>, problem: Problem, fix: String| {
            findings.push(Finding {
                path: rel.clone(),
                problem,
                fix,
            })
        };
        if !arch.runs(info.machine) {
            push(
                &mut findings,
                Problem::WrongMachine {
                    file: info.machine,
                    image: arch,
                },
                format!(
                    "use the image built for {}, or rebuild the program for it",
                    info.machine.name()
                ),
            );
            continue;
        }
        if let Some(interp) = &info.interp {
            if resolve_in_root(root, interp).is_none() {
                let libc = crate::elf::libc_of_interp(interp);
                push(
                    &mut findings,
                    Problem::MissingInterpreter {
                        interp: interp.clone(),
                        libc,
                    },
                    interpreter_fix(libc, &image_libc),
                );
                continue;
            }
        }
        let origin = rel
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or("")
            .to_string();
        let mut dirs: Vec<String> = info
            .runpath
            .iter()
            .map(|p| p.replace("$ORIGIN", &origin).replace("${ORIGIN}", &origin))
            .collect();
        dirs.push(origin.clone());
        dirs.extend(lib_dirs.iter().cloned());
        for name in &info.needed {
            let found = dirs.iter().any(|d| {
                let path = format!("{d}/{name}");
                if let Some(&hit) = known.get(&path) {
                    return hit;
                }
                let hit = resolve_in_root(root, &path).is_some();
                known.insert(path, hit);
                hit
            });
            if !found {
                push(
                    &mut findings,
                    Problem::MissingLibrary { name: name.clone() },
                    format!("add the package that provides {name}, or check LD_LIBRARY_PATH"),
                );
            }
        }
    }
    Report {
        image_libc,
        findings,
        files_checked,
    }
}

fn interpreter_fix(needed: Libc, image: &[Libc]) -> String {
    match (needed, image.first()) {
        (Libc::Glibc, Some(Libc::Musl)) => {
            "it is built for glibc but the image has musl: use a glibc-based image (such as debian or ubuntu), or build it for musl".into()
        }
        (Libc::Musl, Some(Libc::Glibc)) => {
            "it is built for musl but the image has glibc: use an Alpine-based image, or build it for glibc".into()
        }
        (_, None) => "the image has no C library loader: build the program statically, or use a base image with a C library".into(),
        _ => "copy the loader into the image, or rebuild the program for the image's C library".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::elf64;
    use std::os::unix::fs::PermissionsExt;

    fn put(root: &Path, path: &str, bytes: &[u8], mode: u32) {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A small musl image for this CPU: a loader, its libc, and one program.
    fn musl_image() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let m = 183;
        put(r, "lib/ld-musl-aarch64.so.1", &elf64(m, None, &[]), 0o755);
        std::os::unix::fs::symlink("ld-musl-aarch64.so.1", r.join("lib/libc.musl-aarch64.so.1"))
            .unwrap();
        put(
            r,
            "bin/busybox",
            &elf64(
                m,
                Some("/lib/ld-musl-aarch64.so.1"),
                &["libc.musl-aarch64.so.1"],
            ),
            0o755,
        );
        put(r, "etc/hosts", b"127.0.0.1 localhost\n", 0o644);
        tmp
    }

    #[test]
    fn a_clean_image_has_no_findings() {
        let img = musl_image();
        let report = check_tree(img.path(), Machine::Aarch64, &[]);
        assert_eq!(report.image_libc, [Libc::Musl]);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(report.files_checked, 2);
        assert!(!report.has_problems());
    }

    #[test]
    fn a_glibc_program_in_a_musl_image() {
        let img = musl_image();
        put(
            img.path(),
            "usr/bin/app",
            &elf64(183, Some("/lib/ld-linux-aarch64.so.1"), &["libc.so.6"]),
            0o755,
        );
        let report = check_tree(img.path(), Machine::Aarch64, &[]);
        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        let f = &report.findings[0];
        assert_eq!(f.path, "/usr/bin/app");
        assert_eq!(
            f.problem,
            Problem::MissingInterpreter {
                interp: "/lib/ld-linux-aarch64.so.1".into(),
                libc: Libc::Glibc
            }
        );
        assert!(
            f.fix.contains("glibc") && f.fix.contains("musl"),
            "{}",
            f.fix
        );
        assert!(report.has_problems());
    }

    #[test]
    fn a_program_for_another_cpu() {
        let img = musl_image();
        put(
            img.path(),
            "usr/bin/x86",
            &elf64(62, Some("/lib/ld-musl-x86_64.so.1"), &[]),
            0o755,
        );
        let report = check_tree(img.path(), Machine::Aarch64, &[]);
        assert_eq!(
            report.findings[0].problem,
            Problem::WrongMachine {
                file: Machine::X86_64,
                image: Machine::Aarch64
            }
        );
    }

    #[test]
    fn missing_libraries_are_warnings() {
        let img = musl_image();
        put(
            img.path(),
            "usr/bin/needs",
            &elf64(183, Some("/lib/ld-musl-aarch64.so.1"), &["libgone.so.3"]),
            0o755,
        );
        // A library next to the program or on LD_LIBRARY_PATH counts as found.
        put(
            img.path(),
            "opt/app/libnear.so",
            &elf64(183, None, &[]),
            0o644,
        );
        put(
            img.path(),
            "opt/app/tool",
            &elf64(
                183,
                Some("/lib/ld-musl-aarch64.so.1"),
                &["libnear.so", "libenv.so"],
            ),
            0o755,
        );
        put(
            img.path(),
            "srv/lib/libenv.so",
            &elf64(183, None, &[]),
            0o644,
        );
        let report = check_tree(img.path(), Machine::Aarch64, &["/srv/lib".into()]);
        let missing: Vec<&Finding> = report.findings.iter().collect();
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(
            missing[0].problem,
            Problem::MissingLibrary {
                name: "libgone.so.3".into()
            }
        );
        assert!(!report.has_problems(), "a missing library is a warning");
    }

    #[test]
    fn interpreter_links_resolve_inside_the_image() {
        let img = musl_image();
        // An absolute link inside the image, and one that points far up and out.
        std::fs::create_dir_all(img.path().join("lib64")).unwrap();
        std::os::unix::fs::symlink(
            "/lib/ld-musl-aarch64.so.1",
            img.path().join("lib64/ld-a.so"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../../../../../../bin/sh",
            img.path().join("lib64/ld-b.so"),
        )
        .unwrap();
        put(
            img.path(),
            "usr/bin/a",
            &elf64(183, Some("/lib64/ld-a.so"), &[]),
            0o755,
        );
        put(
            img.path(),
            "usr/bin/b",
            &elf64(183, Some("/lib64/ld-b.so"), &[]),
            0o755,
        );
        let report = check_tree(img.path(), Machine::Aarch64, &[]);
        let paths: Vec<&str> = report.findings.iter().map(|f| f.path.as_str()).collect();
        // ld-b.so resolves to <image>/bin/sh, which the image does not have (the host's
        // /bin/sh must not count).
        assert_eq!(paths, ["/usr/bin/b"], "{:?}", report.findings);
    }

    #[test]
    fn host_files_are_never_read_through_links() {
        // A host directory with a glibc loader and a library directory list.
        let host = tempfile::tempdir().unwrap();
        put(
            host.path(),
            "ld-linux-aarch64.so.2",
            &elf64(183, None, &[]),
            0o755,
        );
        put(host.path(), "ld.so.conf", b"/weird\n", 0o644);
        let img = musl_image();
        // Absolute links in the image that, followed on the host, would reach those files.
        std::os::unix::fs::symlink(host.path(), img.path().join("lib64")).unwrap();
        std::fs::create_dir_all(img.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(
            host.path().join("ld.so.conf"),
            img.path().join("etc/ld.so.conf"),
        )
        .unwrap();
        put(img.path(), "weird/libx.so", &elf64(183, None, &[]), 0o644);
        put(
            img.path(),
            "usr/bin/x",
            &elf64(183, Some("/lib/ld-musl-aarch64.so.1"), &["libx.so"]),
            0o755,
        );
        let report = check_tree(img.path(), Machine::Aarch64, &[]);
        assert_eq!(
            report.image_libc,
            [Libc::Musl],
            "the host's loader was counted"
        );
        assert_eq!(
            report
                .findings
                .iter()
                .map(|f| &f.problem)
                .collect::<Vec<_>>(),
            [&Problem::MissingLibrary {
                name: "libx.so".into()
            }],
            "the host's ld.so.conf was read"
        );
    }

    /// Run `check_tree` on another thread; fail if it takes longer than `secs`.
    fn check_within(root: &Path, arch: Machine, secs: u64) -> Report {
        let root = root.to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(check_tree(&root, arch, &[])).ok());
        rx.recv_timeout(std::time::Duration::from_secs(secs))
            .expect("check_tree did not finish")
    }

    #[test]
    fn special_and_huge_config_files_are_not_read_whole() {
        let img = musl_image();
        let etc = img.path().join("etc");
        std::fs::create_dir_all(etc.join("ld.so.conf.d")).unwrap();
        // A FIFO: opening it for reading would wait for a writer forever.
        assert!(
            std::process::Command::new("mkfifo")
                .arg(etc.join("ld.so.conf"))
                .status()
                .unwrap()
                .success()
        );
        // A huge sparse file: only its start is read.
        let big = std::fs::File::create(etc.join("ld.so.conf.d/big.conf")).unwrap();
        use std::io::Write;
        (&big).write_all(b"/weird\n").unwrap();
        big.set_len(1 << 32).unwrap();
        put(img.path(), "weird/libx.so", &elf64(183, None, &[]), 0o644);
        put(
            img.path(),
            "usr/bin/x",
            &elf64(183, Some("/lib/ld-musl-aarch64.so.1"), &["libx.so"]),
            0o755,
        );
        let report = check_within(img.path(), Machine::Aarch64, 10);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn hostile_library_lists_finish_quickly() {
        let img = musl_image();
        std::fs::create_dir_all(img.path().join("d")).unwrap();
        let names: Vec<String> = (0..4000).map(|i| format!("libm{i}.so")).collect();
        let needed: Vec<&str> = names.iter().map(String::as_str).collect();
        let dirs: Vec<String> = (0..500).map(|i| format!("/d/{i}")).collect();
        let prog = crate::elf::elf64_runpath(
            183,
            Some("/lib/ld-musl-aarch64.so.1"),
            &needed,
            Some(&dirs.join(":")),
        );
        put(img.path(), "usr/bin/hostile", &prog, 0o755);
        let report = check_within(img.path(), Machine::Aarch64, 3);
        assert_eq!(report.findings.len(), crate::elf::MAX_NEEDED);
    }

    #[test]
    fn compatible_32_bit_programs_are_not_errors() {
        // An amd64 image with i386 multilib files.
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        put(
            r,
            "lib64/ld-linux-x86-64.so.2",
            &elf64(62, None, &[]),
            0o755,
        );
        put(r, "lib/ld-linux.so.2", &elf64(3, None, &[]), 0o755);
        put(r, "usr/lib32/libc.so.6", &elf64(3, None, &[]), 0o644);
        put(
            r,
            "usr/lib32/prog32",
            &elf64(3, Some("/lib/ld-linux.so.2"), &["libc.so.6"]),
            0o755,
        );
        put(
            r,
            "usr/lib32/noloader",
            &elf64(3, Some("/lib/ld-gone.so.2"), &[]),
            0o755,
        );
        let report = check_tree(r, Machine::X86_64, &[]);
        let problems: Vec<(&str, &Problem)> = report
            .findings
            .iter()
            .map(|f| (f.path.as_str(), &f.problem))
            .collect();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].0, "/usr/lib32/noloader");
        assert!(
            matches!(problems[0].1, Problem::MissingInterpreter { .. }),
            "{problems:?}"
        );
        // 32-bit arm in an arm64 image; a real mismatch is still an error.
        let img = musl_image();
        put(img.path(), "usr/bin/arm32", &elf64(40, None, &[]), 0o755);
        put(img.path(), "usr/bin/amd64", &elf64(62, None, &[]), 0o755);
        let report = check_tree(img.path(), Machine::Aarch64, &[]);
        let paths: Vec<&str> = report.findings.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["/usr/bin/amd64"], "{:?}", report.findings);
    }

    #[test]
    fn unreadable_and_odd_files_are_skipped() {
        let img = musl_image();
        put(img.path(), "usr/bin/script", b"#!/bin/sh\necho hi\n", 0o755);
        put(img.path(), "usr/bin/locked", &elf64(62, None, &[]), 0o000);
        std::os::unix::fs::symlink("/etc/shadow", img.path().join("usr/bin/link")).unwrap();
        let report = check_tree(img.path(), Machine::Aarch64, &[]);
        // As root the locked file is readable (and wrong for this CPU); otherwise skipped.
        assert!(report.findings.len() <= 1, "{:?}", report.findings);
    }
}
