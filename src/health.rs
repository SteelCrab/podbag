//! Health checks. There is no resident process for them: the process
//! that watches a running container (the keeper, or its shim) starts `podbag __health
//! <id>` when a check is due; that process runs the check inside the container, writes the
//! result to `health.json` in the container's directory and ends. Between checks nothing
//! runs.

use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Docker's defaults.
pub const DEFAULT_INTERVAL_MS: u64 = 30_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_RETRIES: u32 = 3;
pub const DEFAULT_START_INTERVAL_MS: u64 = 5_000;
/// Docker keeps this much output of each probe, and this many probes.
const OUTPUT_KEPT: usize = 4096;
const PROBES_KEPT: usize = 5;
const RECORD: &str = "health.json";

/// A container's health check, as it was settled when the container was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthConfig {
    /// As Docker writes it: `["CMD", program, args…]` or `["CMD-SHELL", command]`.
    pub test: Vec<String>,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub retries: u32,
    /// Failures in this time after a start do not count, until the first success.
    pub start_period_ms: u64,
    /// The interval during the start period.
    pub start_interval_ms: u64,
}

/// A check as a request, the command line or an image gives it: fields that are not given
/// come from the image's check, then from Docker's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSettings {
    /// Empty: the image's test. `["NONE"]`: no check at all, not even the image's.
    #[serde(default)]
    pub test: Vec<String>,
    #[serde(default)]
    pub interval_ms: Option<u64>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub retries: Option<u32>,
    #[serde(default)]
    pub start_period_ms: Option<u64>,
    #[serde(default)]
    pub start_interval_ms: Option<u64>,
}

fn invalid(why: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "invalid health check",
        why,
        "give the test as [\"CMD\", program, args…] or [\"CMD-SHELL\", command]",
    )
}

impl HealthSettings {
    /// No check at all (`--no-healthcheck`, `{"Test": ["NONE"]}`).
    pub fn none() -> HealthSettings {
        HealthSettings {
            test: vec!["NONE".to_string()],
            ..Default::default()
        }
    }

    /// Docker's JSON form, as [`HealthSettings::from_docker`] reads it.
    pub fn to_docker(&self) -> serde_json::Value {
        let ns = |ms: Option<u64>| ms.map_or(0, |ms| ms.saturating_mul(1_000_000));
        serde_json::json!({
            "Test": self.test,
            "Interval": ns(self.interval_ms),
            "Timeout": ns(self.timeout_ms),
            "StartPeriod": ns(self.start_period_ms),
            "StartInterval": ns(self.start_interval_ms),
            "Retries": self.retries.unwrap_or(0),
        })
    }

    /// Docker's JSON form (`Healthcheck` of a create request or of an image's config):
    /// durations in nanoseconds, 0 for "not given". None when there is none.
    pub fn from_docker(v: &serde_json::Value) -> Result<Option<HealthSettings>> {
        let Some(map) = v.as_object() else {
            return if v.is_null() {
                Ok(None)
            } else {
                Err(invalid("the health check is not an object"))
            };
        };
        let test = match map.get("Test") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(list)) => list
                .iter()
                .map(|t| t.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| invalid("the test is not a list of strings"))?,
            Some(_) => return Err(invalid("the test is not a list of strings")),
        };
        let ms = |key: &str| -> Result<Option<u64>> {
            match map.get(key).and_then(serde_json::Value::as_i64) {
                None | Some(0) => Ok(None),
                Some(ns) if ns > 0 => Ok(Some((ns as u64).div_ceil(1_000_000))),
                Some(_) => Err(invalid(format!("{key} is negative"))),
            }
        };
        let retries = match map.get("Retries").and_then(serde_json::Value::as_i64) {
            None | Some(0) => None,
            Some(n) => Some(u32::try_from(n).map_err(|_| invalid("Retries is out of range"))?),
        };
        Ok(Some(HealthSettings {
            test,
            interval_ms: ms("Interval")?,
            timeout_ms: ms("Timeout")?,
            retries,
            start_period_ms: ms("StartPeriod")?,
            start_interval_ms: ms("StartInterval")?,
        }))
    }
}

