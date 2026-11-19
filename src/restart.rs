//! Restart policies. There is no resident process: whoever cleans up
//! an ended container (the keeper's finisher, or its shim) starts `podbag __restart <id>`,
//! which waits, looks at the container again and starts it.
//!
//! Beside the container's settings, two small files in its directory: `restart.json`
//! (the restarts so far and the last delay), and `stopped` (the user stopped or killed it:
//! no restart until the user starts it again). They are not in `state.json`, which the
//! shim and the keeper rewrite whole while the user may stop the container.

use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// When a container that ended is started again (`--restart`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Never.
    #[default]
    No,
    /// Whatever its exit code.
    Always,
    /// The same as `Always` in podbag: there is no daemon that starts containers again
    /// when the machine starts.
    UnlessStopped,
    /// After a non-zero exit code, at most `max` times (0: no limit).
    OnFailure { max: u32 },
}

fn invalid(policy: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!("invalid restart policy {policy:?}"),
        "the policies are no, always, unless-stopped and on-failure[:max]",
        "give one of them",
    )
}

impl RestartPolicy {
    /// Docker's API form: `{"Name": …, "MaximumRetryCount": …}`.
    pub fn from_docker(name: &str, max: i64) -> Result<RestartPolicy> {
        Ok(match name {
            "" | "no" => RestartPolicy::No,
            "always" => RestartPolicy::Always,
            "unless-stopped" => RestartPolicy::UnlessStopped,
            "on-failure" => RestartPolicy::OnFailure {
                max: u32::try_from(max).map_err(|_| invalid(&format!("{name}:{max}")))?,
            },
            other => return Err(invalid(other)),
        })
    }

    /// The name and the count as Docker shows them.
    pub fn docker(&self) -> (&'static str, u32) {
        match self {
            RestartPolicy::No => ("no", 0),
            RestartPolicy::Always => ("always", 0),
            RestartPolicy::UnlessStopped => ("unless-stopped", 0),
            RestartPolicy::OnFailure { max } => ("on-failure", *max),
        }
    }
}

impl std::str::FromStr for RestartPolicy {
    type Err = Error;

    /// As `--restart` takes it: `no`, `always`, `unless-stopped`, `on-failure[:max]`.
    fn from_str(s: &str) -> Result<RestartPolicy> {
        match s.split_once(':') {
            Some(("on-failure", max)) => Ok(RestartPolicy::OnFailure {
                max: max.parse().map_err(|_| invalid(s))?,
            }),
            Some(_) => Err(invalid(s)),
            None => RestartPolicy::from_docker(s, 0).map_err(|_| invalid(s)),
        }
    }
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestartPolicy::OnFailure { max } if *max > 0 => write!(f, "on-failure:{max}"),
            other => f.write_str(other.docker().0),
        }
    }
}

/// Whether a container that ended with `exit_code` is started again: never after the user
/// stopped it; on failure only up to its limit.
pub fn wanted(policy: &RestartPolicy, exit_code: i32, stopped_by_user: bool, count: u32) -> bool {
    if stopped_by_user {
        return false;
    }
    match policy {
        RestartPolicy::No => false,
        RestartPolicy::Always | RestartPolicy::UnlessStopped => true,
        RestartPolicy::OnFailure { max } => exit_code != 0 && (*max == 0 || count < *max),
    }
}

/// The wait before the next start, as Docker's: 100 ms first, then twice the last one, at
/// most a minute; 100 ms again after a run of 10 seconds or more.
pub fn next_delay(previous_ms: u64, ran: Duration) -> u64 {
    const FIRST: u64 = 100;
    const MOST: u64 = 60_000;
    if previous_ms == 0 || ran >= Duration::from_secs(10) {
        FIRST
    } else {
        previous_ms.saturating_mul(2).clamp(FIRST, MOST)
    }
}

/// The restarts of a container so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// How many times it was started again since the user last started it.
    pub count: u32,
    /// The last wait before a start.
    pub delay_ms: u64,
    /// Why the last start by the restarter failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

const RECORD: &str = "restart.json";
const STOPPED: &str = "stopped";

