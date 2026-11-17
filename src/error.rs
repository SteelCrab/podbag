//! Errors returned by openpod.
//!
//! Every error says what failed, why, and what to do next.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The class of an error. Callers use it to decide how to react.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The feature does not work on this operating system or kernel.
    Unsupported,
    /// The caller passed a value openpod cannot use.
    InvalidInput,
    /// A program, file or setting that openpod needs is missing.
    MissingRequirement,
    /// A container, image or path does not exist.
    NotFound,
    /// The command to run does not exist in the container.
    CommandNotFound,
    /// The command exists but cannot be executed.
    CommandNotExecutable,
    /// An operating system call failed.
    Runtime,
    /// A bug or an unexpected state in openpod.
    Internal,
}

/// An openpod error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub kind: ErrorKind,
    /// What failed.
    pub what: String,
    /// Why it failed.
    pub why: String,
    /// What to do next.
    pub next: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(
        kind: ErrorKind,
        what: impl Into<String>,
        why: impl Into<String>,
        next: impl Into<String>,
    ) -> Error {
        Error {
            kind,
            what: what.into(),
            why: why.into(),
            next: next.into(),
        }
    }

    /// An operating system error, with the OS message as the cause.
    pub fn io(what: impl Into<String>, err: &std::io::Error, next: impl Into<String>) -> Error {
        Error::new(ErrorKind::Runtime, what, err.to_string(), next)
    }

    /// Containers run only on Linux for now.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn unsupported_os() -> Error {
        Error::new(
            ErrorKind::Unsupported,
            "cannot run containers on this operating system",
            format!(
                "openpod runs containers only on Linux; this is {}",
                std::env::consts::OS
            ),
            "run openpod on Linux",
        )
    }

    /// The process exit code for this error. It follows `docker run`.
    pub fn exit_code(&self) -> i32 {
        match self.kind {
            ErrorKind::CommandNotFound => 127,
            ErrorKind::CommandNotExecutable => 126,
            _ => 125,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}\n  cause: {}\n  fix:   {}",
            self.what, self.why, self.next
        )
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Error {
        Error::new(
            ErrorKind::MissingRequirement,
            "cannot map user IDs for the container",
            "newuidmap was not found in PATH",
            "install the uidmap package",
        )
    }

    #[test]
    fn display_has_three_parts() {
        assert_eq!(
            sample().to_string(),
            "cannot map user IDs for the container\n  cause: newuidmap was not found in PATH\n  fix:   install the uidmap package"
        );
    }

    #[test]
    fn exit_codes_follow_docker() {
        let with = |kind| Error::new(kind, "w", "y", "n").exit_code();
        assert_eq!(with(ErrorKind::CommandNotFound), 127);
        assert_eq!(with(ErrorKind::CommandNotExecutable), 126);
        for kind in [
            ErrorKind::Unsupported,
            ErrorKind::InvalidInput,
            ErrorKind::MissingRequirement,
            ErrorKind::NotFound,
            ErrorKind::Runtime,
            ErrorKind::Internal,
        ] {
            assert_eq!(with(kind), 125);
        }
    }

    #[test]
    fn json_round_trip_keeps_every_field() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(json.contains(r#""kind":"missing_requirement""#));
        assert_eq!(serde_json::from_str::<Error>(&json).unwrap(), sample());
    }

    #[test]
    fn io_error_becomes_runtime_with_os_text() {
        let os = std::io::Error::from_raw_os_error(13);
        let e = Error::io("cannot mount /proc", &os, "check the mount point");
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!(e.why, os.to_string());
    }
}
