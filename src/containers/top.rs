//! `top`: a container's processes. They come from its cgroup (`cgroup.procs`, its
//! groups below included) or, without one, from its PID namespace. Without ps options
//! the table is made from the host's `/proc`, with the columns of `ps -ef`; with options,
//! the host's `ps` runs with them and the rows of the container's processes stay, as
//! Docker does it.

use super::ContainerHandle;
use crate::{Error, ErrorKind, Result};

/// Docker's `top` answer: column titles and one row per process.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Top {
    #[serde(rename = "Titles")]
    pub titles: Vec<String>,
    #[serde(rename = "Processes")]
    pub processes: Vec<Vec<String>>,
}

/// Docker's check of ps options: no other column may be called PID (`-o user=PID`),
/// which would hide the real one.
pub fn check_ps_args(args: &[String]) -> std::result::Result<(), String> {
    let words: Vec<&str> = args.iter().flat_map(|a| a.split_whitespace()).collect();
    for (i, word) in words.iter().enumerate() {
        for part in word.split(',') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            let value = if value.trim().is_empty() {
                words.get(i + 1).copied().unwrap_or("")
            } else {
                value
            };
            if value.starts_with("PID") && key != "pid" {
                return Err(format!("specifying \"{key}={value}\" is not allowed"));
            }
        }
    }
    Ok(())
}

/// Docker's reading of ps output: the titles of the first line, and the lines whose PID
/// is one of `pids` (a thread line, `-` in the PID column, follows its process). The last
/// column takes the rest of a line, spaces and all.
pub fn parse_ps(output: &str, pids: &[u32]) -> std::result::Result<Top, String> {
    let mut lines = output.lines();
    let titles: Vec<String> = lines
        .next()
        .unwrap_or("")
        .split_ascii_whitespace()
        .map(String::from)
        .collect();
    let Some(pid_at) = titles.iter().position(|t| t == "PID") else {
        return Err("Couldn't find PID field in ps output".into());
    };
    let mut top = Top {
        titles,
        processes: Vec::new(),
    };
    let row = |fields: Vec<&str>, n: usize| -> Vec<String> {
        let mut row: Vec<String> = fields.iter().take(n - 1).map(|s| s.to_string()).collect();
        row.push(fields.get(n - 1..).unwrap_or_default().join(" "));
        row
    };
    let mut last_kept = false;
    for line in lines.filter(|l| !l.is_empty()) {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        let Some(pid) = fields.get(pid_at) else {
            continue;
        };
        if *pid == "-" {
            if last_kept {
                top.processes.push(row(fields, top.titles.len()));
            }
            continue;
        }
        let pid: u32 = pid
            .parse()
            .map_err(|e| format!("Unexpected pid '{pid}': {e}"))?;
        last_kept = pids.contains(&pid);
        if last_kept {
            top.processes.push(row(fields, top.titles.len()));
        }
    }
    Ok(top)
}

/// One process, as `/proc` describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// The effective user's name, or its number.
    pub user: String,
    /// `tty_nr` of `/proc/<pid>/stat`.
    pub tty: u64,
    /// User and system time, in clock ticks.
    pub cpu_ticks: u64,
    /// Ticks after boot.
    pub start_ticks: u64,
    /// The arguments, or `[comm]` without them.
    pub command: String,
}

/// A local time, as much of it as `STIME` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub year: i32,
    /// 0 to 11.
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `STIME` as ps writes it: the time today, the day this year, else the year.
pub fn stime(start: Clock, now: Clock) -> String {
    if (start.year, start.month, start.day) == (now.year, now.month, now.day) {
        format!("{:02}:{:02}", start.hour, start.minute)
    } else if start.year == now.year {
        format!("{}{:02}", MONTHS[start.month as usize % 12], start.day)
    } else {
        start.year.to_string()
    }
}