/// The container's restarts so far (none when it has no record).
pub fn record(dirs: &Dirs, id: &str) -> Record {
    std::fs::read(dirs.container_dir(id).join(RECORD))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn write_record(dirs: &Dirs, id: &str, record: &Record) -> Result<()> {
    let dir = dirs.container_dir(id);
    let tmp = dir.join(".restart.json.tmp");
    let json = serde_json::to_vec(record).unwrap_or_default();
    std::fs::write(&tmp, json)
        .and_then(|()| std::fs::rename(&tmp, dir.join(RECORD)))
        .map_err(|e| {
            Error::io(
                format!(
                    "cannot write the restarts of container {}",
                    crate::id::short_id(id)
                ),
                &e,
                "check that the podbag data directory is writable",
            )
        })
}

/// The user stopped or killed the container: it is not started again.
pub fn mark_stopped(dirs: &Dirs, id: &str) {
    let _ = std::fs::write(dirs.container_dir(id).join(STOPPED), b"");
}

pub fn stopped_by_user(dirs: &Dirs, id: &str) -> bool {
    dirs.container_dir(id).join(STOPPED).exists()
}

/// The user starts the container: its restarts count from zero, and it may be started
/// again.
pub fn user_started(dirs: &Dirs, id: &str) {
    let dir = dirs.container_dir(id);
    let _ = std::fs::remove_file(dir.join(STOPPED));
    let _ = std::fs::remove_file(dir.join(RECORD));
}

/// How an ended container's restarter is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relaunch {
    /// Not at all: the keeper is shutting down, or the container could not be handed over.
    Never,
    /// As a child in a session of its own (root's shims, the keeper's finishers).
    Direct,
    /// Through the user's service manager (`systemd-run --user`): a rootless shim may be
    /// in the network session's user namespace and in the container's systemd scope, and
    /// what it starts would stay in both.
    UserManager,
}

/// The container ended with `code`: start its restarter when its policy wants it.
#[cfg(target_os = "linux")]
pub(crate) fn after_exit(dirs: &Dirs, spec: &crate::spec::ContainerSpec, code: i32, how: Relaunch) {
    if spec.restart == RestartPolicy::No || how == Relaunch::Never {
        return;
    }
    let count = record(dirs, &spec.id).count;
    if !wanted(&spec.restart, code, stopped_by_user(dirs, &spec.id), count) {
        return;
    }
    if let Err(e) = spawn(&spec.id, how) {
        eprintln!("podbag: cannot restart container {}: {e}", spec.name);
    }
}

#[cfg(target_os = "linux")]
fn spawn(id: &str, how: Relaunch) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = crate::helper::locate_helper()?;
    let failed = |e: std::io::Error| Error::io("cannot start the restarter", &e, "report this bug");
    match how {
        Relaunch::Never => Ok(()),
        Relaunch::Direct => {
            let mut cmd = Command::new(exe);
            cmd.args(["__restart", id])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: setsid is async-signal-safe; the child leaves this process's session
            // and terminal, so nothing meant for them reaches it.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            // Not waited for: it outlives this process and is reaped by whoever adopts it.
            cmd.spawn().map(drop).map_err(failed)
        }
        Relaunch::UserManager => {
            let systemd_run = crate::cgroup::systemd_run().ok_or_else(|| {
                Error::new(
                    ErrorKind::MissingRequirement,
                    "cannot start the restarter",
                    "systemd-run was not found",
                    "install systemd, or use the keeper (the default)",
                )
            })?;
            let exe = exe.to_string_lossy().into_owned();
            let status = Command::new(systemd_run)
                .args(user_manager_args(id, &exe, &crate::keeper::settings()))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(failed)?;
            if status.success() {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::Runtime,
                    "cannot start the restarter",
                    format!("systemd-run ended with {status}"),
                    "check the user's service manager (systemctl --user status)",
                ))
            }
        }
    }
}

/// The arguments of `systemd-run --user` for a restarter: a transient service of its own,
/// with the settings of this process. When it ends, what it started stays: without a scope
/// of its own (no cgroup v2), the container it started is in the service's cgroup.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn user_manager_args(id: &str, exe: &str, env: &[(String, String)]) -> Vec<String> {
    let mut args: Vec<String> = [
        "--user",
        "--quiet",
        "--collect",
        "--property=KillMode=process",
    ]
    .map(String::from)
    .to_vec();
    args.push(format!(
        "--unit=podbag-restart-{}-{}",
        crate::id::short_id(id),
        std::process::id()
    ));
    args.extend(env.iter().map(|(k, v)| format!("--setenv={k}={v}")));
    args.extend([exe.to_string(), "__restart".to_string(), id.to_string()]);
    args
}

