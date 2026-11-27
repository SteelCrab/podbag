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

pub use crate::client::time::{epoch_of, human_duration};

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
            network_inode: None,
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
