//! Existing containers: list, inspect, logs, stop, kill, wait, remove.

use crate::paths::{self, Dirs};
use crate::shim::control::{self, Request, Response};
use crate::shim::logbook;
pub use crate::shim::logbook::{LogFollower, LogLine, Stream};
use crate::state::{self, State, Status};
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub(crate) mod pause;
mod rename;
pub mod stats;
pub mod top;
pub mod update;

/// Settings for [`ContainerHandle::exec`]. Unset values come from the container.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOptions {
    /// Added to (and overriding) the container's environment.
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub tty: bool,
    pub interactive: bool,
    /// The terminal size (rows, columns) with `tty`; None: copied from the caller's
    /// terminal, if it has one.
    #[serde(default)]
    pub size: Option<(u16, u16)>,
}

/// What `podbag inspect` and `podbag ps` show about a container.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    /// The image name, or the root filesystem directory for `--rootfs` containers.
    pub image: String,
    pub command: Vec<String>,
    pub state: State,
    /// Volumes, host paths and tmpfs mounts.
    #[serde(default)]
    pub mounts: Vec<MountInfo>,
    /// How many times its restart policy started it again since the user last started it.
    #[serde(default)]
    pub restart_count: u32,
    /// Its health since its last start, when it has a check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<crate::health::Health>,
}

/// One mount of a container, as `inspect` shows it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MountInfo {
    /// `bind`, `volume` or `tmpfs`.
    pub kind: String,
    /// The host path, the volume name, or `tmpfs`.
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

impl From<&crate::volume::Mount> for MountInfo {
    fn from(m: &crate::volume::Mount) -> Self {
        use crate::volume::MountSource;
        let (kind, source) = match &m.source {
            MountSource::Bind { path, .. } => ("bind", path.clone()),
            MountSource::Volume { name, .. }
            | MountSource::Nfs { name, .. }
            | MountSource::Driver { name, .. } => ("volume", name.clone()),
            MountSource::Tmpfs { .. } => ("tmpfs", "tmpfs".to_string()),
        };
        let read_only = m.read_only
            || matches!(&m.source, MountSource::Nfs { options, .. } if options.read_only())
            || matches!(&m.source, MountSource::Tmpfs { options } if options.split(',').any(|o| o == "ro"));
        MountInfo {
            kind: kind.to_string(),
            source,
            target: m.target.clone(),
            read_only,
        }
    }
}

/// A tar of a container's files ([`ContainerHandle::copy_out`]), read as it is made. Its
/// end is an error when the copy failed partway: the tar alone could look complete.
pub struct CopyOut {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    helper: crate::helper::HelperChild,
    tar: Option<Box<dyn std::io::Read + Send>>,
    /// The helper was waited for.
    ended: bool,
}

impl std::io::Read for CopyOut {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(tar) = &mut self.tar else {
            return Ok(0);
        };
        let n = tar.read(buf)?;
        if n == 0 && !buf.is_empty() {
            self.tar = None;
            self.ended = true;
            let code = self.helper.wait()?;
            if code != 0 {
                return Err(std::io::Error::other(format!(
                    "the copy failed partway (exit code {code}); the reason is in the log of \
                     the process that ran it"
                )));
            }
        }
        Ok(n)
    }
}

impl Drop for CopyOut {
    fn drop(&mut self) {
        // Closing the pipe ends a helper still writing; then it is reaped.
        self.tar = None;
        if !self.ended {
            let _ = self.helper.wait();
        }
    }
}

#[cfg(target_os = "linux")]
fn decode_stat(output: serde_json::Value) -> Result<crate::archive::PathStat> {
    serde_json::from_value(output).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "the archive helper sent unreadable details",
            e.to_string(),
            "report this bug",
        )
    })
}

/// A container that exists (running or not).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ContainerHandle {
    id: String,
}

/// See [`ContainerHandle::lookup`].
pub struct Lookup(state::Lookup);

impl Lookup {
    /// As [`ContainerHandle::get`]; a container removed since the first call is not found.
    pub fn get(&mut self, name_or_id: &str) -> Result<ContainerHandle> {
        Ok(ContainerHandle {
            id: self.0.resolve(name_or_id)?,
        })
    }
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

