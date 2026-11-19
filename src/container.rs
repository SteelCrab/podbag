//! The public container API.

use crate::image::{Image, ImageConfig};
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

/// A container to run. Build it, then call a `run` method.
///
/// ```no_run
/// let image = podbag::Image::pull("alpine")?;
/// let out = podbag::Container::from_image(&image)
///     .env("GREETING", "hello")
///     .command(["sh", "-c", "echo $GREETING"])
///     .run()?;
/// assert_eq!(out.stdout, b"hello\n");
/// # Ok::<(), podbag::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Container {
    source: Source,
    command: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
    env: Vec<(String, String)>,
    workdir: Option<String>,
    hostname: Option<String>,
    user: Option<String>,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    auto_remove: bool,
    #[serde(default)]
    tty: bool,
    #[serde(default)]
    memory: Option<u64>,
    #[serde(default)]
    cpus: Option<f64>,
    #[serde(default)]
    pids_limit: Option<i64>,
    #[serde(default)]
    security: crate::spec::Security,
    #[serde(default)]
    mounts: Vec<crate::volume::MountRequest>,
    #[serde(default)]
    network: Option<crate::network::NetworkMode>,
    #[serde(default)]
    publish: Vec<crate::network::PortMapping>,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    extra_networks: Vec<crate::spec::Attachment>,
    #[serde(default)]
    restart: crate::restart::RestartPolicy,
    #[serde(default)]
    health: Option<crate::health::HealthSettings>,
    #[serde(default)]
    console_size: Option<(u16, u16)>,
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
            interactive: false,
            name: None,
            auto_remove: false,
            tty: false,
            memory: None,
            cpus: None,
            pids_limit: None,
            security: crate::spec::Security::default(),
            mounts: Vec::new(),
            network: None,
            publish: Vec::new(),
            labels: std::collections::BTreeMap::new(),
            aliases: Vec::new(),
            extra_networks: Vec::new(),
            restart: Default::default(),
            health: None,
            console_size: None,
        }
    }

    /// A label (`key=value` in `docker run --label`).
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Container {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// How to connect the container: a bridge network (the default `bridge`, or one made
    /// with [`crate::network::Network::create`]), the host's network, or none.
    pub fn network(mut self, mode: crate::network::NetworkMode) -> Container {
        self.network = Some(mode);
        self
    }

    /// A further name of the container on its network (`--network-alias`): the other
    /// containers there find it by this name too. User networks only.
    pub fn network_alias(mut self, alias: impl Into<String>) -> Container {
        self.aliases.push(alias.into());
        self
    }

    /// Join a further bridge network too (`--network` given again), with names of its own
    /// there. Its interface is `eth1`, `eth2`, … in the order of these calls; the default
    /// route stays on the first network.
    pub fn extra_network(mut self, network: impl Into<String>, aliases: Vec<String>) -> Container {
        self.extra_networks.push(crate::spec::Attachment {
            network: network.into(),
            aliases,
        });
        self
    }

    /// Start the container again after it ends, as `policy` says (`--restart`). Not with
    /// [`Container::auto_remove`].
    pub fn restart(mut self, policy: crate::restart::RestartPolicy) -> Container {
        self.restart = policy;
        self
    }

    /// Its health check (`--health-cmd` and the like): fields it leaves out come from the
    /// image's check. [`crate::health::HealthSettings::none`] turns the image's off.
    pub fn health(mut self, settings: crate::health::HealthSettings) -> Container {
        self.health = Some(settings);
        self
    }

    /// The first size of its terminal, with [`Container::tty`] (Docker's `ConsoleSize`).
    pub fn console_size(mut self, rows: u16, cols: u16) -> Container {
        self.console_size = (rows > 0 && cols > 0).then_some((rows, cols));
        self
    }

    /// Publish a container port on the host (`-p`).
    pub fn publish(mut self, port: crate::network::PortMapping) -> Container {
        self.publish.push(port);
        self
    }

    /// Mount a host path or a named volume at `target`. A `source` that starts with `/` or
    /// `.` is a host path (relative to this process's working directory; created if
    /// missing); anything else names a volume, created on first use and filled with the
    /// image's files at `target`.
    pub fn volume(
        mut self,
        source: impl Into<String>,
        target: impl Into<String>,
        access: crate::Access,
    ) -> Container {
        let (source, target) = (source.into(), target.into());
        self.mounts
            .push(if source.starts_with('/') || source.starts_with('.') {
                crate::MountRequest::Bind {
                    source,
                    target,
                    access,
                }
            } else {
                crate::MountRequest::Named {
                    name: source,
                    target,
                    access,
                    nocopy: false,
                }
            });
        self
    }

    /// Mount a new volume of the container's own at `target`. It is removed with the
    /// container when the container is removed on exit.
    pub fn anonymous_volume(
        mut self,
        target: impl Into<String>,
        access: crate::Access,
    ) -> Container {
        self.mounts.push(crate::MountRequest::Anonymous {
            target: target.into(),
            access,
        });
        self
    }

    /// Mount a memory file system at `target`; `options` like `size=64m,mode=1777`.
    pub fn tmpfs(mut self, target: impl Into<String>, options: impl Into<String>) -> Container {
        self.mounts.push(crate::MountRequest::Tmpfs {
            target: target.into(),
            options: options.into(),
        });
        self
    }

    /// Add a mount request as `-v` or `--tmpfs` produced it.
    pub fn mount(mut self, request: crate::MountRequest) -> Container {
        self.mounts.push(request);
        self
    }

    /// Keep a capability beyond Docker's defaults (`--cap-add`; a name or `ALL`).
    pub fn cap_add(mut self, name: impl Into<String>) -> Container {
        self.security.cap_add.push(name.into());
        self
    }

    /// Drop a capability (`--cap-drop`; a name or `ALL`). Drops apply before adds.
    pub fn cap_drop(mut self, name: impl Into<String>) -> Container {
        self.security.cap_drop.push(name.into());
        self
    }

    /// Stop setuid programs from gaining privileges (`--security-opt no-new-privileges`).
    pub fn no_new_privileges(mut self, yes: bool) -> Container {
        self.security.no_new_privileges = yes;
        self
    }

    /// `--privileged`: every capability, no seccomp filter, the kernel's files unmasked, a
    /// writable /sys and cgroup (BuildKit and other container tools need them). Rootless,
    /// it gives no more than the user has: all of it applies inside the user namespace.
    pub fn privileged(mut self, yes: bool) -> Container {
        self.security.privileged = yes;
        self
    }

    /// Use the default seccomp filter (on unless turned off: `seccomp=unconfined`).
    pub fn seccomp(mut self, yes: bool) -> Container {
        self.security.seccomp = yes;
        self
    }

    /// Limit the container's memory to `bytes` (`--memory`; at least 6 MiB).
    pub fn memory(mut self, bytes: u64) -> Container {
        self.memory = Some(bytes);
        self
    }

    /// Limit the container to this many CPUs' worth of time (`--cpus`; at least 0.01).
    pub fn cpus(mut self, cpus: f64) -> Container {
        self.cpus = Some(cpus);
        self
    }

    /// Limit the number of processes and threads (`--pids-limit`; 0 or less: no limit).
    pub fn pids_limit(mut self, limit: i64) -> Container {
        self.pids_limit = Some(limit);
        self
    }

    /// The limits as the shim applies them. Invalid values are errors.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn limits(&self) -> Result<crate::cgroup::Limits> {
        if let Some(bytes) = self.memory.filter(|&b| b < crate::cgroup::MIN_MEMORY) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("invalid memory limit {bytes}"),
                "Minimum memory limit allowed is 6MiB",
                "give at least 6 MiB",
            ));
        }
        Ok(crate::cgroup::Limits {
            memory: self.memory,
            cpu_quota: self.cpus.map(crate::cgroup::cpus_to_quota).transpose()?,
            pids: self.pids_limit.and_then(crate::cgroup::pids_max),
            ..Default::default()
        })
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

    /// A unique name for the container. The default is the first 12 characters of its ID.
    pub fn name(mut self, name: impl Into<String>) -> Container {
        self.name = Some(name.into());
        self
    }

    /// Delete the container when it ends (`--rm`). [`Container::run`] and
    /// [`Container::run_inherit`] always do; [`Container::start`] keeps it by default.
    pub fn remove_on_exit(mut self, yes: bool) -> Container {
        self.auto_remove = yes;
        self
    }

    /// Give the command a terminal (`-t`).
    pub fn tty(mut self, yes: bool) -> Container {
        self.tty = yes;
        self
    }

    /// Connect the caller's stdin to the command (`-i`). Without it, stdin is empty.
    pub fn interactive(mut self, yes: bool) -> Container {
        self.interactive = yes;
        self
    }

    /// The user to run as: `name`, `uid`, `name:group` or `uid:gid`. Overrides the image's User.
    pub fn user(mut self, user: impl Into<String>) -> Container {
        self.user = Some(user.into());
        self
    }

    /// Run the container and collect its output. Its stdin is empty. The container is
    /// removed when it ends.
    pub fn run(mut self) -> Result<Output> {
        #[cfg(target_os = "linux")]
        {
            use crate::helper::Io;
            self.auto_remove = true;
            let (_cleanup, mut helper) =
                linux_launch::launch(self, Io::Null, Io::Piped, Io::Piped, false)?;
            collect_output(&mut helper)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = &mut self;
            Err(Error::unsupported_os())
        }
    }

    /// Run the container with this process's stdin, stdout and stderr.
    /// Returns the command's exit code, or 128 + signal number. The container is removed
    /// when it ends.
    pub fn run_inherit(mut self) -> Result<i32> {
        self.auto_remove = true;
        self.spawn_inherit()?.wait()
    }

    /// Start the container in the background and return at once. It keeps running after
    /// this process exits, and stays after it ends until [`ContainerHandle::remove`]
    /// (unless [`Container::remove_on_exit`]).
    #[cfg(unix)]
    pub fn start(self) -> Result<crate::ContainerHandle> {
        #[cfg(target_os = "linux")]
        {
            use crate::helper::Io;
            let (cleanup, mut helper) =
                linux_launch::launch(self, Io::Null, Io::Null, Io::Null, true)?;
            helper.release_detached();
            Ok(crate::ContainerHandle::from_id(cleanup.disarm()))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self;
            Err(Error::unsupported_os())
        }
    }

    /// Create the container without starting it (`docker create`): it is listed as
    /// Created and starts with [`crate::ContainerHandle::start`].
    #[cfg(unix)]
    pub fn create(self) -> Result<crate::ContainerHandle> {
        #[cfg(target_os = "linux")]
        {
            let (cleanup, _) = linux_launch::create(self)?;
            Ok(crate::ContainerHandle::from_id(cleanup.disarm()))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self;
            Err(Error::unsupported_os())
        }
    }

    /// Start the container with this process's stdin, stdout and stderr, and return
    /// without waiting. Used by the CLI to forward signals.
    #[doc(hidden)]
    pub fn spawn_inherit(self) -> Result<Running> {
        #[cfg(target_os = "linux")]
        {
            use crate::helper::Io;
            let (cleanup, helper) =
                linux_launch::launch(self, Io::Inherit, Io::Inherit, Io::Inherit, false)?;
            Ok(Running {
                cleanup: Some(cleanup),
                helper,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::unsupported_os())
        }
    }
}

