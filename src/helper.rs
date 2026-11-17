//! How the library starts the openpod helper process and learns whether it started.

//!
//! The helper is the `openpod` program run with a hidden subcommand. It gets the write end
//! of a pipe as file descriptor 3, writes one JSON line there, and closes it.

use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::FromRawFd;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The file descriptor on which the helper reports its status.
pub const STATUS_FD: i32 = 3;

const HELPER_NAME: &str = "openpod";

/// The one line a helper writes on [`STATUS_FD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StatusMessage {
    Started {
        pid: u32,
    },
    Failed {
        error: Error,
    },
    /// A helper that does a job (not a container) reports its result.
    Done {
        output: serde_json::Value,
    },
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
    /// The systemd-run program the helper was started through, if any.
    via: Option<PathBuf>,
}

/// Run a helper inside a new transient systemd scope of the user's service manager
/// (rootless cgroups). systemd-run registers the scope, then executes the helper in place:
/// same PID, same descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub systemd_run: PathBuf,
    /// The slice that holds the scope, such as `openpod.slice`.
    pub slice: String,
    /// The scope's unit name; systemd picks one when `None`.
    pub unit: Option<String>,
    /// Let the helper manage the cgroups below its scope.
    pub delegate: bool,
}

/// The program and arguments that run `<helper> <args...>` in `scope`.
pub fn scope_command(scope: &Scope, helper: &Path, args: &[&str]) -> (PathBuf, Vec<String>) {
    let mut argv: Vec<String> = ["--user", "--scope", "--quiet", "--collect"]
        .into_iter()
        .map(String::from)
        .collect();
    if scope.delegate {
        argv.extend(["-p".to_string(), "Delegate=yes".to_string()]);
    }
    argv.push(format!("--slice={}", scope.slice));
    if let Some(unit) = &scope.unit {
        argv.push(format!("--unit={unit}"));
    }
    argv.push("--".to_string());
    argv.push(helper.to_string_lossy().into_owned());
    argv.extend(args.iter().map(|a| a.to_string()));
    (scope.systemd_run.clone(), argv)
}

/// Start `<helper> <args...>` with the status pipe on fd 3, in `scope` when given.
pub fn spawn_with_args(
    args: &[&str],
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
    scope: Option<&Scope>,
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

    let mut cmd = match scope {
        Some(s) => {
            let (program, argv) = scope_command(s, &helper, args);
            let mut cmd = Command::new(program);
            cmd.args(argv);
            cmd
        }
        None => {
            let mut cmd = Command::new(&helper);
            cmd.args(args);
            cmd
        }
    };
    cmd.stdin(stdin).stdout(stdout).stderr(stderr);
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
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => match scope {
            Some(s) => return Err(scope_error(&s.systemd_run, e.to_string())),
            None => return Err(fail(e)),
        },
    };
    drop(write_end); // the helper holds the only write end now

    Ok(HelperChild {
        child,
        status: Some(BufReader::new(File::from(read_end))),
        via: scope.map(|s| s.systemd_run.clone()),
    })
}

/// Start `<helper> __shim <id>`, in `scope` when given.
pub fn spawn_shim(
    id: &str,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
    scope: Option<&Scope>,
) -> Result<HelperChild> {
    spawn_with_args(&["__shim", id], stdin, stdout, stderr, scope)
}

fn scope_error(systemd_run: &Path, why: String) -> Error {
    Error::new(
        ErrorKind::MissingRequirement,
        "cannot start the container in a systemd scope",
        format!("{}: {why}", systemd_run.display()),
        "check `systemctl --user status`; rootless resource limits need a systemd user session",
    )
}

impl HelperChild {
    /// Wait for a container helper's status line. Returns the PID it reports.
    pub fn wait_started(&mut self) -> Result<u32> {
        match self.read_status()? {
            StatusMessage::Started { pid } => Ok(pid),
            StatusMessage::Failed { error } => {
                let _ = self.child.wait();
                Err(error)
            }
            StatusMessage::Done { .. } => Err(unexpected_status()),
        }
    }