/// Check a test's form: `CMD` with a program, or `CMD-SHELL` with a command.
fn check_test(test: &[String]) -> Result<()> {
    match test.first().map(String::as_str) {
        Some("CMD") if test.len() > 1 && !test[1].is_empty() => Ok(()),
        Some("CMD-SHELL") if test.len() > 1 && !test[1..].join(" ").trim().is_empty() => Ok(()),
        Some("CMD" | "CMD-SHELL") => Err(invalid("the test has no command")),
        Some(other) => Err(invalid(format!("a test cannot start with {other:?}"))),
        None => Err(invalid("the test is empty")),
    }
}

/// The check a container gets: its own settings over the image's, then Docker's defaults.
/// None: no check.
pub fn resolve(
    own: Option<&HealthSettings>,
    image: Option<&HealthSettings>,
) -> Result<Option<HealthConfig>> {
    let given = |s: Option<&HealthSettings>| s.map(|s| s.test.clone()).filter(|t| !t.is_empty());
    let test = match given(own).or_else(|| given(image)) {
        None => return Ok(None),
        Some(t) if t.first().map(String::as_str) == Some("NONE") => return Ok(None),
        Some(t) => t,
    };
    check_test(&test)?;
    let pick = |f: fn(&HealthSettings) -> Option<u64>, default: u64| {
        own.and_then(f)
            .or_else(|| image.and_then(f))
            .filter(|v| *v > 0)
            .unwrap_or(default)
    };
    Ok(Some(HealthConfig {
        test,
        interval_ms: pick(|s| s.interval_ms, DEFAULT_INTERVAL_MS),
        timeout_ms: pick(|s| s.timeout_ms, DEFAULT_TIMEOUT_MS),
        retries: own
            .and_then(|s| s.retries)
            .or_else(|| image.and_then(|s| s.retries))
            .filter(|r| *r > 0)
            .unwrap_or(DEFAULT_RETRIES),
        start_period_ms: pick(|s| s.start_period_ms, 0),
        start_interval_ms: pick(|s| s.start_interval_ms, DEFAULT_START_INTERVAL_MS),
    }))
}

/// The program and arguments a test runs.
pub fn command_of(test: &[String]) -> Vec<String> {
    match test.first().map(String::as_str) {
        Some("CMD-SHELL") => vec!["/bin/sh".into(), "-c".into(), test[1..].join(" ")],
        Some("CMD") => test[1..].to_vec(),
        _ => test.to_vec(),
    }
}

/// A Go duration, as the docker CLI takes them: `30s`, `1m30s`, `500ms`, `1.5s`, `2h`.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let bad = || {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid duration {text:?}"),
            "a duration is a number with a unit: ns, us, ms, s, m or h (30s, 1m30s, 500ms)",
            "give it with a unit",
        )
    };
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    let mut rest = text;
    let mut total = 0f64;
    if rest.is_empty() {
        return Err(bad());
    }
    while !rest.is_empty() {
        let number_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(bad)?;
        let number: f64 = rest[..number_end].parse().map_err(|_| bad())?;
        rest = &rest[number_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit_end] {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(bad()),
        };
        total += number * seconds;
        rest = &rest[unit_end..];
    }
    Duration::try_from_secs_f64(total).map_err(|_| bad())
}

/// A container's health, as Docker shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Starting,
    Healthy,
    Unhealthy,
}

impl Status {
    /// What `ps` adds to "Up …".
    pub fn ps_suffix(self) -> &'static str {
        match self {
            Status::Starting => " (health: starting)",
            Status::Healthy => " (healthy)",
            Status::Unhealthy => " (unhealthy)",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Healthy => "healthy",
            Status::Unhealthy => "unhealthy",
        }
    }
}

/// One run of the check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    /// RFC 3339 times.
    pub start: String,
    pub end: String,
    /// The check's exit code; -1 when it could not run or ran too long.
    pub exit_code: i32,
    /// Its output, at most 4096 bytes.
    pub output: String,
}

/// A container's health since its last start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub status: Status,
    pub failing_streak: u32,
    /// The last five probes, oldest first.
    pub log: Vec<Probe>,
}

