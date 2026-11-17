//! `ps`, `inspect`, `logs`, `rm`, `stop`, `kill`.

use crate::args::{ExecArgs, parse_env, parse_signal};
use crate::image::report;
use openpod::{
    ContainerHandle, ContainerInfo, Error, ErrorKind, ExecOptions, Image, Status, Stream,
};
use std::io::Write;
use std::time::Duration;

pub fn ps(all: bool, quiet: bool) -> i32 {
    let list = match ContainerHandle::list(all) {
        Ok(l) => l,
        Err(e) => return report(&e),
    };
    if quiet {
        list.iter().for_each(|c| println!("{}", &c.id[..12]));
        return 0;
    }
    let now = openpod::state::now();
    let rows: Vec<[String; 6]> = list
        .iter()
        .map(|c| {
            [
                c.id[..12].to_string(),
                short_image(&c.image),
                command(c),
                format!(
                    "{} ago",
                    human_duration(now.saturating_sub(c.state.created))
                ),
                status(c, now),
                c.name.clone(),
            ]
        })
        .collect();
    print_table(
        &[
            "CONTAINER ID",
            "IMAGE",
            "COMMAND",
            "CREATED",
            "STATUS",
            "NAMES",
        ],
        &rows,
    );
    0
}

pub fn inspect(target: &str) -> i32 {
    let json = match ContainerHandle::get(target).and_then(|h| h.info()) {
        Ok(info) => serde_json::to_string_pretty(&info),
        // Not a container: maybe an image.
        Err(e) if e.kind == ErrorKind::NotFound => match Image::get(target) {
            Ok(image) => serde_json::to_string_pretty(&image),
            Err(_) => return report(&e),
        },
        Err(e) => return report(&e),
    };
    println!("{}", json.unwrap_or_default());
    0
}

pub fn logs(target: &str, follow: bool) -> i32 {
    let handle = match ContainerHandle::get(target) {
        Ok(h) => h,
        Err(e) => return report(&e),
    };
    let mut follower = match handle.follow_logs() {
        Ok(f) => f,
        Err(e) => return report(&e),
    };
    loop {
        // Check before reading, so output written just before the end is still read.
        let running = follow
            && handle
                .info()
                .is_ok_and(|i| i.state.status == Status::Running);
        let lines = match follower.next_lines() {
            Ok(l) => l,
            Err(e) => return report(&e),
        };
        for line in &lines {
            let _ = match line.stream {
                Stream::Stdout => std::io::stdout().write_all(line.log.as_bytes()),
                Stream::Stderr => std::io::stderr().write_all(line.log.as_bytes()),
            };
        }
        let _ = std::io::stdout().flush();
        if !running {
            return 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn rm(targets: &[String], force: bool, volumes: bool) -> i32 {
    each(targets, |h| {
        let removed = if volumes {
            h.remove_with_volumes(force)
        } else {
            h.remove(force)
        };
        removed.map(|()| println!("{}", h.id()))
    })
}

pub fn stop(targets: &[String], seconds: u64) -> i32 {
    each(targets, |h| {
        h.stop(Duration::from_secs(seconds))
            .map(|_| println!("{}", h.id()))
    })
}

pub fn kill(targets: &[String], signal: &str) -> i32 {
    let Some(sig) = parse_signal(signal) else {
        return report(&Error::new(
            ErrorKind::InvalidInput,
            format!("invalid signal {signal}"),
            "use a name like TERM or KILL, or a number",
            "run `kill -l` for the list",
        ));
    };
    each(targets, |h| h.kill(sig).map(|()| println!("{}", h.id())))
}

pub fn exec(a: ExecArgs) -> i32 {
    let (target, command) = a
        .args
        .split_first()
        .expect("clap requires a container and a command");
    let options = ExecOptions {
        env: a
            .env
            .iter()
            .filter_map(|e| parse_env(e, |k| std::env::var(k).ok()))
            .collect(),
        workdir: a.workdir,
        user: a.user,
        tty: a.tty,
        interactive: a.interactive,
    };
    let forwarded = [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP];
    openpod::signals::block(&forwarded);
    let result = ContainerHandle::get(target).and_then(|h| {
        let _raw = (a.tty && a.interactive)
            .then(crate::run::raw_terminal)
            .flatten();
        let running = h.exec_inherit(command, &options)?;
        openpod::signals::forward_to(running.helper_pid(), &forwarded);
        running.wait()
    });
    match result {
        Ok(code) => code,
        Err(e) => {
            openpod::signals::ignore(&forwarded);
            report(&e)
        }
    }
}

/// Apply `f` to each container; report errors and keep going, like Docker.
fn each(targets: &[String], f: impl Fn(&ContainerHandle) -> openpod::Result<()>) -> i32 {
    let mut code = 0;
    for t in targets {
        if let Err(e) = ContainerHandle::get(t).and_then(|h| f(&h)) {
            code = report(&e);
        }
    }
    code
}

fn short_image(image: &str) -> String {
    image
        .strip_prefix("docker.io/library/")
        .or_else(|| image.strip_prefix("docker.io/"))
        .unwrap_or(image)
        .to_string()
}

fn command(c: &ContainerInfo) -> String {
    let full = c.command.join(" ");
    let short: String = full.chars().take(20).collect();
    if full.chars().count() > 20 {
        format!("\"{short}…\"")
    } else {
        format!("\"{full}\"")
    }
}

fn status(c: &ContainerInfo, now: u64) -> String {
    match c.state.status {
        Status::Created => "Created".to_string(),
        Status::Running => format!(
            "Up {}",
            human_duration(now.saturating_sub(c.state.started.unwrap_or(now)))
        ),
        Status::Exited => {
            let code = c.state.exit_code.map_or("?".to_string(), |c| c.to_string());
            let ago = human_duration(now.saturating_sub(c.state.finished.unwrap_or(now)));
            match c.state.oom {
                Some(reason) => format!("Exited ({code}, out of memory: {reason}) {ago} ago"),
                None => format!("Exited ({code}) {ago} ago"),
            }
        }
    }
}

/// Docker's wording for a duration in seconds.
fn human_duration(secs: u64) -> String {
    match secs {
        0 => "Less than a second".to_string(),
        1 => "1 second".to_string(),
        2..60 => format!("{secs} seconds"),
        60..120 => "About a minute".to_string(),
        120..3600 => format!("{} minutes", secs / 60),
        3600..7200 => "About an hour".to_string(),
        7200..172_800 => format!("{} hours", secs / 3600),
        _ => format!("{} days", secs / 86_400),
    }
}

fn print_table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let parts: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", parts.join("   ").trim_end());
    };
    line(header.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}