    /// Wait for a job helper to finish. Returns its output.
    pub fn wait_done(&mut self) -> Result<serde_json::Value> {
        let message = self.read_status()?;
        let _ = self.child.wait();
        match message {
            StatusMessage::Done { output } => Ok(output),
            StatusMessage::Failed { error } => Err(error),
            StatusMessage::Started { .. } => Err(unexpected_status()),
        }
    }

    fn read_status(&mut self) -> Result<StatusMessage> {
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
                if let Some(systemd_run) = &self.via {
                    // systemd-run ends before it executes the helper when it cannot
                    // create the scope.
                    return Err(scope_error(systemd_run, status));
                }
                return Err(Error::new(
                    ErrorKind::Internal,
                    "the openpod helper exited without reporting a status",
                    format!("helper {status}"),
                    "run the same command again; if it keeps failing, report this bug",
                ));
            }
        };
        Ok(message)
    }
}

fn unexpected_status() -> Error {
    Error::new(
        ErrorKind::Internal,
        "the openpod helper sent an unexpected status",
        "a container helper reported a job result, or the other way round",
        "report this bug",
    )
}

/// Run a store job in `openpod __store '<json>'` and return its output.
/// With `show_progress`, the helper writes progress lines to this process's stderr.
pub fn run_store_op(
    op: &crate::store::ops::StoreOp,
    show_progress: bool,
) -> Result<serde_json::Value> {
    let json = serde_json::to_string(op).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "cannot encode a store job",
            e.to_string(),
            "report this bug",
        )
    })?;
    let stderr = if show_progress {
        Stdio::inherit()
    } else {
        Stdio::null()
    };
    spawn_with_args(
        &["__store", &json],
        Stdio::null(),
        Stdio::null(),
        stderr,
        None,
    )?
    .wait_done()
}

/// The status descriptor a helper received from its caller.
pub struct StatusChannel(Option<std::fs::File>);

impl StatusChannel {
    /// Take fd 3 if the caller passed it, and mark it close-on-exec so children never inherit it.
    pub fn take() -> StatusChannel {
        // SAFETY: fcntl on any integer is safe; it fails with EBADF when fd 3 is not open.
        let open = unsafe { libc::fcntl(STATUS_FD, libc::F_GETFD) } != -1;
        if !open {
            return StatusChannel(None);
        }
        // SAFETY: fd 3 is open and was given to this process for this purpose only.
        unsafe { libc::fcntl(STATUS_FD, libc::F_SETFD, libc::FD_CLOEXEC) };
        // SAFETY: as above; nothing else in this process uses fd 3.
        StatusChannel(Some(unsafe { std::fs::File::from_raw_fd(STATUS_FD) }))
    }

    /// Write the status line and close the descriptor. Later calls do nothing.
    pub fn report(&mut self, message: &StatusMessage) {
        use std::io::Write;
        if let Some(mut f) = self.0.take() {
            if let Ok(mut line) = serde_json::to_string(message) {
                line.push('\n');
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    /// Report a failure and return the matching exit code.
    pub fn fail(&mut self, error: Error) -> i32 {
        let code = error.exit_code();
        self.report(&StatusMessage::Failed { error });
        code
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

#[cfg(test)]
mod done_tests {
    use super::*;

    #[test]
    fn scope_command_line() {
        let s = Scope {
            systemd_run: "/usr/bin/systemd-run".into(),
            slice: "openpod.slice".into(),
            unit: Some("openpod-abc".into()),
            delegate: true,
        };
        let (prog, args) = scope_command(&s, Path::new("/bin/openpod"), &["__shim", "abc"]);
        assert_eq!(prog, std::path::PathBuf::from("/usr/bin/systemd-run"));
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "-p",
                "Delegate=yes",
                "--slice=openpod.slice",
                "--unit=openpod-abc",
                "--",
                "/bin/openpod",
                "__shim",
                "abc"
            ]
        );
        let plain = Scope {
            unit: None,
            delegate: false,
            ..s
        };
        let (_, args) = scope_command(&plain, Path::new("/bin/openpod"), &["__exec"]);
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=openpod.slice",
                "--",
                "/bin/openpod",
                "__exec"
            ]
        );
    }

    #[test]
    fn done_message_wire_format() {
        let m = StatusMessage::Done {
            output: serde_json::json!({"a": 1}),
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"status":"done","output":{"a":1}}"#
        );
    }
}
