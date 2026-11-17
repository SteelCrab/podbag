//! Existing containers: list, inspect, logs, stop, kill, wait, remove.

use crate::paths::{self, Dirs};
use crate::shim::control::{self, Request, Response};
use crate::shim::logbook;
pub use crate::shim::logbook::{LogFollower, LogLine, Stream};
use crate::state::{self, State, Status};
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Settings for [`ContainerHandle::exec`]. Unset values come from the container.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOptions {
    /// Added to (and overriding) the container's environment.
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub tty: bool,
    pub interactive: bool,
}

/// What `openpod inspect` and `openpod ps` show about a container.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    /// The image name, or the root filesystem directory for `--rootfs` containers.
    pub image: String,
    pub command: Vec<String>,
    pub state: State,
}

/// A container that exists (running or not).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ContainerHandle {
    id: String,
}

impl ContainerHandle {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn from_id(id: String) -> ContainerHandle {
        ContainerHandle { id }
    }

    /// Find a container by name, full ID, or ID prefix (4+ characters).
    pub fn get(name_or_id: &str) -> Result<ContainerHandle> {
        let dirs = paths::dirs()?;
        Ok(ContainerHandle {
            id: state::resolve(&dirs, name_or_id)?,
        })
    }

    /// Containers, newest first. Without `all`, only running ones.
    pub fn list(all: bool) -> Result<Vec<ContainerInfo>> {
        let dirs = paths::dirs()?;
        let mut out = Vec::new();
        for id in state::list_ids(&dirs)? {
            if let Ok(info) = info_of(&dirs, &id) {
                if all || info.state.status == Status::Running {
                    out.push(info);
                }
            }
        }
        out.sort_by(|a, b| b.state.created.cmp(&a.state.created).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn info(&self) -> Result<ContainerInfo> {
        info_of(&paths::dirs()?, &self.id)
    }

    /// Every stored output line, oldest first.
    pub fn logs(&self) -> Result<Vec<LogLine>> {
        logbook::read_logs(&paths::dirs()?.container_dir(&self.id))
    }

    /// Read output as it is written: the first [`LogFollower::next_lines`] returns every
    /// stored line, later calls only new ones. Unlike polling [`ContainerHandle::logs`],
    /// it continues correctly when the log file is rotated.
    pub fn follow_logs(&self) -> Result<LogFollower> {
        Ok(LogFollower::new(&paths::dirs()?.container_dir(&self.id)))
    }

    /// Send `signal` to the container's main process.
    pub fn kill(&self, signal: i32) -> Result<()> {
        self.kill_in(&paths::dirs()?, signal)
    }

    fn kill_in(&self, dirs: &Dirs, signal: i32) -> Result<()> {
        match control::send(
            dirs,
            &self.id,
            &Request::Kill { signal },
            Some(Duration::from_secs(10)),
        )? {
            Response::Failed { error } => Err(error),
            _ => Ok(()),
        }
    }

    /// Wait for the container to end. Returns its exit code.
    pub fn wait(&self) -> Result<i32> {
        self.wait_in(&paths::dirs()?)
    }

    fn wait_in(&self, dirs: &Dirs) -> Result<i32> {
        match control::send(dirs, &self.id, &Request::Wait, None) {
            Ok(Response::Exited { exit_code }) => Ok(exit_code),
            Ok(Response::Failed { error }) => Err(error),
            Ok(Response::Ok { .. }) | Err(_) => self.recorded_exit(dirs),
        }
    }

    /// SIGTERM, then SIGKILL if the container still runs after `timeout`.
    /// Returns its exit code. Stopping a container that has ended just returns its code.
    pub fn stop(&self, timeout: Duration) -> Result<i32> {
        self.stop_in(&paths::dirs()?, timeout)
    }

    fn stop_in(&self, dirs: &Dirs, timeout: Duration) -> Result<i32> {
        if self.kill_in(dirs, libc::SIGTERM).is_err() {
            // Already ended, or ending: the shim (if still there) or the state has the code.
            return self.wait_in(dirs);
        }
        match control::send(dirs, &self.id, &Request::Wait, Some(timeout)) {
            Ok(Response::Exited { exit_code }) => Ok(exit_code),
            Ok(_) | Err(_) => {
                let _ = self.kill_in(dirs, libc::SIGKILL);
                self.wait_in(dirs)
            }
        }
    }

    /// Run another command in the running container and collect its output.
    pub fn exec<S: AsRef<str>>(
        &self,
        command: &[S],
        options: &ExecOptions,
    ) -> Result<crate::Output> {
        #[cfg(target_os = "linux")]
        {
            use std::process::Stdio;
            let mut opts = options.clone();
            opts.interactive = false;
            let mut helper = self.spawn_exec(
                command,
                &opts,
                Stdio::null(),
                Stdio::piped(),
                Stdio::piped(),
            )?;
            crate::container::collect_output(&mut helper)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (command, options);
            Err(Error::unsupported_os())
        }
    }

    /// Like [`ContainerHandle::exec`] with this process's stdin, stdout and stderr.
    /// Used by the CLI, which forwards signals to the helper.
    #[doc(hidden)]
    pub fn exec_inherit<S: AsRef<str>>(
        &self,
        command: &[S],
        options: &ExecOptions,
    ) -> Result<crate::Running> {
        #[cfg(target_os = "linux")]
        {
            use std::process::Stdio;
            let helper = self.spawn_exec(
                command,
                options,
                Stdio::inherit(),
                Stdio::inherit(),
                Stdio::inherit(),
            )?;
            Ok(crate::Running::for_exec(helper))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (command, options);
            Err(Error::unsupported_os())
        }
    }

    #[cfg(target_os = "linux")]
    fn spawn_exec<S: AsRef<str>>(
        &self,
        command: &[S],
        options: &ExecOptions,
        stdin: std::process::Stdio,
        stdout: std::process::Stdio,
        stderr: std::process::Stdio,
    ) -> Result<crate::helper::HelperChild> {
        let info = self.info()?;
        if info.state.status != Status::Running {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("container {} is not running", info.name),
                "exec needs a running container",
                "start a new container, or run `openpod ps -a` to see its state",
            ));
        }
        let req = crate::linux::exec::ExecRequest {
            command: command.iter().map(|c| c.as_ref().to_string()).collect(),
            options: options.clone(),
        };
        let json = serde_json::to_string(&req).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                "cannot encode an exec request",
                e.to_string(),
                "report this bug",
            )
        })?;
        // Rootless: a plain session may not move a process into the container's cgroup;
        // a process in a scope of the same slice may (design: exec in the same cgroup).
        let scope = match (
            &state::read_config(&paths::dirs()?, &self.id)?.cgroup,
            &info.state.cgroup,
        ) {
            (crate::cgroup::Placement::Scope { slice }, Some(_)) => Some(crate::helper::Scope {
                systemd_run: crate::cgroup::systemd_run().ok_or_else(|| {
                    Error::new(
                        ErrorKind::MissingRequirement,
                        "cannot run the command in the container's cgroup",
                        "systemd-run was not found",
                        "install systemd, or set OPENPOD_SYSTEMD_RUN",
                    )
                })?,
                slice: slice.clone(),
                unit: None,
                delegate: false,
            }),
            _ => None,
        };
        let mut helper = crate::helper::spawn_with_args(
            &["__exec", &self.id, &json],
            stdin,
            stdout,
            stderr,
            scope.as_ref(),
        )?;
        helper.wait_started()?;
        Ok(helper)
    }

    /// Delete the container. A running container is refused unless `force`, which kills it.
    pub fn remove(&self, force: bool) -> Result<()> {
        let dirs = paths::dirs()?;
        let info = info_of(&dirs, &self.id)?;
        if info.state.status == Status::Running {
            if !force {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("cannot remove container {}", info.name),
                    "it is running",
                    "stop it first, or use `openpod rm -f`",
                ));
            }
            let _ = self.kill(libc::SIGKILL);
            let _ = self.wait();
        }
        // The last chance to find the container's cgroup: its record goes away now.
        #[cfg(target_os = "linux")]
        if let Some(path) = state::read_state(&dirs, &self.id)?.cgroup {
            crate::cgroup::remove_dir(std::path::Path::new(&path));
        }
        if state::remove_container_dir(&dirs, &self.id).is_ok() {
            return Ok(());
        }
        // Files the container's users created (subordinate IDs when rootless).
        remove_with_helper(&self.id)
    }

    fn recorded_exit(&self, dirs: &Dirs) -> Result<i32> {
        let st = state::reconcile(dirs, &self.id)?;
        st.exit_code.ok_or_else(|| {
            Error::new(
                ErrorKind::Runtime,
                format!(
                    "the exit code of container {} is unknown",
                    crate::id::short_id(&self.id)
                ),
                "its shim ended unexpectedly",
                "check `openpod logs` for the last output",
            )
        })
    }
}