/// `TIME`: CPU time as `[DD-]HH:MM:SS`.
pub fn cpu_time(seconds: u64) -> String {
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let hms = format!("{:02}:{:02}:{:02}", rest / 3600, rest / 60 % 60, rest % 60);
    if days > 0 {
        format!("{days}-{hms}")
    } else {
        hms
    }
}

/// `TTY`: the terminal of a `tty_nr`, `?` for none.
pub fn tty_name(tty_nr: u64) -> String {
    let major = (tty_nr >> 8) & 0xfff;
    let minor = (tty_nr & 0xff) | ((tty_nr >> 12) & 0xfff00);
    match major {
        136..=143 => format!("pts/{}", minor + (major - 136) * 256),
        4 if minor < 64 => format!("tty{minor}"),
        4 => format!("ttyS{}", minor - 64),
        _ => "?".to_string(),
    }
}

/// `C`: the percent of one CPU used over the process's life, at most 99.
pub fn cpu_percent(cpu_ticks: u64, elapsed_ticks: u64) -> u64 {
    if elapsed_ticks == 0 {
        return 0;
    }
    (cpu_ticks * 100 / elapsed_ticks).min(99)
}

/// The table of `ps -ef`. `uptime_ticks`: ticks since boot now; `clock`: the local time
/// of a number of ticks after boot.
pub fn ps_ef(procs: &[Proc], hz: u64, uptime_ticks: u64, clock: impl Fn(u64) -> Clock) -> Top {
    let now = clock(uptime_ticks);
    Top {
        titles: ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"]
            .map(String::from)
            .to_vec(),
        processes: procs
            .iter()
            .map(|p| {
                vec![
                    p.user.clone(),
                    p.pid.to_string(),
                    p.ppid.to_string(),
                    cpu_percent(p.cpu_ticks, uptime_ticks.saturating_sub(p.start_ticks))
                        .to_string(),
                    stime(clock(p.start_ticks), now),
                    tty_name(p.tty),
                    cpu_time(p.cpu_ticks / hz.max(1)),
                    p.command.clone(),
                ]
            })
            .collect(),
    }
}

/// The fields of `/proc/<pid>/stat` after the command name, which may hold spaces and
/// parentheses: the name, then the rest from the state on.
pub fn split_stat(stat: &str) -> Option<(&str, Vec<&str>)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    Some((comm, stat.get(close + 1..)?.split_whitespace().collect()))
}

/// User names by number, from an `/etc/passwd` text.
pub fn users(passwd: &str) -> std::collections::HashMap<u32, String> {
    passwd
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let uid = f.nth(1)?.parse().ok()?;
            Some((uid, name.to_string()))
        })
        .collect()
}

fn not_running(name: &str) -> Error {
    Error::new(
        ErrorKind::Conflict,
        format!("cannot list the processes of container {name}"),
        "it is not running",
        "start it first",
    )
}

