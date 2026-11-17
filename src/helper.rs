//! How the library starts the openpod helper process and learns whether it started.

//!
//! The helper is the `openpod` program run with a hidden subcommand. It gets the write end
//! of a pipe as file descriptor 3, writes one JSON line there, and closes it.

use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader};
#[cfg(not(target_os = "linux"))]
use std::os::fd::FromRawFd;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// The file descriptor on which the helper reports its status.
pub const STATUS_FD: i32 = 3;

const HELPER_NAME: &str = "openpod";

/// The one line a helper writes on [`STATUS_FD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StatusMessage {
    Started { pid: u32 },
    Failed { error: Error },
}

/// Find the helper program for the current process.
pub fn locate_helper() -> Result<PathBuf> {
    locate_helper_with(
        std::env::var("OPENPOD_BIN").ok(),
        std::env::current_exe().ok(),
        std::env::var("PATH").ok(),
    )
}

/// Find the helper: `OPENPOD_BIN`, the current program if it is openpod, an openpod next to
/// the current program, then `PATH`.
pub fn locate_helper_with(
    env_bin: Option<String>,
    current_exe: Option<PathBuf>,
    path_var: Option<String>,
) -> Result<PathBuf> {
    let not_found = |why: String| {
        Error::new(
            ErrorKind::MissingRequirement,
            "cannot find the openpod helper program",
            why,
            "install openpod in PATH, or set OPENPOD_BIN to the path of the openpod program",
        )
    };

    if let Some(bin) = env_bin.filter(|b| !b.is_empty()) {
        let bin = PathBuf::from(bin);
        return if bin.is_file() {
            Ok(bin)
        } else {
            Err(not_found(format!(
                "OPENPOD_BIN is set to {}, which does not exist",
                bin.display()
            )))
        };
    }

    if let Some(exe) = &current_exe {
        if exe.file_name().is_some_and(|n| n == HELPER_NAME) && exe.is_file() {
            return Ok(exe.clone());
        }
        if let Some(sibling) = exe
            .parent()
            .map(|d| d.join(HELPER_NAME))
            .filter(|p| p.is_file())
        {
            return Ok(sibling);
        }
    }

    if let Some(found) = path_var
        .iter()
        .flat_map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .map(|dir| dir.join(HELPER_NAME))
        .find(|p| p.is_file())
    {
        return Ok(found);
    }

    Err(not_found(
        "openpod is not next to this program and not in PATH".to_string(),
    ))
}

/// A started helper process and the read end of its status pipe.
pub struct HelperChild {
    pub child: Child,
    status: Option<BufReader<File>>,
}