/// The health after `probe`, as Docker counts: a success makes it healthy; a failure
/// counts towards `retries`, except in the start period before the first success.
pub fn after(
    previous: Option<Health>,
    mut probe: Probe,
    config: &HealthConfig,
    in_start_period: bool,
) -> Health {
    let mut health = previous.unwrap_or_default();
    if probe.exit_code == 0 {
        health.status = Status::Healthy;
        health.failing_streak = 0;
    } else if !(in_start_period && health.status == Status::Starting) {
        health.failing_streak += 1;
        if health.failing_streak >= config.retries.max(1) {
            health.status = Status::Unhealthy;
        }
    }
    if probe.output.len() > OUTPUT_KEPT {
        let mut end = OUTPUT_KEPT;
        while !probe.output.is_char_boundary(end) {
            end -= 1;
        }
        probe.output.truncate(end);
    }
    health.log.push(probe);
    let extra = health.log.len().saturating_sub(PROBES_KEPT);
    health.log.drain(..extra);
    health
}

/// The container's health since its last start; None before its first check.
pub fn read(dirs: &Dirs, id: &str) -> Option<Health> {
    let bytes = std::fs::read(dirs.container_dir(id).join(RECORD)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write(dirs: &Dirs, id: &str, health: &Health) -> Result<()> {
    let dir = dirs.container_dir(id);
    let tmp = dir.join(".health.json.tmp");
    let json = serde_json::to_vec(health).unwrap_or_default();
    std::fs::write(&tmp, json)
        .and_then(|()| std::fs::rename(&tmp, dir.join(RECORD)))
        .map_err(|e| {
            Error::io(
                "cannot write the container's health",
                &e,
                "check that the podbag data directory is writable",
            )
        })
}

/// A start begins a new health: the old record goes.
pub fn reset(dirs: &Dirs, id: &str) {
    let _ = std::fs::remove_file(dirs.container_dir(id).join(RECORD));
}

/// When the watcher of a running container starts checks: one at a time, `interval` after
/// the last one ended (`start_interval` in the start period), the first one `interval`
/// after the start.
#[derive(Debug)]
pub struct Schedule {
    interval: Duration,
    start_interval: Duration,
    start_period: Duration,
    timeout: Duration,
    started: Instant,
    last_end: Option<Instant>,
    /// The probe that runs now, and when it began.
    probe: Option<(u32, Instant)>,
}

/// A probe gets this long beyond its own timeout before its watcher kills it.
const PROBE_GRACE: Duration = Duration::from_secs(10);

impl Schedule {
    pub fn new(config: &HealthConfig, started: Instant) -> Schedule {
        Schedule {
            interval: Duration::from_millis(config.interval_ms.max(1)),
            start_interval: Duration::from_millis(config.start_interval_ms.max(1)),
            start_period: Duration::from_millis(config.start_period_ms),
            timeout: Duration::from_millis(config.timeout_ms),
            started,
            last_end: None,
            probe: None,
        }
    }

    fn in_start_period(&self, at: Instant) -> bool {
        at.saturating_duration_since(self.started) < self.start_period
    }

    /// When something is to be done: the next check, or killing a probe that hangs.
    pub fn next_at(&self) -> Instant {
        match self.probe {
            Some((_, began)) => began + self.timeout + PROBE_GRACE,
            None => {
                let base = self.last_end.unwrap_or(self.started);
                let wait = if self.in_start_period(base) {
                    self.start_interval
                } else {
                    self.interval
                };
                base + wait
            }
        }
    }

    /// At `now`: start a check when one is due (`spawn` starts the probe process, told
    /// whether the container is in its start period, and gives its PID). Returns a probe
    /// that hangs, for the caller to kill.
    pub fn tick(&mut self, now: Instant, spawn: impl FnOnce(bool) -> Option<u32>) -> Option<u32> {
        match self.probe {
            Some((pid, began)) => (now >= began + self.timeout + PROBE_GRACE).then_some(pid),
            None => {
                if now >= self.next_at() {
                    match spawn(self.in_start_period(now)) {
                        Some(pid) => self.probe = Some((pid, now)),
                        // No process now: the next try after an interval.
                        None => self.last_end = Some(now),
                    }
                }
                None
            }
        }
    }

    /// Process `pid` ended. True when it was this schedule's probe.
    pub fn ended(&mut self, pid: u32, now: Instant) -> bool {
        if self.probe.is_some_and(|(p, _)| p == pid) {
            self.probe = None;
            self.last_end = Some(now);
            true
        } else {
            false
        }
    }

    /// The probe that runs now.
    pub fn probe(&self) -> Option<u32> {
        self.probe.map(|(pid, _)| pid)
    }
}

/// The environment a keeper gives a probe: the container's saved settings, or (when there
/// are none) the keeper's own. An empty environment would leave the probe without HOME and
/// the podbag directories: every check would fail without a word.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn probe_env(saved: &[(String, String)]) -> Option<&[(String, String)]> {
    (!saved.is_empty()).then_some(saved)
}

/// Start a probe for container `id` (`podbag __health <id>`), with `env` as its whole
/// environment when given. Its watcher reaps it. None when no process could be started.
#[cfg(target_os = "linux")]
pub(crate) fn spawn_probe(
    id: &str,
    in_start_period: bool,
    env: Option<&[(String, String)]>,
) -> Option<u32> {
    use std::process::{Command, Stdio};
    let exe = crate::helper::locate_helper().ok()?;
    let mut cmd = Command::new(exe);
    cmd.args(["__health", id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if in_start_period {
        cmd.arg("--start-period");
    }
    if let Some(env) = env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    let child = cmd.spawn().ok()?;
    let pid = child.id();
    // Reaped by the watcher (waitpid), not through `child`.
    std::mem::forget(child);
    Some(pid)
}

/// `podbag __health <id>`: run container `id`'s check once inside it, and write the
/// result into its health record.
pub fn probe_main(id: &str, in_start_period: bool) -> i32 {
    #[cfg(target_os = "linux")]
    {
        if crate::id::check(id).is_err() {
            return 2;
        }
        let Ok(dirs) = crate::paths::dirs() else {
            return 1;
        };
        let Some(config) = crate::state::read_config(&dirs, id)
            .ok()
            .and_then(|spec| spec.health)
        else {
            return 1;
        };
        let start = now_text();
        let (exit_code, output) = run_check(id, &config);
        let probe = Probe {
            start,
            end: now_text(),
            exit_code,
            output,
        };
        let health = after(read(&dirs, id), probe, &config, in_start_period);
        match write(&dirs, id, &health) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (id, in_start_period);
        1
    }
}

#[cfg(target_os = "linux")]
fn now_text() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    crate::shim::logbook::rfc3339(now.as_secs(), now.subsec_nanos())
}

/// Run the check in the container: its exit code and output (both streams), or -1 and why
/// when it could not run or ran past its timeout (then it is ended).
#[cfg(target_os = "linux")]
fn run_check(id: &str, config: &HealthConfig) -> (i32, String) {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::Stdio;
    let failed = |why: String| (-1, why);
    let handle = match crate::ContainerHandle::get(id) {
        Ok(h) => h,
        Err(e) => return failed(e.to_string()),
    };
    let (mut reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return failed(format!("cannot run the check: {e}")),
    };
    let Ok(writer2) = writer.try_clone() else {
        return failed("cannot run the check".to_string());
    };
    let running = handle.exec_with(
        &command_of(&config.test),
        &crate::containers::ExecOptions::default(),
        Stdio::null(),
        Stdio::from(writer),
        Stdio::from(writer2),
        None,
    );
    let running = match running {
        Ok(r) => r,
        Err(e) => return failed(e.to_string()),
    };
    let helper = running.helper_pid();
    // SAFETY: pidfd_open on our own child; the result is a new descriptor or -1.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, helper as libc::pid_t, 0) };
    // SAFETY: a fresh descriptor that nothing else owns.
    let pidfd = (pidfd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(pidfd as i32) });
    let timeout = Duration::from_millis(config.timeout_ms);
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    let mut timed_out = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            timed_out = true;
            break;
        }
        let mut p = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut p, 1, left.as_millis().clamp(1, 60_000) as i32) };
        if ready == 0 {
            continue;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if output.len() < OUTPUT_KEPT {
                    output.extend_from_slice(&buf[..n.min(OUTPUT_KEPT - output.len())]);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    if timed_out {
        // The exec helper passes SIGTERM on to the check; whatever does not end with it
        // ends with the helper's SIGKILL a second later.
        let signal = |sig: i32| match &pidfd {
            // SAFETY: a signal through our own child's pidfd.
            Some(fd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            },
            // SAFETY: a signal to our own child, not reaped yet.
            None => unsafe {
                libc::kill(helper as libc::pid_t, sig);
            },
        };
        signal(libc::SIGTERM);
        let ended = pidfd.as_ref().is_some_and(|fd| {
            let mut p = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            unsafe { libc::poll(&mut p, 1, 1000) > 0 }
        });
        if !ended {
            signal(libc::SIGKILL);
        }
        let _ = running.wait();
        return failed(format!(
            "Health check exceeded timeout ({}ms)",
            timeout.as_millis()
        ));
    }
    let code = running.wait().unwrap_or(-1);
    (code, String::from_utf8_lossy(&output).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::time::{Duration, Instant};

    fn config(retries: u32) -> HealthConfig {
        HealthConfig {
            test: vec!["CMD".into(), "true".into()],
            interval_ms: 1000,
            timeout_ms: 1000,
            retries,
            start_period_ms: 0,
            start_interval_ms: 5000,
        }
    }

    fn probe(code: i32) -> Probe {
        Probe {
            start: "s".into(),
            end: "e".into(),
            exit_code: code,
            output: format!("{code}"),
        }
    }

    #[test]
    fn health_follows_dockers_rules() {
        let c = config(2);
        let state = |h: &Health| (h.status, h.failing_streak);
        // A success makes it healthy.
        let h = after(None, probe(0), &c, false);
        assert_eq!(state(&h), (Status::Healthy, 0));
        // Failures count; at `retries` it is unhealthy; a success makes it healthy again.
        let h = after(Some(h), probe(1), &c, false);
        assert_eq!(state(&h), (Status::Healthy, 1));
        let h = after(Some(h), probe(1), &c, false);
        assert_eq!(state(&h), (Status::Unhealthy, 2));
        let h = after(Some(h), probe(0), &c, false);
        assert_eq!(state(&h), (Status::Healthy, 0));
        // In the start period, before the first success, failures do not count.
        let h = after(None, probe(1), &c, true);
        assert_eq!(state(&h), (Status::Starting, 0));
        let h = after(Some(h), probe(7), &c, true);
        assert_eq!(state(&h), (Status::Starting, 0));
        // After it, they do.
        let h = after(Some(h), probe(1), &c, false);
        assert_eq!(state(&h), (Status::Starting, 1));
        let h = after(Some(h), probe(-1), &c, false);
        assert_eq!(state(&h), (Status::Unhealthy, 2));
        // A failure in the start period after a success counts.
        let h = after(None, probe(0), &c, true);
        let h = after(Some(h), probe(1), &c, true);
        assert_eq!(state(&h), (Status::Healthy, 1));
        assert_eq!(Status::default(), Status::Starting);
        assert_eq!(
            [Status::Starting, Status::Healthy, Status::Unhealthy].map(Status::name),
            ["starting", "healthy", "unhealthy"]
        );
    }

    #[test]
    fn the_log_keeps_five_probes_and_bounded_output() {
        let c = config(3);
        let mut h = None;
        for code in 0..7 {
            h = Some(after(h, probe(code), &c, false));
        }
        let codes: Vec<i32> = h.unwrap().log.iter().map(|p| p.exit_code).collect();
        assert_eq!(codes, [2, 3, 4, 5, 6]);
        // 6000 bytes of two-byte characters: cut at a character's edge, at most 4096.
        let mut long = probe(0);
        long.output = "é".repeat(3000);
        let kept = after(None, long, &c, false).log[0].output.len();
        assert!((4095..=4096).contains(&kept), "{kept}");
    }

    #[test]
    fn tests_are_read_in_every_form() {
        let own = |test: &[&str]| HealthSettings {
            test: test.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        };
        let image = HealthSettings {
            test: vec!["CMD".into(), "img".into()],
            interval_ms: Some(2000),
            retries: Some(5),
            ..Default::default()
        };
        // Its own test, the image's values for the rest, then Docker's defaults.
        let r = resolve(Some(&own(&["CMD-SHELL", "curl -f x"])), Some(&image))
            .unwrap()
            .unwrap();
        assert_eq!(r.test, ["CMD-SHELL", "curl -f x"]);
        assert_eq!(
            (
                r.interval_ms,
                r.retries,
                r.timeout_ms,
                r.start_period_ms,
                r.start_interval_ms
            ),
            (2000, 5, 30_000, 0, 5_000)
        );
        assert_eq!(command_of(&r.test), ["/bin/sh", "-c", "curl -f x"]);
        // No test of its own: the image's.
        let intervals = HealthSettings {
            interval_ms: Some(1000),
            ..Default::default()
        };
        let r = resolve(Some(&intervals), Some(&image)).unwrap().unwrap();
        assert_eq!(
            (r.test.as_slice(), r.interval_ms),
            (&["CMD".to_string(), "img".to_string()][..], 1000)
        );
        assert_eq!(command_of(&r.test), ["img"]);
        // NONE turns the image's off; nothing anywhere is none.
        assert_eq!(
            resolve(Some(&HealthSettings::none()), Some(&image)).unwrap(),
            None
        );
        assert_eq!(resolve(None, None).unwrap(), None);
        assert_eq!(resolve(Some(&intervals), None).unwrap(), None);
        assert!(resolve(None, Some(&image)).unwrap().is_some());
        // Tests that are not one.
        for bad in [&["CMD"][..], &["CMD-SHELL", " "], &["RUN", "x"]] {
            assert_eq!(
                resolve(Some(&own(bad)), None).map_err(|e| e.kind),
                Err(ErrorKind::InvalidInput),
                "{bad:?}"
            );
        }
        // Docker's JSON form: nanoseconds, 0 for "not given".
        let d = HealthSettings::from_docker(&serde_json::json!({
            "Test": ["CMD", "true"], "Interval": 1_500_000_000_u64, "Timeout": 0,
            "Retries": 2, "StartPeriod": 3_000_000_000_u64
        }))
        .unwrap()
        .unwrap();
        assert_eq!(
            (d.interval_ms, d.timeout_ms, d.retries, d.start_period_ms),
            (Some(1500), None, Some(2), Some(3000))
        );
        assert_eq!(
            HealthSettings::from_docker(&serde_json::Value::Null).unwrap(),
            None
        );
        assert_eq!(
            HealthSettings::from_docker(&serde_json::json!({"Test": ["NONE"]})).unwrap(),
            Some(HealthSettings::none())
        );
        assert!(HealthSettings::from_docker(&serde_json::json!({"Test": "true"})).is_err());
        assert!(HealthSettings::from_docker(&serde_json::json!({"Interval": -1})).is_err());
    }

    #[test]
    fn durations_are_read_as_docker_writes_them() {
        for (text, ms) in [
            ("30s", 30_000),
            ("1m30s", 90_000),
            ("500ms", 500),
            ("1.5s", 1500),
            ("2h", 7_200_000),
            ("0", 0),
            ("100us", 0),
        ] {
            assert_eq!(parse_duration(text).unwrap().as_millis(), ms, "{text}");
        }
        for bad in ["", "5", "s", "1x", "1.2.3s", "-1s"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_probe_without_saved_settings_keeps_the_watchers_environment() {
        // An empty list is no environment at all: HOME and the podbag directories would
        // be missing, and every check would fail without a word.
        assert_eq!(probe_env(&[]), None);
        let saved = vec![("HOME".to_string(), "/h".to_string())];
        assert_eq!(probe_env(&saved), Some(saved.as_slice()));
    }

    #[test]
    fn a_schedule_never_runs_two_probes() {
        let mut c = config(3);
        c.interval_ms = 1000;
        c.timeout_ms = 500;
        c.start_period_ms = 3000;
        c.start_interval_ms = 200;
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut s = Schedule::new(&c, t0);
        // In the start period: the start interval.
        assert_eq!(s.next_at(), at(200));
        assert_eq!(s.tick(at(100), |_| panic!("not due")), None);
        let mut told = Vec::new();
        s.tick(at(200), |in_start_period| {
            told.push(in_start_period);
            Some(42)
        });
        assert_eq!(told, [true]);
        assert_eq!(s.probe(), Some(42));
        // While it runs, no second one; one that hangs is given back to be killed.
        assert_eq!(s.tick(at(5000), |_| panic!("a second probe")), None);
        assert_eq!(s.tick(at(10_700), |_| panic!("a second probe")), Some(42));
        assert!(!s.ended(7, at(900)));
        assert!(s.ended(42, at(900)));
        assert_eq!(s.probe(), None);
        // Counted from the last end: still in the start period.
        assert_eq!(s.next_at(), at(1100));
        // After the start period: the interval.
        let mut later = Schedule::new(&c, t0);
        later.tick(at(200), |_| Some(1));
        later.ended(1, at(3500));
        assert_eq!(later.next_at(), at(4500));
        // No process could be started: try again an interval later.
        let mut none = Schedule::new(&c, t0);
        none.tick(at(200), |_| None);
        assert_eq!(none.probe(), None);
        assert_eq!(none.next_at(), at(400));
    }
}
