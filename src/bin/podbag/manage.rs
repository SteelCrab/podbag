//! `stats`, `top`, `pause`, `unpause`, `rename`, `update`.

use crate::args::UpdateArgs;
use crate::image::report;
use podbag::stats::{Row, Sample, table};
use podbag::update::Update;
use podbag::{ContainerHandle, Error, ErrorKind};
use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

/// Run `f` for each container; the exit code of the last failure, or 0.
fn each(targets: &[String], f: impl Fn(&ContainerHandle) -> podbag::Result<()>) -> i32 {
    let mut code = 0;
    let mut lookup = ContainerHandle::lookup().ok();
    for t in targets {
        let handle = match &mut lookup {
            Some(lookup) => lookup.get(t),
            None => ContainerHandle::get(t),
        };
        match handle.and_then(|h| f(&h)) {
            Ok(()) => println!("{t}"),
            Err(e) => code = report(&e),
        }
    }
    code
}

pub fn pause(targets: &[String]) -> i32 {
    each(targets, ContainerHandle::pause)
}

pub fn unpause(targets: &[String]) -> i32 {
    each(targets, ContainerHandle::unpause)
}

pub fn rename(target: &str, new_name: &str) -> i32 {
    match ContainerHandle::get(target).and_then(|h| h.rename(new_name)) {
        Ok(()) => 0,
        Err(e) => report(&e),
    }
}

fn invalid(what: &str, why: String) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!("invalid {what}"),
        why,
        "see `podbag update --help`",
    )
}

/// The options of `update` as Docker's values.
pub fn update_of(a: &UpdateArgs) -> podbag::Result<Update> {
    let size = |v: &Option<String>| -> podbag::Result<i64> {
        v.as_deref()
            .map(|s| podbag::cgroup::parse_memory(s).map(|b| b as i64))
            .transpose()
            .map(|b| b.unwrap_or(0))
    };
    let memory_swap = match a.memory_swap.as_deref() {
        None => 0,
        Some("-1") => -1,
        Some(s) => podbag::cgroup::parse_memory(s)? as i64,
    };
    let nano_cpus = match a.cpus {
        None => 0,
        Some(c) if c.is_finite() && c > 0.0 => (c * 1e9).round() as i64,
        Some(c) => return Err(invalid("--cpus", format!("{c} is not a number of CPUs"))),
    };
    let restart = a
        .restart
        .as_deref()
        .map(str::parse::<podbag::restart::RestartPolicy>)
        .transpose()?;
    Ok(Update {
        memory: size(&a.memory)?,
        memory_swap,
        memory_reservation: size(&a.memory_reservation)?,
        nano_cpus,
        cpu_quota: a.cpu_quota.unwrap_or(0),
        cpu_period: a.cpu_period.unwrap_or(0),
        cpu_shares: a.cpu_shares.unwrap_or(0),
        cpuset_cpus: a.cpuset_cpus.clone().unwrap_or_default(),
        cpuset_mems: a.cpuset_mems.clone().unwrap_or_default(),
        pids_limit: a.pids_limit,
        blkio_weight: a.blkio_weight.map_or(0, i64::from),
        restart,
    })
}

pub fn update(a: &UpdateArgs) -> i32 {
    let update = match update_of(a) {
        Ok(u) => u,
        Err(e) => return report(&e),
    };
    if update.is_empty() {
        return report(&invalid(
            "update",
            "you must provide one or more flags when using this command".into(),
        ));
    }
    each(&a.containers, |h| {
        for w in h.update(&update)? {
            eprintln!("podbag: warning: {w}");
        }
        Ok(())
    })
}

/// Docker's `top` table: tab stops at least 20 wide, 3 spaces apart.
fn top_table(top: &podbag::top::Top) -> String {
    let n = top.titles.len();
    let mut widths = vec![0; n];
    for row in std::iter::once(&top.titles).chain(&top.processes) {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&top.titles).chain(&top.processes) {
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == n {
                out.push_str(cell);
            } else {
                let width = (widths[i] + 3).max(20);
                out.push_str(&format!("{cell:<width$}"));
            }
        }
        out.push('\n');
    }
    out
}

pub fn top(target: &str, ps_args: &[String]) -> i32 {
    match ContainerHandle::get(target).and_then(|h| h.top(ps_args)) {
        Ok(top) => {
            print!("{}", top_table(&top));
            0
        }
        Err(e) => report(&e),
    }
}