fn info_of(dirs: &Dirs, id: &str) -> Result<ContainerInfo> {
    let spec = state::read_config(dirs, id)?;
    let st = state::reconcile(dirs, id)?;
    let image = match (&spec.image_name, &spec.rootfs) {
        (Some(name), _) => name.clone(),
        (None, crate::spec::Rootfs::Dir { path }) => path.clone(),
        (None, crate::spec::Rootfs::Overlay { lower, .. }) => lower.clone(),
    };
    Ok(ContainerInfo {
        id: id.to_string(),
        name: spec.name,
        image,
        command: spec.command,
        state: st,
    })
}

#[cfg(unix)]
fn remove_with_helper(id: &str) -> Result<()> {
    crate::helper::run_store_op(
        &crate::store::ops::StoreOp::RemoveContainer { id: id.to_string() },
        false,
    )
    .map(drop)
}

#[cfg(not(unix))]
fn remove_with_helper(_id: &str) -> Result<()> {
    Err(Error::unsupported_os())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A shim whose container has just exited: kill is refused, wait gives the code.
    fn exiting_shim(dirs: &Dirs, id: &str, code: i32) -> std::thread::JoinHandle<()> {
        let listener = control::bind(dirs, id).unwrap();
        listener.set_nonblocking(false).unwrap();
        let id = id.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(3) {
                let Ok(stream) = stream else { return };
                let response = match control::read_request(&stream) {
                    Some(Request::Kill { .. }) => Response::Failed {
                        error: control::not_running(&id),
                    },
                    Some(Request::Wait) => Response::Exited { exit_code: code },
                    None => return,
                };
                control::reply(&stream, &response);
                if matches!(response, Response::Exited { .. }) {
                    return;
                }
            }
        })
    }

    // The shim refuses kill while it reads the last output of a container that has just
    // exited, and its state file still says "running" then.
    #[test]
    fn stop_while_the_container_exits_returns_its_code() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            data: tmp.path().join("data"),
            runtime: tmp.path().join("run"),
            config: tmp.path().join("cfg"),
        };
        let id = "c".repeat(64);
        std::fs::create_dir_all(dirs.container_dir(&id)).unwrap();
        let running = State {
            status: Status::Running,
            pid: Some(1),
            shim_pid: Some(1),
            exit_code: None,
            created: 1,
            started: Some(1),
            finished: None,
            cgroup: None,
            oom: None,
        };
        state::write_state(&dirs, &id, &running).unwrap();
        let _shim_alive = state::hold_shim_lock(&dirs, &id).unwrap();
        let shim = exiting_shim(&dirs, &id, 3);
        let handle = ContainerHandle::from_id(id);
        assert_eq!(handle.stop_in(&dirs, Duration::from_secs(1)).unwrap(), 3);
        shim.join().unwrap();
    }
}