    /// For a command that names several containers: finds them one after another with
    /// one look at the container list (a `get` for each reads every container again).
    pub fn lookup() -> Result<Lookup> {
        Ok(Lookup(state::Lookup::new(&paths::dirs()?)))
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
        // Before the signal: whoever cleans up after the end must see it.
        crate::restart::mark_stopped(dirs, &self.id);
        match control::send(
            dirs,
            &self.id,
            &Request::Kill { signal },
            Some(Duration::from_secs(10)),
        )? {
            Response::Failed { error } => Err(error),
            _ => {
                // A paused container handles the signal once it runs again.
                pause::thaw_after_signal(dirs, &self.id);
                Ok(())
            }
        }
    }

    /// Wait for the container to end. Returns its exit code.
    pub fn wait(&self) -> Result<i32> {
        self.wait_in(&paths::dirs()?)
    }

    /// Like [`ContainerHandle::wait`] for at most `limit`: None if it still runs then.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn wait_up_to(&self, limit: Duration) -> Result<Option<i32>> {
        let dirs = paths::dirs()?;
        match control::send(&dirs, &self.id, &Request::Wait, Some(limit)) {
            Ok(Response::Exited { exit_code }) => Ok(Some(exit_code)),
            Ok(Response::Failed { error }) => Err(error),
            // The request timed out: it runs on.
            Err(e) if e.kind == ErrorKind::Runtime => Ok(None),
            Ok(Response::Ok { .. }) | Err(_) => self.recorded_exit(&dirs).map(Some),
        }
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
        self.stop_in(&paths::dirs()?, Some(timeout))
    }

    /// Stop using an override in seconds, or the container's configured grace period.
    pub fn stop_configured(&self, seconds: Option<i64>) -> Result<i32> {
        let dirs = paths::dirs()?;
        self.stop_in(&dirs, self.stop_timeout(&dirs, seconds)?)
    }

    fn stop_timeout(&self, dirs: &Dirs, seconds: Option<i64>) -> Result<Option<Duration>> {
        let seconds = match seconds {
            Some(value) => value,
            None => state::read_config(dirs, &self.id)?
                .stop_timeout
                .unwrap_or(10),
        };
        Ok(u64::try_from(seconds).ok().map(Duration::from_secs))
    }

    fn stop_in(&self, dirs: &Dirs, timeout: Option<Duration>) -> Result<i32> {
        if self.kill_in(dirs, libc::SIGTERM).is_err() && !self.signal_a_start(dirs, libc::SIGTERM) {
            // Already ended, or ending: the shim (if still there) or the state has the code.
            return self.wait_in(dirs);
        }
        if timeout.is_none() {
            return self.wait_in(dirs);
        }
        if timeout != Some(Duration::ZERO)
            && let Ok(Response::Exited { exit_code }) =
                control::send(dirs, &self.id, &Request::Wait, timeout)
        {
            return Ok(exit_code);
        }
        let _ = self.kill_in(dirs, libc::SIGKILL);
        self.wait_in(dirs)
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
                None,
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
                None,
            )?;
            Ok(crate::Running::for_exec(helper))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (command, options);
            Err(Error::unsupported_os())
        }
    }

    /// Start a created or exited container in the background (`docker start`). An exited
    /// one keeps its writable layer, log and volumes. A failed start keeps the container.
    pub fn start(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use crate::helper::Io;
            let dirs = paths::dirs()?;
            self.refuse_running()?;
            crate::restart::user_started(&dirs, &self.id);
            let mut helper = crate::container::start_shim(
                &dirs,
                &self.id,
                Io::Null,
                Io::Null,
                Io::Null,
                crate::helper::ShimMode::Detached,
            )?;
            helper.release_detached();
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::unsupported_os())
        }
    }

    /// Start the container attached to these descriptors (`start -a`, the API's attach):
    /// output to `stdout` and `stderr` (a terminal's to `stdout`), `stdin` to the container
    /// when it was created interactive. A failed start keeps the container.
    #[doc(hidden)]
    pub fn start_attached(
        &self,
        stdin: crate::helper::Io,
        stdout: crate::helper::Io,
        stderr: crate::helper::Io,
    ) -> Result<crate::Running> {
        #[cfg(target_os = "linux")]
        {
            let dirs = paths::dirs()?;
            self.refuse_running()?;
            crate::restart::user_started(&dirs, &self.id);
            let helper = crate::container::start_shim(
                &dirs,
                &self.id,
                stdin,
                stdout,
                stderr,
                crate::helper::ShimMode::Attached,
            )?;
            Ok(crate::Running::for_exec(helper))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (stdin, stdout, stderr);
            Err(Error::unsupported_os())
        }
    }

    #[cfg(target_os = "linux")]
    fn refuse_running(&self) -> Result<()> {
        let info = self.info()?;
        if info.state.paused {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("cannot start container {}", info.name),
                "it is paused",
                "run `podbag unpause` instead",
            ));
        }
        if info.state.status == Status::Running {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("cannot start container {}", info.name),
                "it is already running",
                "stop it first, or use `podbag restart`",
            ));
        }
        Ok(())
    }

    /// Stop the container if it runs (like [`ContainerHandle::stop`]), then start it.
    pub fn restart(&self, timeout: Duration) -> Result<()> {
        self.restart_in(&paths::dirs()?, Some(timeout))
    }

    /// Restart using an override in seconds, or the container's configured grace period.
    pub fn restart_configured(&self, seconds: Option<i64>) -> Result<()> {
        let dirs = paths::dirs()?;
        self.restart_in(&dirs, self.stop_timeout(&dirs, seconds)?)
    }

    fn restart_in(&self, dirs: &Dirs, timeout: Option<Duration>) -> Result<()> {
        if self.info()?.state.status == Status::Running {
            self.stop_in(dirs, timeout)?;
        }
        self.start()
    }

    /// Set the size of the container's terminal (`docker resize`). Nothing to do without
    /// a terminal.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let dirs = paths::dirs()?;
        let response = control::send(
            &dirs,
            &self.id,
            &Request::Resize { rows, cols },
            Some(Duration::from_secs(5)),
        )
        .map_err(|e| Error {
            kind: ErrorKind::Conflict,
            ..e
        })?;
        match response {
            Response::Failed { error } => Err(Error {
                kind: ErrorKind::Conflict,
                ..error
            }),
            _ => Ok(()),
        }
    }

    /// Run a command in the running container with these descriptors (the API's exec).
    /// `resize`: the reading end of a pipe of resize records (rows, cols: two
    /// little-endian u16) for the command's terminal.
    #[doc(hidden)]
    pub fn exec_with<S: AsRef<str>>(
        &self,
        command: &[S],
        options: &ExecOptions,
        stdin: std::process::Stdio,
        stdout: std::process::Stdio,
        stderr: std::process::Stdio,
        resize: Option<std::os::fd::OwnedFd>,
    ) -> Result<crate::Running> {
        #[cfg(target_os = "linux")]
        {
            let helper = self.spawn_exec(command, options, stdin, stdout, stderr, resize)?;
            Ok(crate::Running::for_exec(helper))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (command, options, stdin, stdout, stderr, resize);
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
        resize: Option<std::os::fd::OwnedFd>,
    ) -> Result<crate::helper::HelperChild> {
        let info = self.info()?;
        if info.state.status != Status::Running {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("container {} is not running", info.name),
                "exec needs a running container",
                "start a new container, or run `podbag ps -a` to see its state",
            ));
        }
        if info.state.paused {
            return Err(pause::exec_refused(&self.id));
        }
        let req = crate::linux::exec::ExecRequest {
            command: command.iter().map(|c| c.as_ref().to_string()).collect(),
            options: options.clone(),
            resize: resize.is_some(),
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
        // a process in a scope of the same slice may.
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
                        "install systemd, or set PODBAG_SYSTEMD_RUN",
                    )
                })?,
                slice: slice.clone(),
                unit: None,
                delegate: false,
            }),
            _ => None,
        };
        let mut helper = crate::helper::spawn_with_fd(
            &["__exec", &self.id, &json],
            stdin,
            stdout,
            stderr,
            scope.as_ref(),
            resize,
        )?;
        helper.wait_started()?;
        Ok(helper)
    }

    /// The details of `path` in the container (`docker cp` asks before it copies).
    pub fn stat_path(&self, path: &str) -> Result<crate::archive::PathStat> {
        #[cfg(target_os = "linux")]
        {
            let mut helper = self.archive_helper(
                crate::linux::archive::ArchiveOp::Stat,
                path,
                Default::default(),
                std::process::Stdio::null(),
                std::process::Stdio::null(),
            )?;
            decode_stat(helper.wait_done()?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            Err(Error::unsupported_os())
        }
    }

    /// `path` in the container as a tar whose names start with its last part, after its
    /// details. Read the tar to its end, or drop it to stop.
    pub fn copy_out(&self, path: &str) -> Result<(crate::archive::PathStat, CopyOut)> {
        #[cfg(target_os = "linux")]
        {
            let mut helper = self.archive_helper(
                crate::linux::archive::ArchiveOp::Get,
                path,
                Default::default(),
                std::process::Stdio::null(),
                std::process::Stdio::piped(),
            )?;
            let stat = decode_stat(helper.wait_ready()?)?;
            let tar = helper.take_stdout();
            Ok((
                stat,
                CopyOut {
                    helper,
                    tar,
                    ended: false,
                },
            ))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            Err(Error::unsupported_os())
        }
    }

    /// Unpack the tar (gzip-compressed or not) read from `tar` into the directory `path`
    /// of the container, running or not.
    pub fn copy_in(
        &self,
        path: &str,
        tar: &mut dyn std::io::Read,
        options: &crate::archive::PutOptions,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let mut helper = self.archive_helper(
                crate::linux::archive::ArchiveOp::Put,
                path,
                options.clone(),
                std::process::Stdio::piped(),
                std::process::Stdio::null(),
            )?;
            let copied = match helper.take_stdin() {
                Some(mut stdin) => std::io::copy(tar, &mut stdin),
                None => Ok(0),
            };
            // A helper that ended (done, or failed) closed its end: its result tells.
            let result = helper.wait_done();
            match copied {
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(Error::io(
                    "cannot read the archive to copy",
                    &e,
                    "send the whole archive",
                )),
                _ => result.map(|_| ()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, tar, options);
            Err(Error::unsupported_os())
        }
    }

    /// What changed in the container's files since its image (`docker diff`), sorted.
    pub fn changes(&self) -> Result<Vec<crate::build::Change>> {
        #[cfg(target_os = "linux")]
        {
            let mut helper = self.archive_helper(
                crate::linux::archive::ArchiveOp::Changes,
                "/",
                Default::default(),
                std::process::Stdio::null(),
                std::process::Stdio::null(),
            )?;
            serde_json::from_value(helper.wait_done()?).map_err(|e| {
                Error::new(
                    ErrorKind::Internal,
                    "the archive helper sent an unreadable list",
                    e.to_string(),
                    "report this bug",
                )
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::unsupported_os())
        }
    }

    /// A new image of the container's image plus its changes (`docker commit`).
    pub fn commit(&self, options: &crate::build::CommitOptions) -> Result<crate::build::Built> {
        #[cfg(target_os = "linux")]
        {
            crate::build::commit::commit(self, options)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = options;
            Err(Error::unsupported_os())
        }
    }

    /// All the container's files, without its volumes, as a tar (`docker export`).
    pub fn export(&self) -> Result<CopyOut> {
        self.tar_of(
            #[cfg(target_os = "linux")]
            crate::linux::archive::ArchiveOp::Export,
        )
    }

    /// What changed since its image, as a layer tar (builds and `commit`).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn layer(&self) -> Result<CopyOut> {
        self.tar_of(
            #[cfg(target_os = "linux")]
            crate::linux::archive::ArchiveOp::Layer,
        )
    }

    fn tar_of(
        &self,
        #[cfg(target_os = "linux")] op: crate::linux::archive::ArchiveOp,
    ) -> Result<CopyOut> {
        #[cfg(target_os = "linux")]
        {
            let mut helper = self.archive_helper(
                op,
                "/",
                Default::default(),
                std::process::Stdio::null(),
                std::process::Stdio::piped(),
            )?;
            helper.wait_ready()?;
            let tar = helper.take_stdout();
            Ok(CopyOut {
                helper,
                tar,
                ended: false,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::unsupported_os())
        }
    }

    #[cfg(target_os = "linux")]
    fn archive_helper(
        &self,
        op: crate::linux::archive::ArchiveOp,
        path: &str,
        put: crate::archive::PutOptions,
        stdin: std::process::Stdio,
        stdout: std::process::Stdio,
    ) -> Result<crate::helper::HelperChild> {
        let req = crate::linux::archive::ArchiveRequest {
            op,
            path: path.to_string(),
            put,
        };
        let json = serde_json::to_string(&req).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                "cannot encode an archive request",
                e.to_string(),
                "report this bug",
            )
        })?;
        crate::helper::spawn_with_args(
            &["__archive", &self.id, &json],
            stdin,
            stdout,
            std::process::Stdio::inherit(),
            None,
        )
    }

    /// A start under way (a restarter's) holds the shim lock before the container has a
    /// control socket: send `signal` once it has one. False when the lock was let go first
    /// (the start failed, or what held it was the cleanup of an ended run).
    fn signal_a_start(&self, dirs: &Dirs, signal: i32) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if !state::shim_running(dirs, &self.id) || std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
            if self.kill_in(dirs, signal).is_ok() {
                return true;
            }
            // One that runs and refuses the signal is ending, not starting.
            if state::read_state(dirs, &self.id).is_ok_and(|s| s.status == Status::Running) {
                return false;
            }
        }
    }

    /// Hold the container's shim lock for its removal: no start can begin while the
    /// record goes. A start under way or the cleanup of an ended run holds it for a moment;
    /// with `force`, what such a start began is killed. None when the directory went
    /// meanwhile (a `--rm` container removed itself).
    #[cfg(target_os = "linux")]
    fn quiet(&self, dirs: &Dirs, force: bool) -> Result<Option<std::fs::File>> {
        if force {
            // A restarter that has not started it yet leaves it alone now.
            crate::restart::mark_stopped(dirs, &self.id);
        }
        let short = crate::id::short_id(&self.id);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match state::try_shim_lock(dirs, &self.id) {
                Ok(Some(lock)) => return Ok(Some(lock)),
                Ok(None) => {}
                Err(_) if !dirs.container_dir(&self.id).exists() => return Ok(None),
                Err(e) => return Err(e),
            }
            if force {
                // Reaches what a start began, once it has a control socket.
                let _ = self.kill_in(dirs, libc::SIGKILL);
            } else if state::read_state(dirs, &self.id).is_ok_and(|s| s.status == Status::Running) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!("cannot remove container {short}"),
                    "it is running",
                    "stop it first, or use `podbag rm -f`",
                ));
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!("cannot remove container {short}"),
                    "a start, or the cleanup of its last run, did not end within 10 seconds",
                    "try again",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Join bridge network `network` too, with `aliases` there, from the container's next
    /// start on (`network connect`). A container that runs is refused: stop it first.
    pub fn connect(&self, network: &str, aliases: &[String]) -> Result<()> {
        let dirs = paths::dirs()?;
        let network = match crate::network::parse_network_arg(network)? {
            crate::network::NetworkMode::Bridge { network } => network,
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "cannot connect container {} to network {network}",
                        self.short()
                    ),
                    format!("{network} is a network mode, not a network to join"),
                    "connect it to a bridge network",
                ));
            }
        };
        crate::network::get_in(&dirs, &network)?;
        state::change_settings(&dirs, &self.id, |spec| {
            if spec.bridge_networks().contains(&network.as_str()) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "cannot connect container {} to network {network}",
                        self.short()
                    ),
                    "it is already connected to that network",
                    "leave it as it is",
                ));
            }
            spec.extra_networks.push(crate::spec::Attachment {
                network: network.clone(),
                aliases: aliases.to_vec(),
            });
            Ok(())
        })
    }

    /// Leave network `network` from the container's next start on (`network disconnect`).
    /// Its first network stays; a container that runs is refused.
    pub fn disconnect(&self, network: &str) -> Result<()> {
        let dirs = paths::dirs()?;
        let network = match crate::network::parse_network_arg(network)? {
            crate::network::NetworkMode::Bridge { network } => network,
            _ => network.to_string(),
        };
        state::change_settings(&dirs, &self.id, |spec| {
            if let Some(at) = spec
                .extra_networks
                .iter()
                .position(|a| a.network == network)
            {
                spec.extra_networks.remove(at);
                return Ok(());
            }
            let what = format!(
                "cannot disconnect container {} from network {network}",
                self.short()
            );
            if spec.bridge_networks().first() == Some(&network.as_str()) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    what,
                    "it is the container's first network",
                    "create the container again with the networks it needs",
                ));
            }
            Err(Error::new(
                ErrorKind::NotFound,
                what,
                "it is not connected to that network",
                "run `podbag inspect` to see its networks",
            ))
        })
    }

    fn short(&self) -> &str {
        crate::id::short_id(&self.id)
    }

    /// Delete the container. A running container is refused unless `force`, which kills it.
    pub fn remove(&self, force: bool) -> Result<()> {
        let dirs = paths::dirs()?;
        let info = info_of(&dirs, &self.id)?;
        // A container that was running when a forced removal began may remove itself
        // while it ends (`--rm`): gone is what the caller asked for.
        let mut may_vanish = false;
        if info.state.status == Status::Running {
            if !force {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!("cannot remove container {}", info.name),
                    "it is running",
                    "stop it first, or use `podbag rm -f`",
                ));
            }
            let _ = self.kill(libc::SIGKILL);
            let _ = self.wait();
            may_vanish = true;
        }
        // Held while the record goes: a start (a restarter's too) cannot begin meanwhile.
        #[cfg(target_os = "linux")]
        let _quiet = self.quiet(&dirs, force)?;
        let gone = || may_vanish && !dirs.container_dir(&self.id).exists();
        // The last chance to find the container's cgroup: its record goes away now.
        #[cfg(target_os = "linux")]
        match state::read_state(&dirs, &self.id) {
            Ok(st) => {
                if let Some(path) = st.cgroup {
                    crate::cgroup::remove_dir(std::path::Path::new(&path));
                }
            }
            // Its own removal has taken the record already (and takes the cgroup): what
            // is left of the directory goes below.
            Err(_) if may_vanish => {}
            Err(e) => return Err(e),
        }
        #[cfg(target_os = "linux")]
        if let Ok(spec) = state::read_config(&dirs, &self.id) {
            crate::volume::plugin::release(&dirs, &spec)?;
        }
        if state::remove_container_dir(&dirs, &self.id).is_ok() {
            return Ok(());
        }
        // Files the container's users created (subordinate IDs when rootless).
        match remove_with_helper(&self.id) {
            Err(_) if gone() => Ok(()),
            done => done,
        }
    }

    /// [`ContainerHandle::remove`], and also delete the container's anonymous volumes
    /// (`rm -v`).
    pub fn remove_with_volumes(&self, force: bool) -> Result<()> {
        let dirs = paths::dirs()?;
        let spec = state::read_config(&dirs, &self.id)?;
        self.remove(force)?;
        let anonymous = crate::volume::anonymous_volumes(&spec.mounts);
        crate::volume::remove_anonymous(&dirs, &self.id, anonymous)
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
                "check `podbag logs` for the last output",
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
        mounts: spec.mounts.iter().map(MountInfo::from).collect(),
        restart_count: crate::restart::record(dirs, id).count,
        health: spec
            .health
            .as_ref()
            .map(|_| crate::health::read(dirs, id).unwrap_or_default()),
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
                    Some(Request::Resize { .. }) => Response::Ok { ok: true },
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
            ip: None,
            ports: Vec::new(),
            paused: false,
        };
        state::write_state(&dirs, &id, &running).unwrap();
        let _shim_alive = state::hold_shim_lock(&dirs, &id).unwrap();
        let shim = exiting_shim(&dirs, &id, 3);
        let handle = ContainerHandle::from_id(id);
        assert_eq!(
            handle.stop_in(&dirs, Some(Duration::from_secs(1))).unwrap(),
            3
        );
        shim.join().unwrap();
    }
}