/// Docker's rule for container names: a letter or digit, then letters, digits, `_`, `.`
/// and `-`.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn check_name(name: &str) -> Result<()> {
    let ok = name
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid container name ({name})"),
            "only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed",
            "choose another name",
        ))
    }
}

/// Start the shim of an existing container; a failed start keeps the container.
#[cfg(target_os = "linux")]
pub(crate) fn start_shim(
    dirs: &crate::paths::Dirs,
    id: &str,
    stdin: crate::helper::Io,
    stdout: crate::helper::Io,
    stderr: crate::helper::Io,
    mode: crate::helper::ShimMode,
) -> Result<crate::helper::HelperChild> {
    linux_launch::start(dirs, id, stdin, stdout, stderr, mode, true).map_err(|failed| failed.error)
}

/// Read a helper's stdout and stderr to the end, then wait for it.
#[cfg(target_os = "linux")]
pub(crate) fn collect_output(helper: &mut crate::helper::HelperChild) -> Result<Output> {
    use std::io::Read;
    // Read both streams at once; reading one to the end first can block on a full pipe.
    let mut err_pipe = helper.take_stderr();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        err_pipe
            .as_mut()
            .map(|p| p.read_to_end(&mut buf))
            .transpose()
            .map(|_| buf)
    });
    let mut stdout = Vec::new();
    let out_result = match helper.take_stdout() {
        Some(mut p) => p.read_to_end(&mut stdout).map(drop),
        None => Ok(()),
    };
    // Always collect the other stream and the helper, even after a read error.
    let err_result = err_reader.join().unwrap_or_else(|_| Ok(Vec::new()));
    let exit_code = linux_launch::wait(helper)?;
    let read_fail =
        |e: std::io::Error| Error::io("cannot read the container output", &e, "report this bug");
    out_result.map_err(read_fail)?;
    let stderr = err_result.map_err(read_fail)?;
    Ok(Output {
        exit_code,
        stdout,
        stderr,
    })
}