/// `podbag __restart <id>`: wait, look at the container again, and start it while its
/// policy still wants it. Ends at once when the container was removed, started, or
/// stopped by the user meanwhile.
pub fn restart_main(id: &str) -> i32 {
    #[cfg(target_os = "linux")]
    {
        if crate::id::check(id).is_err() {
            return 2;
        }
        let Ok(dirs) = crate::paths::dirs() else {
            return 1;
        };
        let mut start_failed = false;
        loop {
            let Some((seen, delay)) = due(&dirs, id, start_failed) else {
                return 0;
            };
            let last = record(&dirs, id);
            let count = last.count;
            let _ = write_record(
                &dirs,
                id,
                &Record {
                    delay_ms: delay,
                    ..last
                },
            );
            let until = std::time::Instant::now() + Duration::from_millis(delay);
            loop {
                if changed(&dirs, id, &seen) {
                    return 0;
                }
                let left = until.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(left.min(Duration::from_millis(100)));
            }
            let started = Record {
                count: count + 1,
                delay_ms: delay,
                error: None,
            };
            let _ = write_record(&dirs, id, &started);
            match start(&dirs, id) {
                Ok(()) => return 0,
                // A start that fails is a run that failed at once: the next wait is longer.
                Err(e) => {
                    let _ = write_record(
                        &dirs,
                        id,
                        &Record {
                            error: Some(e.to_string()),
                            ..started
                        },
                    );
                    start_failed = true;
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = id;
        1
    }
}

/// The state the restarter saw and the wait before the next start, while the container's
/// policy wants it started again.
#[cfg(target_os = "linux")]
fn due(dirs: &Dirs, id: &str, start_failed: bool) -> Option<(crate::state::State, u64)> {
    let spec = crate::state::read_config(dirs, id).ok()?;
    let st = crate::state::read_state(dirs, id).ok()?;
    if st.status != crate::state::Status::Exited {
        return None;
    }
    let last = record(dirs, id);
    let code = st.exit_code.unwrap_or(-1);
    if !wanted(&spec.restart, code, stopped_by_user(dirs, id), last.count) {
        return None;
    }
    let ran = match (st.started, st.finished, start_failed) {
        (Some(started), Some(finished), false) => {
            Duration::from_secs(finished.saturating_sub(started))
        }
        _ => Duration::ZERO,
    };
    Some((st, next_delay(last.delay_ms, ran)))
}

/// The container is gone, runs, was started since, or the user stopped it.
#[cfg(target_os = "linux")]
fn changed(dirs: &Dirs, id: &str, seen: &crate::state::State) -> bool {
    match crate::state::read_state(dirs, id) {
        Ok(now) => {
            now.status != crate::state::Status::Exited
                || now.started != seen.started
                || stopped_by_user(dirs, id)
        }
        Err(_) => true,
    }
}

#[cfg(target_os = "linux")]
fn start(dirs: &Dirs, id: &str) -> Result<()> {
    use crate::helper::Io;
    let mut helper = crate::container::start_shim(
        dirs,
        id,
        Io::Null,
        Io::Null,
        Io::Null,
        crate::helper::ShimMode::Detached,
    )?;
    helper.release_detached();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::time::Duration;

    #[test]
    fn policies_read_and_print_as_docker_writes_them() {
        for (text, policy) in [
            ("no", RestartPolicy::No),
            ("always", RestartPolicy::Always),
            ("unless-stopped", RestartPolicy::UnlessStopped),
            ("on-failure", RestartPolicy::OnFailure { max: 0 }),
            ("on-failure:3", RestartPolicy::OnFailure { max: 3 }),
        ] {
            assert_eq!(text.parse::<RestartPolicy>().unwrap(), policy, "{text}");
            assert_eq!(policy.to_string(), text);
            let json = serde_json::to_string(&policy).unwrap();
            assert_eq!(
                serde_json::from_str::<RestartPolicy>(&json).unwrap(),
                policy
            );
        }
        assert_eq!("".parse::<RestartPolicy>().unwrap(), RestartPolicy::No);
        for bad in [
            "sometimes",
            "on-failure:x",
            "on-failure:-1",
            "always:3",
            "no:1",
        ] {
            assert_eq!(
                bad.parse::<RestartPolicy>().map_err(|e| e.kind),
                Err(ErrorKind::InvalidInput),
                "{bad}"
            );
        }
        // Docker's API form: a name and a count.
        assert_eq!(
            RestartPolicy::from_docker("on-failure", 2).unwrap(),
            RestartPolicy::OnFailure { max: 2 }
        );
        assert_eq!(
            RestartPolicy::from_docker("", 0).unwrap(),
            RestartPolicy::No
        );
        assert!(RestartPolicy::from_docker("sometimes", 0).is_err());
        assert_eq!(
            RestartPolicy::OnFailure { max: 2 }.docker(),
            ("on-failure", 2)
        );
        assert_eq!(RestartPolicy::Always.docker(), ("always", 0));
        assert_eq!(RestartPolicy::No.docker(), ("no", 0));
        assert_eq!(RestartPolicy::default(), RestartPolicy::No);
    }

    #[test]
    fn wanted_follows_the_policy() {
        use RestartPolicy::*;
        for (policy, code, count, want) in [
            (No, 1, 0, false),
            (Always, 0, 0, true),
            (Always, 1, 50, true),
            (UnlessStopped, 0, 0, true),
            (OnFailure { max: 0 }, 0, 0, false),
            (OnFailure { max: 0 }, 1, 99, true),
            (OnFailure { max: 2 }, 1, 1, true),
            (OnFailure { max: 2 }, 1, 2, false),
            (OnFailure { max: 2 }, 137, 0, true),
        ] {
            assert_eq!(
                wanted(&policy, code, false, count),
                want,
                "{policy} {code} {count}"
            );
            // Stopped by the user: never.
            assert!(!wanted(&policy, code, true, count), "{policy} stopped");
        }
    }

    #[test]
    fn delays_double_up_to_a_minute_and_start_again_after_ten_seconds() {
        let short = Duration::from_secs(1);
        let mut delay = 0;
        let mut seen = Vec::new();
        for _ in 0..12 {
            delay = next_delay(delay, short);
            seen.push(delay);
        }
        assert_eq!(seen[..6], [100, 200, 400, 800, 1600, 3200]);
        assert_eq!((seen[10], seen[11]), (60_000, 60_000));
        assert_eq!(next_delay(60_000, Duration::from_secs(10)), 100);
        assert_eq!(next_delay(3200, Duration::from_secs(9)), 6400);
    }

    #[test]
    fn the_restarter_leaves_the_new_container_alive() {
        // A transient service's processes are killed when its main process ends; the
        // container that the restarter started stays in it without cgroup v2 scopes.
        let args = user_manager_args("ab".repeat(32).as_str(), "/bin/podbag", &[]);
        assert!(
            args.iter().any(|a| a == "--property=KillMode=process"),
            "{args:?}"
        );
        assert!(args.ends_with(&[
            "/bin/podbag".to_string(),
            "__restart".to_string(),
            "ab".repeat(32)
        ]));
    }

    #[test]
    fn the_record_of_a_container_starts_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = crate::paths::Dirs {
            data: tmp.path().join("data"),
            runtime: tmp.path().join("run"),
            config: tmp.path().join("cfg"),
        };
        let id = "ab".repeat(32);
        std::fs::create_dir_all(dirs.container_dir(&id)).unwrap();
        assert_eq!(record(&dirs, &id), Record::default());
        assert!(!stopped_by_user(&dirs, &id));
        mark_stopped(&dirs, &id);
        assert!(stopped_by_user(&dirs, &id));
        let two = Record {
            count: 2,
            delay_ms: 200,
            error: None,
        };
        write_record(&dirs, &id, &two).unwrap();
        assert_eq!(record(&dirs, &id), two);
        // A start by the user: the count, the delay and the mark go.
        user_started(&dirs, &id);
        assert_eq!(record(&dirs, &id), Record::default());
        assert!(!stopped_by_user(&dirs, &id));
    }
}
