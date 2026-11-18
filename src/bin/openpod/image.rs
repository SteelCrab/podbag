//! `pull`, `images`, `load`, `save`, `rmi` and `system`.

use crate::args::SystemCmd;
use openpod::image::human_size;
use openpod::{Error, ErrorKind, Image};

/// Print an error the CLI way and return its exit code.
pub fn report(e: &Error) -> i32 {
    eprintln!("openpod: {e}");
    e.exit_code()
}

pub fn load(input: Option<&std::path::Path>, quiet: bool) -> i32 {
    let loaded = match input {
        Some(path) => match std::fs::File::open(path) {
            Ok(mut f) => Image::load(&mut f),
            Err(e) => Err(Error::io(
                format!("cannot open {}", path.display()),
                &e,
                "check the path",
            )),
        },
        None => Image::load(&mut std::io::stdin().lock()),
    };
    match loaded {
        Ok(report) => {
            if !quiet {
                for name in &report.names {
                    println!("Loaded image: {name}");
                }
                for id in &report.untagged {
                    println!("Loaded image ID: {id}");
                }
            }
            0
        }
        Err(e) => report(&e),
    }
}

pub fn save(output: Option<&std::path::Path>, names: &[String]) -> i32 {
    use std::io::IsTerminal;
    let saved = match output {
        Some(path) => std::fs::File::create(path)
            .map_err(|e| {
                Error::io(
                    format!("cannot create {}", path.display()),
                    &e,
                    "check the path",
                )
            })
            .and_then(|mut f| Image::save(names, &mut f)),
        None if std::io::stdout().is_terminal() => Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot save images to a terminal",
            "the archive is binary",
            "redirect stdout to a file, or use -o FILE",
        )),
        None => Image::save(names, &mut std::io::stdout().lock()),
    };
    match saved {
        Ok(()) => 0,
        Err(e) => report(&e),
    }
}

pub fn pull(name: &str) -> i32 {
    match Image::pull_with_progress(name) {
        Ok(image) => {
            println!("{}@{}", image.name, image.digest);
            0
        }
        Err(e) => report(&e),
    }
}

pub fn check_image(name: &str) -> i32 {
    let report = match Image::check(name) {
        Ok(r) => r,
        Err(e) => return report(&e),
    };
    let libc = match report.image_libc.as_slice() {
        [] => "no C library".to_string(),
        list => list
            .iter()
            .map(|l| l.name())
            .collect::<Vec<_>>()
            .join(" and "),
    };
    println!(
        "{name}: {} programs and libraries checked; the image has {libc}",
        report.files_checked
    );
    for f in &report.findings {
        let level = if f.is_error() { "error" } else { "warning" };
        println!("{level}: {} {}", f.path, f.text());
        println!("  fix: {}", f.fix);
    }
    if report.findings.is_empty() {
        println!("no problems found");
    }
    i32::from(report.has_problems())
}

pub fn images() -> i32 {
    match Image::list() {
        Ok(list) => {
            let rows: Vec<[String; 3]> = list
                .iter()
                .map(|i| [i.name.clone(), short_digest(&i.digest), human_size(i.size)])
                .collect();
            print_table(&["NAME", "DIGEST", "SIZE"], &rows);
            0
        }
        Err(e) => report(&e),
    }
}

pub fn rmi(names: &[String]) -> i32 {
    let mut code = 0;
    for name in names {
        match Image::remove(name) {
            Ok(removed) => removed.iter().for_each(|n| println!("Untagged: {n}")),
            Err(e) => code = report(&e),
        }
    }
    code
}

pub fn system(cmd: SystemCmd) -> i32 {
    match cmd {
        SystemCmd::Df => match openpod::disk_usage() {
            Ok(u) => {
                let rows = [
                    ["Images".to_string(), u.images.to_string(), "-".to_string()],
                    [
                        "Objects".to_string(),
                        u.objects.to_string(),
                        human_size(u.object_bytes),
                    ],
                    [
                        "Containers".to_string(),
                        u.containers.to_string(),
                        "-".to_string(),
                    ],
                ];
                print_table(&["TYPE", "COUNT", "SIZE"], &rows);
                0
            }
            Err(e) => report(&e),
        },
        // Handled in main on Unix; there is no service elsewhere.
        SystemCmd::Service { .. } => report(&Error::new(
            ErrorKind::Unsupported,
            "cannot serve the Docker API on this operating system",
            "the service needs Unix sockets and Linux containers",
            "run openpod on Linux",
        )),
        SystemCmd::Prune { force: false } => report(&Error::new(
            ErrorKind::InvalidInput,
            "refusing to prune without confirmation",
            "prune deletes every image that no container uses",
            "add -f to confirm",
        )),
        SystemCmd::Prune { force: true } => match openpod::prune() {
            Ok(r) => {
                println!(
                    "Deleted {} images, reclaimed {}",
                    r.removed_images.len() + r.removed_ids.len(),
                    human_size(r.freed_bytes)
                );
                0
            }
            Err(e) => report(&e),
        },
    }
}

fn short_digest(d: &str) -> String {
    d.strip_prefix("sha256:")
        .unwrap_or(d)
        .chars()
        .take(12)
        .collect()
}

fn print_table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.len());
        }
    }
    let line = |cells: Vec<&str>| {
        let parts: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", parts.join("  ").trim_end());
    };
    line(header.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}