/// A started container. Returned by [`Container::spawn_inherit`].
#[doc(hidden)]
pub struct Running {
    #[cfg(target_os = "linux")]
    cleanup: Option<linux_launch::Cleanup>,
    #[cfg(target_os = "linux")]
    helper: crate::helper::HelperChild,
}

impl Running {
    #[cfg(target_os = "linux")]
    pub(crate) fn for_exec(helper: crate::helper::HelperChild) -> Running {
        Running {
            cleanup: None,
            helper,
        }
    }

    /// The PID of the helper process that watches the container.
    pub fn helper_pid(&self) -> u32 {
        #[cfg(target_os = "linux")]
        return self.helper.pid();
        #[cfg(not(target_os = "linux"))]
        unreachable!("no container runs on this operating system")
    }

    /// Pass these signals on to the helper while this process waits for it. A helper that
    /// the keeper started is outside this process's terminal: it gets the terminal's
    /// signals (Ctrl+C, a new window size) from here too.
    pub fn forward_signals(&self, signals: &[i32]) {
        #[cfg(target_os = "linux")]
        if self.helper.is_kept() {
            let mut all = signals.to_vec();
            all.push(libc::SIGWINCH);
            crate::signals::forward_all_to(self.helper.pid(), &all);
        } else {
            crate::signals::forward_to(self.helper.pid(), signals);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = signals;
    }

    /// Wait for the container to end. Returns its exit code, or 128 + signal number.
    pub fn wait(mut self) -> Result<i32> {
        #[cfg(target_os = "linux")]
        {
            let code = linux_launch::wait(&mut self.helper);
            if code.is_err() && !self.helper.ended() {
                // A kept helper that outlived its keeper would still run the container:
                // its record stays.
                let _ = self.cleanup.take().map(linux_launch::Cleanup::disarm);
            } else {
                drop(self.cleanup);
            }
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
    use crate::helper::{self, HelperChild, Io};
    use crate::paths::Dirs;
    use crate::spec::{ContainerSpec, Rootfs};
    use crate::store::ops::StoreOp;
    use crate::store::{ImageRoot, Store};
    use crate::volume::{self, Access, Mount, MountRequest, MountSource};
    use crate::{Error, ErrorKind, Result, id, paths, state};

    /// Removes the container directory when dropped. Files that container users created
    /// (owned by subordinate IDs when rootless) are removed through the store helper.
    pub(super) struct Cleanup {
        dirs: Dirs,
        id: String,
        armed: bool,
        /// Anonymous volumes made for this container, removed with it.
        anonymous: Vec<String>,
    }

    impl Cleanup {
        /// Keep the container; return its ID.
        pub(super) fn disarm(mut self) -> String {
            self.armed = false;
            std::mem::take(&mut self.id)
        }
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            if state::remove_container_dir(&self.dirs, &self.id).is_err() {
                let _ = helper::run_store_op(
                    &StoreOp::RemoveContainer {
                        id: self.id.clone(),
                    },
                    false,
                );
            }
            let anonymous = self.anonymous.iter().map(String::as_str);
            let _ = crate::volume::remove_anonymous(&self.dirs, &self.id, anonymous);
        }
    }

    /// Turn mount requests into mounts: host paths become absolute (and exist), volumes
    /// exist. New anonymous volumes are listed in `anonymous`.
    fn resolve_mounts(
        dirs: &Dirs,
        requests: Vec<MountRequest>,
        anonymous: &mut Vec<String>,
    ) -> Result<Vec<Mount>> {
        let access = |a: Access| a == Access::ReadOnly;
        let empty = |path: &str| std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none());
        let mut mounts = Vec::with_capacity(requests.len());
        for request in requests {
            mounts.push(match request {
                MountRequest::Bind {
                    source,
                    target,
                    access: a,
                } => Mount {
                    source: MountSource::Bind {
                        path: bind_source(&source)?,
                    },
                    target,
                    read_only: access(a),
                    copy_up: false,
                },
                MountRequest::Named {
                    name,
                    target,
                    access: a,
                    nocopy,
                } => {
                    let (v, _) = volume::create_in(dirs, Some(&name), false)?;
                    Mount {
                        copy_up: !nocopy && empty(&v.mountpoint),
                        source: MountSource::Volume {
                            name,
                            anonymous: false,
                            path: v.mountpoint,
                        },
                        target,
                        read_only: access(a),
                    }
                }
                MountRequest::Anonymous { target, access: a } => {
                    let (v, _) = volume::create_in(dirs, None, true)?;
                    anonymous.push(v.name.clone());
                    Mount {
                        source: MountSource::Volume {
                            name: v.name,
                            anonymous: true,
                            path: v.mountpoint,
                        },
                        target,
                        read_only: access(a),
                        copy_up: true,
                    }
                }
                MountRequest::Tmpfs { target, options } => {
                    // The builder takes options as given; check them as --tmpfs does.
                    let MountRequest::Tmpfs { target, options } =
                        volume::parse_tmpfs_arg(&format!("{target}:{options}"))?
                    else {
                        unreachable!("parse_tmpfs_arg returns a tmpfs request")
                    };
                    Mount {
                        source: MountSource::Tmpfs { options },
                        target,
                        read_only: false,
                        copy_up: false,
                    }
                }
            });
        }
        Ok(mounts)
    }

    /// A host path for `-v`: absolute, created when missing (as `docker run -v` does).
    fn bind_source(source: &str) -> Result<String> {
        let fail = |e: std::io::Error| {
            Error::io(
                format!("cannot use {source} as a volume"),
                &e,
                "check that the path exists or can be created",
            )
        };
        let path = std::path::Path::new(source);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_err(fail)?.join(path)
        };
        if !path.exists() {
            std::fs::create_dir_all(&path).map_err(fail)?;
        }
        Ok(std::fs::canonicalize(&path)
            .map_err(fail)?
            .to_string_lossy()
            .into_owned())
    }