impl ContainerHandle {
    /// The container's processes (`docker top`). Without `ps_args`, the columns of
    /// `ps -ef`; with them, the host's `ps` with these options (each a separate
    /// argument, no shell).
    pub fn top(&self, ps_args: &[String]) -> Result<Top> {
        #[cfg(target_os = "linux")]
        {
            let info = self.info()?;
            let (crate::state::Status::Running, Some(pid)) = (info.state.status, info.state.pid)
            else {
                return Err(not_running(&info.name));
            };
            let pids = linux::container_pids(pid, info.state.cgroup.as_deref());
            if ps_args.is_empty() {
                return Ok(linux::table(&pids));
            }
            check_ps_args(ps_args).map_err(|why| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "invalid ps options",
                    why,
                    "give ps options that keep the PID column",
                )
            })?;
            linux::run_ps(ps_args, &pids)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (ps_args, not_running);
            Err(Error::unsupported_os())
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Clock, Proc, Top};
    use crate::{Error, ErrorKind, Result};
    use std::path::Path;

    /// The host PIDs of the container whose main process is `init`: from its cgroup
    /// (and the groups below), or from its PID namespace and the descendants of `init`.
    pub(super) fn container_pids(init: u32, cgroup: Option<&str>) -> Vec<u32> {
        let mut pids = match cgroup {
            Some(group) => group_pids(Path::new(group)),
            None => namespace_pids(init),
        };
        if pids.is_empty() {
            pids.push(init);
        }
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    fn group_pids(group: &Path) -> Vec<u32> {
        let mut pids = Vec::new();
        let mut todo = vec![group.to_path_buf()];
        while let Some(dir) = todo.pop() {
            if let Ok(text) = std::fs::read_to_string(dir.join("cgroup.procs")) {
                pids.extend(text.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
            }
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    todo.push(entry.path());
                }
            }
        }
        pids
    }

    fn all_pids() -> Vec<u32> {
        std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect()
    }

    fn parent_of(pid: u32) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        super::split_stat(&stat)?.1.get(1)?.parse().ok()
    }

    /// Processes in `init`'s PID namespace (where its link can be read) and every
    /// descendant of `init` (whose namespace link may not be readable: another user).
    fn namespace_pids(init: u32) -> Vec<u32> {
        let ns = |pid: u32| std::fs::read_link(format!("/proc/{pid}/ns/pid")).ok();
        let own = ns(init);
        let all = all_pids();
        let parents: Vec<(u32, Option<u32>)> = all.iter().map(|&p| (p, parent_of(p))).collect();
        let mut found: Vec<u32> = vec![init];
        let mut i = 0;
        while i < found.len() {
            let parent = found[i];
            for (p, pp) in &parents {
                if *pp == Some(parent) && !found.contains(p) {
                    found.push(*p);
                }
            }
            i += 1;
        }
        if own.is_some() {
            found.extend(all.iter().filter(|&&p| ns(p) == own));
        }
        found
    }

    fn read_proc(pid: u32, users: &std::collections::HashMap<u32, String>) -> Option<Proc> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (comm, f) = super::split_stat(&stat)?;
        let number = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let uid: Option<u32> = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|v| v.split_whitespace().nth(1)?.parse().ok());
        let user = match uid {
            Some(u) => users
                .get(&u)
                .filter(|name| name.len() <= 8)
                .cloned()
                .unwrap_or_else(|| u.to_string()),
            None => "?".to_string(),
        };
        let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<String> = args
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let command = if args.is_empty() {
            format!("[{comm}]")
        } else {
            args.join(" ")
        };
        Some(Proc {
            pid,
            ppid: number(1) as u32,
            user,
            tty: number(4),
            cpu_ticks: number(11) + number(12),
            start_ticks: number(19),
            command,
        })
    }

    /// The `ps -ef` table of these processes, from the host's `/proc`.
    pub(super) fn table(pids: &[u32]) -> Top {
        let users = super::users(&std::fs::read_to_string("/etc/passwd").unwrap_or_default());
        let procs: Vec<Proc> = pids.iter().filter_map(|&p| read_proc(p, &users)).collect();
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        let uptime: f64 = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|t| t.split_whitespace().next()?.parse().ok())
            .unwrap_or(0.0);
        let uptime_ticks = (uptime * hz as f64) as u64;
        let boot = crate::state::now().saturating_sub(uptime as u64);
        super::ps_ef(&procs, hz, uptime_ticks, |ticks| local(boot + ticks / hz))
    }

    /// The local time of a Unix time.
    // The libc crate marks musl's `time_t` as about to change size; localtime_r takes it
    // whatever its size.
    #[allow(deprecated)]
    fn local(secs: u64) -> Clock {
        let t = secs as libc::time_t;
        // SAFETY: an all-zero tm is valid; localtime_r writes it from a valid time.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are valid for the call.
        unsafe { libc::localtime_r(&t, &mut tm) };
        Clock {
            year: tm.tm_year + 1900,
            month: tm.tm_mon as u32,
            day: tm.tm_mday as u32,
            hour: tm.tm_hour as u32,
            minute: tm.tm_min as u32,
        }
    }

    fn find_ps() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
        std::env::split_paths(&path)
            .chain(["/usr/bin".into(), "/bin".into()])
            .map(|d| d.join("ps"))
            .find(|p| p.is_file())
    }

    /// The host's `ps` with these options (each its own argument, never through a
    /// shell), first with `-q` and the PIDs, then without (some options exclude `-q`).
    pub(super) fn run_ps(args: &[String], pids: &[u32]) -> Result<Top> {
        let ps = find_ps().ok_or_else(|| {
            Error::new(
                ErrorKind::MissingRequirement,
                "cannot run ps",
                "the host has no ps program",
                "install procps, or run top without ps options",
            )
        })?;
        let list: Vec<String> = pids.iter().map(u32::to_string).collect();
        let run = |extra: Option<String>| {
            std::process::Command::new(&ps)
                .args(args)
                .args(extra)
                .stdin(std::process::Stdio::null())
                .output()
        };
        let ok = |o: &std::process::Output| o.status.success();
        let output = match run(Some(format!("-q{}", list.join(",")))) {
            Ok(o) if ok(&o) => o,
            _ => run(None).map_err(|e| Error::io("cannot run ps", &e, "check the ps program"))?,
        };
        if !ok(&output) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "ps failed",
                stderr
                    .lines()
                    .next()
                    .unwrap_or("ps ended with an error")
                    .to_string(),
                "check the ps options",
            ));
        }
        super::parse_ps(&String::from_utf8_lossy(&output.stdout), pids).map_err(|why| {
            Error::new(
                ErrorKind::InvalidInput,
                "cannot read the output of ps",
                why,
                "give ps options that keep the PID column",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ps_output_keeps_the_containers_rows() {
        let out = "UID          PID    PPID  C STIME TTY          TIME CMD\n\
                   root           1       0  0 10:00 ?        00:00:01 /sbin/init splash\n\
                   root         200       1  0 10:01 ?        00:00:00 sleep 1d\n\
                   1000         300     200  0 10:02 pts/0    00:00:00 sh -c echo  two  spaces\n";
        let top = parse_ps(out, &[200, 300]).unwrap();
        assert_eq!(
            top.titles,
            strings(&["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"])
        );
        assert_eq!(
            top.processes,
            [
                strings(&[
                    "root", "200", "1", "0", "10:01", "?", "00:00:00", "sleep 1d"
                ]),
                strings(&[
                    "1000",
                    "300",
                    "200",
                    "0",
                    "10:02",
                    "pts/0",
                    "00:00:00",
                    "sh -c echo two spaces"
                ]),
            ]
        );
    }

    #[test]
    fn thread_lines_follow_their_process() {
        let out = "    PID TTY          TIME CMD\n     10 ?        00:00:00 app\n      - -        00:00:00 -\n     11 ?        00:00:00 other\n      - -        00:00:00 -\n";
        let top = parse_ps(out, &[10]).unwrap();
        assert_eq!(top.processes.len(), 2);
        assert_eq!(top.processes[1][0], "-");
    }

    #[test]
    fn ps_output_needs_a_pid_column() {
        let e = parse_ps("COMMAND\nsleep\n", &[1]).unwrap_err();
        assert_eq!(e, "Couldn't find PID field in ps output");
        let e = parse_ps("PID CMD\nx sleep\n", &[1]).unwrap_err();
        assert!(e.starts_with("Unexpected pid 'x'"), "{e}");
        assert_eq!(parse_ps("PID CMD\n", &[1]).unwrap().processes.len(), 0);
    }

    #[test]
    fn no_other_column_may_be_called_pid() {
        assert!(check_ps_args(&strings(&["-o", "pid,comm"])).is_ok());
        assert!(check_ps_args(&strings(&["-o", "pid=PID,comm"])).is_ok());
        assert!(check_ps_args(&strings(&["aux"])).is_ok());
        assert_eq!(
            check_ps_args(&strings(&["-o", "user=PID"])).unwrap_err(),
            "specifying \"user=PID\" is not allowed"
        );
        assert!(check_ps_args(&strings(&["-o", "pid,user=PIDX"])).is_err());
        assert!(check_ps_args(&strings(&["-o user= PID"])).is_err());
    }

    #[test]
    fn columns_like_ps_ef() {
        let clock = |ticks: u64| {
            let minutes = ticks / 100 / 60;
            Clock {
                year: 2026,
                month: 10,
                day: 20 + (minutes / 1440) as u32,
                hour: (minutes / 60 % 24) as u32,
                minute: (minutes % 60) as u32,
            }
        };
        let procs = [
            Proc {
                pid: 4242,
                ppid: 4200,
                user: "root".into(),
                tty: 0,
                cpu_ticks: 150,
                start_ticks: 100 * 60 * 61,
                command: "sleep 1d".into(),
            },
            Proc {
                pid: 4300,
                ppid: 4242,
                user: "100999".into(),
                tty: (136 << 8) | 2,
                cpu_ticks: 100 * 90_000,
                start_ticks: 100 * 60 * 1500,
                command: "[kworker]".into(),
            },
        ];
        // Now: the next day, 02:00, 100 Hz.
        let now = 100 * 60 * (1440 + 120);
        let top = ps_ef(&procs, 100, now, clock);
        assert_eq!(top.titles.join(" "), "UID PID PPID C STIME TTY TIME CMD");
        assert_eq!(
            top.processes[0],
            strings(&[
                "root", "4242", "4200", "0", "Nov20", "?", "00:00:01", "sleep 1d"
            ])
        );
        assert_eq!(
            top.processes[1],
            strings(&[
                "100999",
                "4300",
                "4242",
                "99",
                "01:00",
                "pts/2",
                "1-01:00:00",
                "[kworker]"
            ])
        );
    }

    #[test]
    fn small_formats() {
        assert_eq!(cpu_time(0), "00:00:00");
        assert_eq!(cpu_time(3725), "01:02:05");
        assert_eq!(cpu_time(2 * 86_400 + 61), "2-00:01:01");
        assert_eq!(tty_name(0), "?");
        assert_eq!(tty_name((136 << 8) | 5), "pts/5");
        assert_eq!(tty_name((137 << 8) | 1), "pts/257");
        assert_eq!(tty_name((4 << 8) | 1), "tty1");
        assert_eq!(tty_name((4 << 8) | 65), "ttyS1");
        assert_eq!(cpu_percent(50, 100), 50);
        assert_eq!(cpu_percent(500, 100), 99);
        assert_eq!(cpu_percent(5, 0), 0);
        let day = |year, month, day| Clock {
            year,
            month,
            day,
            hour: 9,
            minute: 5,
        };
        assert_eq!(stime(day(2026, 10, 20), day(2026, 10, 20)), "09:05");
        assert_eq!(stime(day(2026, 0, 3), day(2026, 10, 20)), "Jan03");
        assert_eq!(stime(day(2025, 11, 31), day(2026, 0, 1)), "2025");
    }

    #[test]
    fn stat_lines_with_odd_names() {
        let (comm, rest) = split_stat("42 (a (b) c) S 1 42 42 0 -1").unwrap();
        assert_eq!(comm, "a (b) c");
        assert_eq!(rest[..2], ["S", "1"]);
        assert!(split_stat("garbage").is_none());
        let u = users("root:x:0:0:root:/root:/bin/sh\nbad\nweb:x:101:101::/:/bin/false\n");
        assert_eq!(u.get(&0).map(String::as_str), Some("root"));
        assert_eq!(u.get(&101).map(String::as_str), Some("web"));
    }
}
