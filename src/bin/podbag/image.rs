//! `pull`, `images`, `load`, `save`, `rmi` and `system`.

use crate::args::SystemCmd;
use podbag::config::{self, ImageCompression};
use podbag::image::human_size;
use podbag::{Error, ErrorKind, Image};

/// Print an error the CLI way and return its exit code.
pub fn report(e: &Error) -> i32 {
    eprintln!("podbag: {e}");
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
        SystemCmd::Df => match podbag::disk_usage() {
            Ok(u) => {
                let rows = [
                    ["Images".to_string(), u.images.to_string(), "-".to_string()],
                    [
                        "Objects".to_string(),
                        u.objects.to_string(),
                        human_size(u.object_bytes),
                    ],
                    [
                        "Compressed".to_string(),
                        u.compressed_objects.to_string(),
                        format!(
                            "{} ({} before)",
                            human_size(u.compressed_bytes),
                            human_size(u.original_bytes)
                        ),
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
        SystemCmd::Docker { cmd } => crate::dockerctx::run(cmd),
        SystemCmd::Service { .. } | SystemCmd::DialStdio { .. } => report(&Error::new(
            ErrorKind::Unsupported,
            "cannot serve the Docker API on this operating system",
            "the service needs Unix sockets and Linux containers",
            "run podbag on Linux",
        )),
        SystemCmd::Prune { force: false } => report(&Error::new(
            ErrorKind::InvalidInput,
            "refusing to prune without confirmation",
            "prune deletes every image that no container uses",
            "add -f to confirm",
        )),
        SystemCmd::Verify { fix } => match podbag::verify_store(fix) {
            Ok(r) => {
                println!("Checked {} files: {} damaged", r.objects, r.damaged.len());
                if !r.images.is_empty() {
                    println!("Images that use them: {}", r.images.join(", "));
                }
                if !r.removed.is_empty() {
                    println!(
                        "Removed: {} (pull or load them again)",
                        r.removed.join(", ")
                    );
                }
                if !r.in_use.is_empty() {
                    println!(
                        "Kept, a container uses them: {} (remove the containers, then run this again)",
                        r.in_use.join(", ")
                    );
                }
                if !fix && !r.damaged.is_empty() {
                    println!("Run `podbag system verify --fix` to remove them");
                }
                let healthy = r.damaged.is_empty() || (fix && r.in_use.is_empty());
                i32::from(!healthy)
            }
            Err(e) => report(&e),
        },
        SystemCmd::Compress { images } => match podbag::image::compress_images(&images) {
            Ok(r) => {
                for name in &r.skipped {
                    eprintln!("podbag: {name} is not compressed: a container uses it");
                }
                println!(
                    "Compressed {} images: {} \u{2192} {}",
                    r.report.images,
                    human_size(r.report.bytes_before),
                    human_size(r.report.bytes_after)
                );
                0
            }
            Err(e) => report(&e),
        },
        SystemCmd::Decompress { images } => match podbag::image::decompress_images(&images) {
            Ok(n) => {
                println!("Decompressed {n} images");
                0
            }
            Err(e) => report(&e),
        },
        SystemCmd::Compression { mode, after, json } => {
            match compression(mode.as_deref(), after.as_deref()) {
                Ok(c) if json => {
                    println!(
                        "{{\"mode\":\"{}\",\"after\":{}}}",
                        c.image_compression.name(),
                        c.compress_idle_after
                    );
                    0
                }
                Ok(c) => {
                    match c.image_compression {
                        ImageCompression::Off => println!("Image compression: off"),
                        ImageCompression::Idle => println!(
                            "Image compression: idle (images unused for {})",
                            config::format_age(c.compress_idle_after)
                        ),
                    }
                    0
                }
                Err(e) => report(&e),
            }
        }
        SystemCmd::Prune { force: true } => match podbag::prune() {
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

/// Save what `system compression` was given, then the settings in effect (environment
/// overrides included). The age goes first: the mode is already checked by clap, so a bad
/// age saves nothing.
fn compression(mode: Option<&str>, after: Option<&str>) -> podbag::Result<config::Config> {
    let dirs = podbag::paths::dirs()?;
    for (key, value) in [("compress_idle_after", after), ("image_compression", mode)] {
        if let Some(value) = value {
            config::set_value(&dirs, key, value)?;
        }
    }
    config::load(&dirs)
}

fn short_digest(d: &str) -> String {
    d.strip_prefix("sha256:")
        .unwrap_or(d)
        .chars()
        .take(12)
        .collect()
}

pub fn print_table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) {
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