    /// Save the settings and start the shim. Returns after the command started.
    /// Create the container and start it: a failed start removes it again (`run`).
    pub(super) fn launch(
        c: Container,
        stdin: Io,
        stdout: Io,
        stderr: Io,
        detach: bool,
    ) -> Result<(Cleanup, HelperChild)> {
        let (mut cleanup, auto_remove) = create(c)?;
        let mode = if detach {
            helper::ShimMode::Detached
        } else {
            helper::ShimMode::Attached
        };
        let started = start(
            &cleanup.dirs,
            &cleanup.id,
            stdin,
            stdout,
            stderr,
            mode,
            false,
        );
        let helper = match started {
            Ok(helper) => helper,
            Err(failed) => {
                if failed.unanswered {
                    // A starter may run the container: nothing of it is removed.
                    let _ = cleanup.disarm();
                }
                return Err(failed.error);
            }
        };
        // Started: from here the shim owns the container. It removes it at the end when
        // auto-remove is set; otherwise the container stays.
        cleanup.armed = auto_remove && !detach;
        Ok((cleanup, helper))
    }

    /// Everything up to the container record (Status::Created): checks, volumes, the spec,
    /// directories. The returned guard removes it all until disarmed. Also returns whether
    /// the container is removed when it ends.
    pub(super) fn create(c: Container) -> Result<(Cleanup, bool)> {
        if let Some(name) = &c.name {
            super::check_name(name)?;
        }
        let limits = c.limits()?;
        // SAFETY: geteuid has no preconditions.
        let rootless = unsafe { libc::geteuid() } != 0;
        let (placement, warning) = crate::cgroup::choose_placement(rootless, &limits)?;
        if let Some(w) = warning {
            eprintln!("podbag: warning: {w}");
        }
        let dirs = paths::dirs()?;
        let network = c.network.clone().unwrap_or_default();
        let mut c = c;
        if !c.publish.is_empty() {
            match network {
                crate::network::NetworkMode::Host => {
                    eprintln!("podbag: warning: published ports are discarded with --network host");
                    c.publish.clear();
                }
                crate::network::NetworkMode::None => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "cannot publish ports",
                        "the container has no network (--network none)",
                        "use a bridge network, or drop -p",
                    ));
                }
                crate::network::NetworkMode::Bridge { .. } => {}
            }
        }
        crate::spec::check_networks(&network, &c.aliases, &c.extra_networks)?;
        // Without the keeper, a rootless bridge container's shim is in the network
        // session's user namespace: only the user's service manager can start its restarter.
        if c.restart != crate::restart::RestartPolicy::No
            && rootless
            && matches!(network, crate::network::NetworkMode::Bridge { .. })
            && !crate::keeper::enabled(&dirs)
            && !(crate::cgroup::systemd_run().is_some() && crate::cgroup::user_manager())
        {
            eprintln!(
                "podbag: warning: the restart policy needs the keeper or a systemd user session; \
                 this container will not be started again"
            );
        }
        if c.auto_remove && c.restart != crate::restart::RestartPolicy::No {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cannot run the container",
                "--restart and --rm do not go together: one starts it again, the other removes it",
                "drop one of them",
            ));
        }
        if let crate::network::NetworkMode::Bridge { network } = &network {
            crate::network::get_in(&dirs, network)?;
        }
        for a in &c.extra_networks {
            crate::network::get_in(&dirs, &a.network)?;
        }
        let id = id::new_id()?;
        let short = id::short_id(&id).to_string();
        let hostname = c.hostname.clone().unwrap_or_else(|| short.clone());
        let name = c.name.clone().unwrap_or_else(|| short.clone());
        let (auto_remove, tty, interactive) = (c.auto_remove, c.tty, c.interactive);
        let image_volumes = match &c.source {
            Source::Image(image) => image.config.volumes.clone(),
            Source::Rootfs(_) => Vec::new(),
        };
        let health = crate::health::resolve(
            c.health.as_ref(),
            match &c.source {
                Source::Image(image) => image.config.healthcheck.as_ref(),
                Source::Rootfs(_) => None,
            },
        )?;
        let requests = volume::with_image_volumes(c.mounts.clone(), &image_volumes)?;
        // Declared before the lock below, so on an early return the lock goes first: the
        // cleanup takes the volume lock exclusively, and a shared lock held by this same
        // process would block it.
        let mut cleanup = Cleanup {
            dirs: dirs.clone(),
            id: id.clone(),
            armed: true,
            anonymous: Vec::new(),
        };
        // Held until the config names the volumes: `volume rm` checks the configs under
        // the exclusive lock.
        let volume_lock = (!requests.is_empty())
            .then(|| volume::lock_shared(&dirs))
            .transpose()?;
        let mounts = resolve_mounts(&dirs, requests, &mut cleanup.anonymous)?;
        match c.source {
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
                    image_name: None,
                    name: name.clone(),
                    auto_remove,
                    tty,
                    interactive,
                    detach: false,
                    labels: c.labels.clone(),
                    limits: limits.clone(),
                    cgroup: placement.clone(),
                    security: c.security.clone(),
                    mounts: mounts.clone(),
                    network: network.clone(),
                    publish: c.publish.clone(),
                    aliases: c.aliases.clone(),
                    restart: c.restart,
                    health: health.clone(),
                    console_size: c.console_size,
                    extra_networks: c.extra_networks.clone(),
                };
                spec.validate()?;
                state::create(&dirs, &spec)?;
            }
            Source::Image(ref image) => {
                let command =
                    command_line(&image.config, c.entrypoint.as_deref(), c.command.as_deref())?;
                let not_pulled = || {
                    Error::new(
                        ErrorKind::NotFound,
                        format!("cannot find image {}", image.name),
                        "it has not been pulled",
                        format!("run `podbag pull {}`", image.name),
                    )
                };
                // As root the store may build what this mount method needs; rootless stores
                // always hold the hard-link tree a pull made.
                let mut store = if rootless {
                    Store::open_existing(&dirs).ok_or_else(not_pulled)?
                } else {
                    Store::open_existing(&dirs).ok_or_else(not_pulled)?;
                    Store::open(&dirs, true)?
                };
                store.use_mount_method(rootless, crate::config::load(&dirs)?.image_mount);
                let _ = store.touch(&image.name);
                // Hold the shared lock until the config names the image: prune and rmi
                // skip images that a container config names.
                let lock = store.lock_shared()?;
                let (_, record) = store.find(&image.digest)?;
                let (lower, data) = match store.image_root(&record.digest)? {
                    ImageRoot::Tree(tree) => (tree, None),
                    ImageRoot::Composefs { lower, objects } => (lower, Some(objects)),
                };
                let text = |p: &std::path::Path| p.to_string_lossy().into_owned();
                let dir = dirs.container_dir(&id);
                let sub = |name: &str| dir.join(name).to_string_lossy().into_owned();
                let mut env = image.config.env.clone();
                env.extend(c.env);
                let spec = ContainerSpec {
                    rootfs: Rootfs::Overlay {
                        lower: text(&lower),
                        upper: sub("upper"),
                        work: sub("work"),
                        merged: sub("merged"),
                        data: data.as_deref().map(text),
                    },
                    id,
                    command,
                    env,
                    workdir: c.workdir.or(image.config.workdir.clone()),
                    hostname,
                    user: c.user.or(image.config.user.clone()),
                    image: Some(record.digest),
                    image_name: Some(image.name.clone()),
                    name: name.clone(),
                    auto_remove,
                    tty,
                    interactive,
                    detach: false,
                    labels: c.labels.clone(),
                    limits: limits.clone(),
                    cgroup: placement.clone(),
                    security: c.security.clone(),
                    mounts: mounts.clone(),
                    network: network.clone(),
                    publish: c.publish.clone(),
                    aliases: c.aliases.clone(),
                    restart: c.restart,
                    health: health.clone(),
                    console_size: c.console_size,
                    extra_networks: c.extra_networks.clone(),
                };
                spec.validate()?;
                state::create(&dirs, &spec)?;
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
                                "check the permissions of the podbag data directory",
                            )
                        })?;
                }
                drop(lock);
            }
        }
        drop(volume_lock);
        Ok((cleanup, auto_remove))
    }

    /// A start that failed.
    pub(super) struct Failed {
        pub(super) error: Error,
        /// The keeper took the request and never answered: a starter may still run the
        /// container, so its record must stay.
        pub(super) unanswered: bool,
    }

    impl From<Error> for Failed {
        fn from(error: Error) -> Failed {
            Failed {
                error,
                unanswered: false,
            }
        }
    }

    /// Start the shim of container `id` (created or exited) and wait until it runs.
    pub(super) fn start(
        dirs: &Dirs,
        id: &str,
        stdin: Io,
        stdout: Io,
        stderr: Io,
        mode: helper::ShimMode,
        keep_on_failure: bool,
    ) -> std::result::Result<HelperChild, Failed> {
        use crate::keeper::client::Asked;
        let spec = state::read_config(dirs, id)?;
        state::wait_for_previous_shim(dirs, id, std::time::Duration::from_secs(5))?;
        // Through the keeper when there is one; a shim of our own otherwise.
        let io = [stdin, stdout, stderr];
        if crate::keeper::enabled(dirs) {
            let scope = matches!(spec.cgroup, crate::cgroup::Placement::Scope { .. });
            match crate::keeper::client::start(dirs, id, &io, mode, keep_on_failure, scope) {
                Asked::Started(mut kept) => {
                    kept.wait_started()?;
                    return Ok(kept);
                }
                Asked::Unavailable => {}
                // A shim of our own now could start the container a second time.
                Asked::Unanswered => {
                    return Err(Failed {
                        error: Error::new(
                            ErrorKind::Runtime,
                            format!("cannot start container {}", crate::id::short_id(id)),
                            "the keeper of this runtime directory did not answer in time",
                            "look at it with `podbag ps -a`, then start or remove it; \
                             `PODBAG_KEEPER=0` starts containers without the keeper",
                        ),
                        unanswered: true,
                    });
                }
            }
        }
        let [stdin, stdout, stderr] = io.map(Io::into_stdio);
        let scope = match &spec.cgroup {
            crate::cgroup::Placement::Scope { slice } => Some(helper::Scope {
                systemd_run: crate::cgroup::systemd_run().ok_or_else(|| {
                    Error::new(
                        ErrorKind::MissingRequirement,
                        "cannot start the container in a systemd scope",
                        "systemd-run was not found",
                        "install systemd, or set PODBAG_SYSTEMD_RUN",
                    )
                })?,
                slice: slice.clone(),
                // A name of this run's own: the last run's scope can still be there for a
                // moment (systemd removes it after its last process).
                unit: Some(format!("podbag-{id}-{}", &id::new_id()?[..8])),
                delegate: true,
            }),
            _ => None,
        };
        let mut helper = helper::spawn_shim(
            id,
            stdin,
            stdout,
            stderr,
            scope.as_ref(),
            mode,
            keep_on_failure,
        )?;
        helper.wait_started()?;
        Ok(helper)
    }

    /// Wait for the shim. Returns its exit code, or 128 + signal number.
    pub(super) fn wait(helper: &mut HelperChild) -> Result<i32> {
        let kept = helper.is_kept();
        helper.wait().map_err(|e| {
            if kept {
                // The connection to the keeper ended: everything it watched ended too.
                Error::new(
                    ErrorKind::Runtime,
                    "the container was stopped",
                    format!("the keeper that watched it ended ({e})"),
                    "run it again; `podbag ps -a` shows what is left of it",
                )
            } else {
                Error::io("cannot wait for the container", &e, "report this bug")
            }
        })
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
