//! Docker's names, times and texts, shared by the API handlers.

use crate::state::{State, Status};

/// The name the docker CLI shows: the default registry and `library/` left out.
pub fn docker_image_name(full: &str) -> String {
    let short = full
        .strip_prefix("docker.io/library/")
        .or_else(|| full.strip_prefix("docker.io/"))
        .unwrap_or(full);
    short.to_string()
}

/// The repository part of a name, without its tag: `docker.io/library/alpine:3.22` → `alpine`.
pub fn docker_repo(full: &str) -> String {
    let name = docker_image_name(full);
    let name = name.split('@').next().unwrap_or(&name);
    // A tag follows the last colon after the last slash.
    match name.rfind(':') {
        Some(i) if !name[i..].contains('/') => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// Docker's architecture names.
pub fn docker_arch(machine: &str) -> String {
    match machine {
        "aarch64" => "arm64".to_string(),
        "x86_64" => "amd64".to_string(),
        other => other.to_string(),
    }
}

/// RFC 3339 time for epoch seconds (`0001-01-01T00:00:00Z` for unknown).
pub fn rfc3339(secs: Option<u64>) -> String {
    match secs {
        Some(s) => crate::shim::logbook::rfc3339(s, 0),
        None => "0001-01-01T00:00:00Z".to_string(),
    }
}

/// Seconds since the epoch for an RFC 3339 UTC time like `2006-01-02T15:04:05.999999999Z`.
pub fn epoch_of(time: &str) -> Option<f64> {
    let (date, clock) = time.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let clock = clock.strip_suffix('Z')?;
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, "0"));
    let mut t = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, min, sec) = (t.next()??, t.next()??, t.next()??);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let fraction: f64 = format!("0.{fraction}").parse().ok()?;
    Some((days * 86_400 + h * 3600 + min * 60 + sec) as f64 + fraction)
}

/// Docker's human durations (go-units HumanDuration).
pub fn human_duration(secs: u64) -> String {
    let minutes = secs / 60;
    let hours = (secs + 1800) / 3600;
    match secs {
        0 => "Less than a second".to_string(),
        1 => "1 second".to_string(),
        s if s < 60 => format!("{s} seconds"),
        _ if minutes == 1 => "About a minute".to_string(),
        _ if minutes < 60 => format!("{minutes} minutes"),
        _ if hours == 1 => "About an hour".to_string(),
        _ if hours < 48 => format!("{hours} hours"),
        _ if hours < 24 * 7 * 2 => format!("{} days", hours / 24),
        _ if hours < 24 * 30 * 2 => format!("{} weeks", hours / 24 / 7),
        _ if hours < 24 * 365 * 2 => format!("{} months", hours / 24 / 30),
        _ => format!("{} years", secs / 3600 / 24 / 365),
    }
}

/// The STATUS column: `Created`, `Up 5 minutes`, `Exited (0) 3 seconds ago`.
pub fn status_text(state: &State, now: u64) -> String {
    match state.status {
        Status::Created => "Created".to_string(),
        Status::Running => format!(
            "Up {}{}",
            human_duration(now.saturating_sub(state.started.unwrap_or(now))),
            if state.paused { " (Paused)" } else { "" }
        ),
        Status::Exited => format!(
            "Exited ({}) {} ago",
            state.exit_code.unwrap_or(-1),
            human_duration(now.saturating_sub(state.finished.unwrap_or(now)))
        ),
    }
}

/// Docker's state name of a container: `paused` while it is paused.
pub fn state_label(state: &State) -> &'static str {
    if state.status == Status::Running && state.paused {
        "paused"
    } else {
        state_name(state.status)
    }
}

/// Docker's lowercase state names.
pub fn state_name(status: Status) -> &'static str {
    match status {
        Status::Created => "created",
        Status::Running => "running",
        Status::Exited => "exited",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{State, Status};

    fn state(
        status: Status,
        started: Option<u64>,
        finished: Option<u64>,
        code: Option<i32>,
    ) -> State {
        State {
            status,
            pid: None,
            shim_pid: None,
            exit_code: code,
            created: 0,
            started,
            finished,
            cgroup: None,
            oom: None,
            ip: None,
            ports: vec![],
            paused: false,
        }
    }

    #[test]
    fn docker_names_drop_the_default_registry() {
        assert_eq!(
            docker_image_name("docker.io/library/alpine:3.22"),
            "alpine:3.22"
        );
        assert_eq!(
            docker_image_name("docker.io/bitnami/redis:7"),
            "bitnami/redis:7"
        );
        assert_eq!(
            docker_image_name("127.0.0.1:5000/x/y:v1"),
            "127.0.0.1:5000/x/y:v1"
        );
        assert_eq!(docker_repo("docker.io/library/alpine:3.22"), "alpine");
        assert_eq!(docker_repo("127.0.0.1:5000/x/y:v1"), "127.0.0.1:5000/x/y");
    }

    #[test]
    fn status_text_matches_docker() {
        let now = 10_000_000;
        assert_eq!(
            status_text(&state(Status::Created, None, None, None), now),
            "Created"
        );
        let mut paused = state(Status::Running, Some(now - 600), None, None);
        paused.paused = true;
        assert_eq!(status_text(&paused, now), "Up 10 minutes (Paused)");
        assert_eq!(state_label(&paused), "paused");
        let up = |secs| status_text(&state(Status::Running, Some(now - secs), None, None), now);
        assert_eq!(up(0), "Up Less than a second");
        assert_eq!(up(1), "Up 1 second");
        assert_eq!(up(45), "Up 45 seconds");
        assert_eq!(up(90), "Up About a minute");
        assert_eq!(up(600), "Up 10 minutes");
        assert_eq!(up(3600), "Up About an hour");
        assert_eq!(up(5 * 3600), "Up 5 hours");
        assert_eq!(up(3 * 86400), "Up 3 days");
        let exited = status_text(
            &state(Status::Exited, Some(1), Some(now - 120), Some(137)),
            now,
        );
        assert_eq!(exited, "Exited (137) 2 minutes ago");
    }

    #[test]
    fn arch_names_are_docker_s() {
        assert_eq!(docker_arch("aarch64"), "arm64");
        assert_eq!(docker_arch("x86_64"), "amd64");
        assert_eq!(docker_arch("riscv64"), "riscv64");
    }

    #[test]
    fn errors_map_to_http_status() {
        use crate::api::router::{Reply, reply_error};
        use crate::{Error, ErrorKind};
        let status = |kind| match reply_error(&Error::new(kind, "w", "y", "n")) {
            Reply::Json(s, _) => s,
            _ => 0,
        };
        assert_eq!(status(ErrorKind::NotFound), 404);
        assert_eq!(status(ErrorKind::Conflict), 409);
        assert_eq!(status(ErrorKind::InvalidInput), 400);
        assert_eq!(status(ErrorKind::Unsupported), 501);
        assert_eq!(status(ErrorKind::Runtime), 500);
    }

    #[test]
    fn log_times_read_back() {
        assert_eq!(
            epoch_of(&rfc3339(Some(1_700_000_000))),
            Some(1_700_000_000.0)
        );
        assert_eq!(
            epoch_of(&crate::shim::logbook::rfc3339(1_700_000_000, 500_000_000)),
            Some(1_700_000_000.5)
        );
        assert_eq!(epoch_of("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(epoch_of("2024-02-29T12:00:00Z"), Some(1_709_208_000.0));
        assert_eq!(epoch_of("nonsense"), None);
    }
}