/// Start `<helper> <args...>` with the status pipe on fd 3.
pub fn spawn_with_args(
    args: &[&str],
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<HelperChild> {
    let helper = locate_helper()?;
    let fail = |e: std::io::Error| {
        Error::io(
            "cannot start the openpod helper",
            &e,
            format!("check that {} can be executed", helper.display()),
        )
    };

    let (read_end, write_end) = cloexec_pipe().map_err(fail)?;
    let write_fd = write_end.as_raw_fd();

    let mut cmd = Command::new(&helper);
    cmd.args(args).stdin(stdin).stdout(stdout).stderr(stderr);
    // SAFETY: the closure only calls sigemptyset, sigprocmask, dup2 and fcntl,
    // which are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            // The helper must not inherit signals the caller blocked.
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            if write_fd == STATUS_FD {
                // dup2 onto itself keeps FD_CLOEXEC, so clear it.
                if libc::fcntl(STATUS_FD, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(write_fd, STATUS_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(fail)?;
    drop(write_end); // the helper holds the only write end now

    Ok(HelperChild {
        child,
        status: Some(BufReader::new(File::from(read_end))),
    })
}

/// Start `<helper> __shim <id>`.
pub fn spawn_shim(id: &str, stdin: Stdio, stdout: Stdio, stderr: Stdio) -> Result<HelperChild> {
    spawn_with_args(&["__shim", id], stdin, stdout, stderr)
}

impl HelperChild {
    /// Wait for the helper's status line. Returns the PID it reports.
    pub fn wait_started(&mut self) -> Result<u32> {
        let mut line = String::new();
        let read = match self.status.take() {
            Some(mut r) => r.read_line(&mut line),
            None => Ok(0),
        };

        let message = match read {
            Ok(n) if n > 0 => {
                serde_json::from_str::<StatusMessage>(line.trim_end()).map_err(|e| {
                    Error::new(
                        ErrorKind::Internal,
                        "the openpod helper sent an unreadable status",
                        format!("{e}: {line:?}"),
                        "report this bug",
                    )
                })?
            }
            _ => {
                let status = self
                    .child
                    .wait()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|e| e.to_string());
                return Err(Error::new(
                    ErrorKind::Internal,
                    "the openpod helper exited without reporting a status",
                    format!("helper {status}"),
                    "run the same command again; if it keeps failing, report this bug",
                ));
            }
        };

        match message {
            StatusMessage::Started { pid } => Ok(pid),
            StatusMessage::Failed { error } => {
                let _ = self.child.wait();
                Err(error)
            }
        }
    }
}

/// A pipe whose ends are close-on-exec from the start. On Linux this is atomic, so a
/// helper started by another thread at the same moment cannot inherit it.
#[cfg(target_os = "linux")]
fn cloexec_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(Into::into)
}

/// macOS has no pipe2. The helper never creates containers there, so the short window
/// before FD_CLOEXEC is set does not matter.
#[cfg(not(target_os = "linux"))]
fn cloexec_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: fds is a valid array of two ints.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe() returned two new descriptors that nothing else owns.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&r, &w] {
        // SAFETY: fd is open for the lifetime of this call.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((r, w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, ErrorKind};
    use std::path::PathBuf;

    #[test]
    fn status_messages_have_a_stable_wire_format() {
        assert_eq!(
            serde_json::to_string(&StatusMessage::Started { pid: 42 }).unwrap(),
            r#"{"status":"started","pid":42}"#
        );
        let failed = StatusMessage::Failed {
            error: Error::new(ErrorKind::NotFound, "w", "y", "n"),
        };
        let json = serde_json::to_string(&failed).unwrap();
        assert!(json.starts_with(r#"{"status":"failed","error":{"#));
        assert_eq!(
            serde_json::from_str::<StatusMessage>(&json).unwrap(),
            failed
        );
    }

    fn touch(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"").unwrap();
        p
    }

    #[test]
    fn locate_prefers_the_environment_variable() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = touch(tmp.path(), "custom-openpod");
        let found = locate_helper_with(
            Some(bin.display().to_string()),
            Some(touch(tmp.path(), "openpod")),
            None,
        )
        .unwrap();
        assert_eq!(found, bin);
    }

    #[test]
    fn locate_fails_when_the_environment_variable_points_nowhere() {
        let tmp = tempfile::tempdir().unwrap();
        let e = locate_helper_with(
            Some("/no/such/openpod".into()),
            Some(touch(tmp.path(), "openpod")),
            None,
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::MissingRequirement);
    }

    #[test]
    fn locate_uses_the_current_executable_when_it_is_openpod() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = touch(tmp.path(), "openpod");
        assert_eq!(
            locate_helper_with(None, Some(exe.clone()), None).unwrap(),
            exe
        );
    }

    #[test]
    fn locate_finds_a_sibling_then_path() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let app = touch(a.path(), "my-app");
        let in_path = touch(b.path(), "openpod");
        assert_eq!(
            locate_helper_with(
                None,
                Some(app.clone()),
                Some(b.path().display().to_string())
            )
            .unwrap(),
            in_path
        );
        let sibling = touch(a.path(), "openpod");
        assert_eq!(
            locate_helper_with(None, Some(app), Some(b.path().display().to_string())).unwrap(),
            sibling
        );
    }

    #[test]
    fn locate_explains_how_to_fix_a_missing_helper() {
        let e = locate_helper_with(None, None, Some("/nonexistent".into())).unwrap_err();
        assert_eq!(e.kind, ErrorKind::MissingRequirement);
        assert_eq!(e.what, "cannot find the openpod helper program");
        assert!(e.next.contains("OPENPOD_BIN"));
    }
}
