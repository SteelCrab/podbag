//! The public container API.

use crate::image::{Image, ImageConfig};
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

/// A container to run. Build it, then call a `run` method.
///
/// ```no_run
/// let image = openpod::Image::pull("alpine")?;
/// let out = openpod::Container::from_image(&image)
///     .env("GREETING", "hello")
///     .command(["sh", "-c", "echo $GREETING"])
///     .run()?;
/// assert_eq!(out.stdout, b"hello\n");
/// # Ok::<(), openpod::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Container {
    source: Source,
    command: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
    env: Vec<(String, String)>,
    workdir: Option<String>,
    hostname: Option<String>,
    user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Source {
    Rootfs(String),
    Image(Image),
}

/// The result of [`Container::run`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// The exit code, or 128 + signal number when a signal ended the command.
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The command to run, by Docker's rules: an `entrypoint` override replaces the image's
/// Entrypoint and drops its Cmd; a `command` replaces the image's Cmd.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn command_line(
    image: &ImageConfig,
    entrypoint: Option<&[String]>,
    command: Option<&[String]>,
) -> Result<Vec<String>> {
    let (entry, cmd) = match entrypoint {
        Some(e) => (
            e.to_vec(),
            command.map(<[String]>::to_vec).unwrap_or_default(),
        ),
        None => (
            image.entrypoint.clone(),
            command
                .map(<[String]>::to_vec)
                .unwrap_or_else(|| image.cmd.clone()),
        ),
    };
    let all: Vec<String> = entry.into_iter().chain(cmd).collect();
    if all.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "no command to run",
            "the image has no Entrypoint or Cmd, and no command was given",
            "give a command after the image name",
        ));
    }
    Ok(all)
}

impl Container {
    /// A container that uses an unpacked root filesystem directory.
    pub fn from_rootfs(path: impl Into<String>) -> Container {
        Container::new(Source::Rootfs(path.into()))
    }

    /// A container from a pulled image. It gets its own writable layer on top of the image.
    pub fn from_image(image: &Image) -> Container {
        Container::new(Source::Image(image.clone()))
    }

    fn new(source: Source) -> Container {
        Container {
            source,
            command: None,
            entrypoint: None,
            env: Vec::new(),
            workdir: None,
            hostname: None,
            user: None,
        }
    }

