//! The public container API.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// A container to run. Build it, then call a `run` method.
///
/// ```no_run
/// let out = openpod::Container::from_rootfs("/srv/alpine")
///     .env("GREETING", "hello")
///     .command(["sh", "-c", "echo $GREETING"])
///     .run()?;
/// assert_eq!(out.stdout, b"hello\n");
/// # Ok::<(), openpod::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Container {
    rootfs: String,
    command: Vec<String>,
    env: Vec<(String, String)>,
    workdir: Option<String>,
    hostname: Option<String>,
}

/// The result of [`Container::run`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// The exit code, or 128 + signal number when a signal ended the command.
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Container {
    /// A container that uses an unpacked root filesystem directory.
    pub fn from_rootfs(path: impl Into<String>) -> Container {
        Container {
            rootfs: path.into(),
            command: Vec::new(),
            env: Vec::new(),
            workdir: None,
            hostname: None,
        }
    }

    /// The command and its arguments.
    pub fn command<I, S>(mut self, args: I) -> Container
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command = args.into_iter().map(Into::into).collect();
        self
    }

    /// Set an environment variable. A later value for the same name wins.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Container {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The working directory inside the container. It is created when missing.
    pub fn workdir(mut self, dir: impl Into<String>) -> Container {
        self.workdir = Some(dir.into());
        self
    }

    /// The container hostname. The default is the first 12 characters of the container ID.
    pub fn hostname(mut self, name: impl Into<String>) -> Container {
        self.hostname = Some(name.into());
        self
    }

    /// Run the container and collect its output. Its stdin is empty.
    pub fn run(self) -> Result<Output> {
        #[cfg(target_os = "linux")]
        {
            use std::io::Read;
            use std::process::Stdio;
            let (_cleanup, mut helper) =
                linux_launch::launch(self, Stdio::null(), Stdio::piped(), Stdio::piped())?;
            let read_fail = |e: std::io::Error| {
                Error::io("cannot read the container output", &e, "report this bug")
            };

            // Read both streams at once; reading one to the end first can block on a full pipe.
            let mut err_pipe = helper.child.stderr.take();
            let err_reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                err_pipe
                    .as_mut()
                    .map(|p| p.read_to_end(&mut buf))
                    .transpose()
                    .map(|_| buf)
            });
            let mut stdout = Vec::new();
            if let Some(mut p) = helper.child.stdout.take() {
                p.read_to_end(&mut stdout).map_err(read_fail)?;
            }
            let stderr = err_reader
                .join()
                .unwrap_or_else(|_| Ok(Vec::new()))
                .map_err(read_fail)?;
            let exit_code = linux_launch::wait(&mut helper)?;
            Ok(Output {
                exit_code,
                stdout,
                stderr,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::unsupported_os())
        }
    }

    /// Run the container with this process's stdin, stdout and stderr.
    /// Returns the command's exit code, or 128 + signal number.
    pub fn run_inherit(self) -> Result<i32> {
        self.spawn_inherit()?.wait()
    }

    /// Start the container with this process's stdin, stdout and stderr, and return
    /// without waiting. Used by the CLI to forward signals.
    #[doc(hidden)]
    pub fn spawn_inherit(self) -> Result<Running> {
        #[cfg(target_os = "linux")]
        {
            use std::process::Stdio;
            let (cleanup, helper) =
                linux_launch::launch(self, Stdio::inherit(), Stdio::inherit(), Stdio::inherit())?;
            Ok(Running { cleanup, helper })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::unsupported_os())
        }
    }
}

/// A started container. Returned by [`Container::spawn_inherit`].
#[doc(hidden)]
pub struct Running {
    #[cfg(target_os = "linux")]
    cleanup: linux_launch::Cleanup,
    #[cfg(target_os = "linux")]
    helper: crate::helper::HelperChild,
}

impl Running {
    /// The PID of the helper process that watches the container.
    pub fn helper_pid(&self) -> u32 {
        #[cfg(target_os = "linux")]
        return self.helper.child.id();
        #[cfg(not(target_os = "linux"))]
        unreachable!("no container runs on this operating system")
    }

    /// Wait for the container to end. Returns its exit code, or 128 + signal number.
    pub fn wait(mut self) -> Result<i32> {
        #[cfg(target_os = "linux")]
        {
            let code = linux_launch::wait(&mut self.helper);
            drop(self.cleanup);
            code
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = &mut self;
            unreachable!("no container runs on this operating system")
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_launch {
    use super::Container;
    use crate::helper::{self, HelperChild};
    use crate::paths::Dirs;
    use crate::spec::ContainerSpec;
    use crate::{Error, ErrorKind, Result, id, paths, state};
    use std::process::Stdio;

    /// Removes the container directory when dropped.
    pub(super) struct Cleanup {
        dirs: Dirs,
        id: String,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = state::remove_container_dir(&self.dirs, &self.id);
        }
    }

    /// Save the settings and start the shim. Returns after the command started.
    pub(super) fn launch(
        c: Container,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<(Cleanup, HelperChild)> {
        let rootfs = std::fs::canonicalize(&c.rootfs)
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("cannot use root filesystem {}", c.rootfs),
                    "the directory does not exist",
                    "check the path, or unpack a root filesystem there",
                )
            })?;
        let dirs = paths::dirs()?;
        let id = id::new_id()?;
        let spec = ContainerSpec {
            hostname: c.hostname.unwrap_or_else(|| id::short_id(&id).to_string()),
            id,
            rootfs: rootfs.to_string_lossy().into_owned(),
            command: c.command,
            env: c.env,
            workdir: c.workdir,
        };
        spec.validate()?;
        state::write_config(&dirs, &spec)?;
        let cleanup = Cleanup { dirs, id: spec.id };

        let mut helper = helper::spawn_shim(&cleanup.id, stdin, stdout, stderr)?;
        helper.wait_started()?;
        Ok((cleanup, helper))
    }

    /// Wait for the shim. Returns its exit code, or 128 + signal number.
    pub(super) fn wait(helper: &mut HelperChild) -> Result<i32> {
        use std::os::unix::process::ExitStatusExt;
        let status = helper
            .child
            .wait()
            .map_err(|e| Error::io("cannot wait for the container", &e, "report this bug"))?;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // macOS and Windows will send these requests to a VM, so they must serialize.
    #[test]
    fn container_request_round_trips_through_json() {
        let c = Container::from_rootfs("/srv/alpine")
            .command(["sh", "-c", "echo hi"])
            .env("A", "1")
            .workdir("/app")
            .hostname("box");
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Container>(&json).unwrap(), c);
    }
}