/// The containers to show: those named, or the running ones (all with `-a`).
fn chosen(targets: &[String], all: bool) -> podbag::Result<Vec<(ContainerHandle, String)>> {
    if targets.is_empty() {
        return Ok(ContainerHandle::list(all)?
            .into_iter()
            .filter_map(|c| Some((ContainerHandle::get(&c.id).ok()?, c.name)))
            .collect());
    }
    let mut lookup = ContainerHandle::lookup()?;
    targets
        .iter()
        .map(|t| {
            let h = lookup.get(t)?;
            let name = h.info()?.name;
            Ok((h, name))
        })
        .collect()
}

pub fn stats(targets: &[String], all: bool, no_stream: bool) -> i32 {
    let mut previous: HashMap<String, Sample> = HashMap::new();
    let mut noted = std::collections::HashSet::new();
    let mut first = true;
    loop {
        let containers = match chosen(targets, all) {
            Ok(c) => c,
            Err(e) => return report(&e),
        };
        let mut samples = Vec::new();
        for (h, name) in containers {
            match h.stats() {
                Ok(s) => samples.push((h, name, s)),
                // Removed meanwhile: it leaves the table.
                Err(e) if e.kind == ErrorKind::NotFound && !first => {}
                Err(e) => return report(&e),
            }
        }
        for (h, name, s) in &samples {
            match &s.missing {
                Some(why) if noted.insert(h.id().to_string()) => {
                    eprintln!("podbag: {name}: {why}");
                }
                _ => {}
            }
        }
        // `--no-stream` waits for a second look, for the CPU use in between.
        if first && no_stream {
            first = false;
            previous = samples
                .into_iter()
                .map(|(h, _, s)| (h.id().to_string(), s))
                .collect();
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        first = false;
        let rows: Vec<Row> = samples
            .iter()
            .map(|(h, name, s)| Row::new(h.id(), name, previous.get(h.id()), s))
            .collect();
        let text = table(&rows);
        let mut out = std::io::stdout().lock();
        let written = if no_stream {
            out.write_all(text.as_bytes())
        } else {
            // Like docker: the screen is cleared and drawn again.
            out.write_all(format!("\x1b[2J\x1b[H{text}").as_bytes())
        };
        if written.and_then(|()| out.flush()).is_err() || no_stream {
            return 0;
        }
        drop(out);
        previous = samples
            .into_iter()
            .filter(|(_, _, s)| s.read_ns != 0)
            .map(|(h, _, s)| (h.id().to_string(), s))
            .collect();
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_lines_up_like_docker() {
        let top = podbag::top::Top {
            titles: vec!["UID".into(), "PID".into(), "CMD".into()],
            processes: vec![vec!["root".into(), "4242".into(), "sleep 1d".into()]],
        };
        let text = top_table(&top);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("{:<20}{:<20}CMD", "UID", "PID"));
        assert_eq!(lines[1], format!("{:<20}{:<20}sleep 1d", "root", "4242"));
    }

    #[test]
    fn update_options_as_docker_values() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            let cli = crate::args::Cli::try_parse_from(
                ["podbag", "update"].iter().chain(args.iter()).copied(),
            )
            .unwrap();
            match cli.cmd {
                crate::args::Cmd::Update(a) => update_of(&a),
                _ => unreachable!(),
            }
        };
        let u = parse(&[
            "-m",
            "128m",
            "--memory-swap",
            "-1",
            "--cpus",
            "1.5",
            "--pids-limit",
            "-1",
            "--restart",
            "on-failure:2",
            "c1",
        ])
        .unwrap();
        assert_eq!(u.memory, 128 << 20);
        assert_eq!(u.memory_swap, -1);
        assert_eq!(u.nano_cpus, 1_500_000_000);
        assert_eq!(u.pids_limit, Some(-1));
        assert_eq!(
            u.restart,
            Some(podbag::restart::RestartPolicy::OnFailure { max: 2 })
        );
        assert!(parse(&["c1"]).unwrap().is_empty());
        assert!(parse(&["-m", "lots", "c1"]).is_err());
        assert!(parse(&["--restart", "sometimes", "c1"]).is_err());
    }
}