    /// The command and its arguments. For an image, this replaces the image's Cmd.
    pub fn command<I, S>(mut self, args: I) -> Container
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command = Some(args.into_iter().map(Into::into).collect());
        self
    }

    /// Replace the image's Entrypoint (and drop its Cmd). An empty list means no entrypoint.
    pub fn entrypoint<I, S>(mut self, args: I) -> Container
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.entrypoint = Some(args.into_iter().map(Into::into).collect());
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

    /// The user to run as: `name`, `uid`, `name:group` or `uid:gid`. Overrides the image's User.
    pub fn user(mut self, user: impl Into<String>) -> Container {
        self.user = Some(user.into());
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
    use super::{Container, Source, command_line};
    use crate::helper::{self, HelperChild};
    use crate::paths::Dirs;
    use crate::spec::{ContainerSpec, Rootfs};
    use crate::store::Store;
    use crate::store::ops::StoreOp;
    use crate::{Error, ErrorKind, Result, id, paths, state};
    use std::process::Stdio;

    /// Removes the container directory when dropped. Files that container users created
    /// (owned by subordinate IDs when rootless) are removed through the store helper.
    pub(super) struct Cleanup {
        dirs: Dirs,
        id: String,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if state::remove_container_dir(&self.dirs, &self.id).is_err() {
                let _ = helper::run_store_op(
                    &StoreOp::RemoveContainer {
                        id: self.id.clone(),
                    },
                    false,
                );
            }
        }
    }

    /// Save the settings and start the shim. Returns after the command started.
    pub(super) fn launch(
        c: Container,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<(Cleanup, HelperChild)> {
        let dirs = paths::dirs()?;
        let id = id::new_id()?;
        let hostname = c
            .hostname
            .clone()
            .unwrap_or_else(|| id::short_id(&id).to_string());
        let cleanup = match c.source {
            Source::Rootfs(ref path) => {
                let rootfs = std::fs::canonicalize(path)
                    .ok()
                    .filter(|p| p.is_dir())
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::NotFound,
                            format!("cannot use root filesystem {path}"),
                            "the directory does not exist",
                            "check the path, or unpack a root filesystem there",
                        )
                    })?;
                let spec = ContainerSpec {
                    id,
                    rootfs: Rootfs::Dir {
                        path: rootfs.to_string_lossy().into_owned(),
                    },
                    command: c.command.unwrap_or_default(),
                    env: c.env,
                    workdir: c.workdir,
                    hostname,
                    user: c.user,
                    image: None,
                };
                spec.validate()?;
                state::write_config(&dirs, &spec)?;
                Cleanup { dirs, id: spec.id }
            }
            Source::Image(ref image) => {
                let command =
                    command_line(&image.config, c.entrypoint.as_deref(), c.command.as_deref())?;
                let store = Store::open_existing(&dirs).ok_or_else(|| {
                    Error::new(
                        ErrorKind::NotFound,
                        format!("cannot find image {}", image.name),
                        "it has not been pulled",
                        format!("run `openpod pull {}`", image.name),
                    )
                })?;
                let _ = store.touch(&image.name);
                // Hold the shared lock until the config names the image: prune and rmi
                // skip images that a container config names.
                let lock = store.lock_shared()?;
                let (_, record) = store.find(&image.digest)?;
                let dir = dirs.container_dir(&id);
                let sub = |name: &str| dir.join(name).to_string_lossy().into_owned();
                let mut env = image.config.env.clone();
                env.extend(c.env);
                let spec = ContainerSpec {
                    rootfs: Rootfs::Overlay {
                        lower: store
                            .rootfs_path(&record.digest)
                            .to_string_lossy()
                            .into_owned(),
                        upper: sub("upper"),
                        work: sub("work"),
                        merged: sub("merged"),
                    },
                    id,
                    command,
                    env,
                    workdir: c.workdir.or(image.config.workdir.clone()),
                    hostname,
                    user: c.user.or(image.config.user.clone()),
                    image: Some(record.digest),
                };
                spec.validate()?;
                state::write_config(&dirs, &spec)?;
                let cleanup = Cleanup {
                    dirs,
                    id: spec.id.clone(),
                };
                for name in ["upper", "work", "merged"] {
                    use std::os::unix::fs::DirBuilderExt;
                    let path = dir.join(name);
                    std::fs::DirBuilder::new()
                        .mode(0o755)
                        .create(&path)
                        .map_err(|e| {
                            Error::io(
                                format!("cannot create {}", path.display()),
                                &e,
                                "check the permissions of the openpod data directory",
                            )
                        })?;
                }
                drop(lock);
                cleanup
            }
        };
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
    use crate::ImageConfig;

    fn image(entrypoint: &[&str], cmd: &[&str]) -> ImageConfig {
        ImageConfig {
            entrypoint: entrypoint.iter().map(|s| s.to_string()).collect(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            ..ImageConfig::default()
        }
    }
    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_line_follows_docker() {
        let img = image(&["/entry"], &["default"]);
        assert_eq!(
            command_line(&img, None, None).unwrap(),
            ["/entry", "default"]
        );
        assert_eq!(
            command_line(&img, None, Some(&v(&["mine"]))).unwrap(),
            ["/entry", "mine"]
        );
        assert_eq!(
            command_line(&img, Some(&v(&["/other"])), None).unwrap(),
            ["/other"]
        );
        assert_eq!(
            command_line(&img, Some(&v(&["/other"])), Some(&v(&["x"]))).unwrap(),
            ["/other", "x"]
        );
        assert_eq!(
            command_line(&image(&[], &["sh"]), Some(&[]), Some(&v(&["ls"]))).unwrap(),
            ["ls"]
        );
        let e = command_line(&image(&[], &[]), None, None).unwrap_err();
        assert_eq!(e.what, "no command to run");
    }

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
