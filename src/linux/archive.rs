//! `podbag __archive <id> '<json>'`: a container's files for `docker cp`.
//!
//! In a running container the helper joins its user namespace (rootless) and mount
//! namespace and makes the container's root its own. In one that does not run, it holds
//! the shim lock (no start can begin meanwhile), and mounts the container's root
//! filesystem, volumes and bind mounts in a mount namespace of its own (rootless: in a user
//! namespace mapped like the container's). Either way paths resolve as the container sees
//! them: a symbolic link to /etc leads to the container's /etc, never the host's.
//!
//! The result goes on the status channel: `done` with the path's details (stat, get) or
//! nothing (put), or `failed`. A get writes the tar on stdout after `done`; a put reads
//! the tar from stdin. A get that fails after `done` ends with a nonzero exit code.

use crate::archive::PutOptions;
use crate::helper::{StatusChannel, StatusMessage};
use crate::state::{self, Status};
use crate::{Error, ErrorKind, Result, paths};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

/// What `__archive` receives on its command line.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ArchiveRequest {
    pub op: ArchiveOp,
    /// The path in the container; a relative one starts at its root.
    pub path: String,
    #[serde(default)]
    pub put: PutOptions,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveOp {
    /// Report the path's details.
    Stat,
    /// Report its details, then write it as a tar on stdout.
    Get,
    /// Unpack the tar (gzip-compressed or not) on stdin into the directory.
    Put,
    /// Report `done`, then write what changed since the image as a layer tar on stdout
    /// (builds, `commit`). The path is not used.
    Layer,
    /// Report what changed since the image, as a list (`diff`).
    Changes,
    /// Report `done`, then write all files (no volumes) as a tar on stdout (`export`).
    Export,
}

impl ArchiveOp {
    /// Ops that read the container's own layer rather than a path in it.
    fn reads_layer(self) -> bool {
        matches!(
            self,
            ArchiveOp::Layer | ArchiveOp::Changes | ArchiveOp::Export
        )
    }
}

/// Where a container's changes are: its upper directory, the merged view, the image, and
/// mounts to leave out of an export. `upper` is None for a plain folder (`--rootfs`).
struct View {
    upper: Option<String>,
    merged: String,
    lower: String,
    skip: Vec<String>,
}

/// How long a start under way, or the cleanup of an ended run, may hold the container.
const BUSY_WAIT: Duration = Duration::from_secs(10);

pub fn archive_main(id: &str, json: &str) -> i32 {
    let mut status = StatusChannel::take();
    match run(id, json, &mut status) {
        Ok(()) => 0,
        // After `done` the report goes nowhere: the exit code and the log tell.
        Err(e) => {
            eprintln!("podbag: {e}");
            status.fail(e)
        }
    }
}

fn run(id: &str, json: &str, status: &mut StatusChannel) -> Result<()> {
    crate::id::check(id)?;
    let dirs = paths::dirs()?;
    let req: ArchiveRequest = serde_json::from_str(json).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "cannot read an archive request",
            e.to_string(),
            "report this bug",
        )
    })?;
    let spec = state::read_config(&dirs, id)?;
    // SAFETY: geteuid has no preconditions.
    let rootless = unsafe { libc::geteuid() } != 0;
    let deadline = Instant::now() + BUSY_WAIT;
    loop {
        if let Some(_lock) = state::try_shim_lock(&dirs, id)? {
            // Nothing runs it, and nothing starts it until this ends. Its image may need
            // mounting again (after a reboot).
            crate::store::ensure_image_mount(&dirs, &spec.rootfs)?;
            if req.op.reads_layer() {
                return crate::linux::userns::run_in_namespaces(libc::CLONE_NEWNS, || {
                    let view = match &spec.rootfs {
                        crate::spec::Rootfs::Dir { path } => View {
                            upper: None,
                            merged: path.clone(),
                            lower: path.clone(),
                            skip: Vec::new(),
                        },
                        crate::spec::Rootfs::Overlay { .. } => {
                            let (upper, merged, lower) =
                                super::rootfs::mount_changes(&spec, rootless)?;
                            View {
                                upper: Some(upper),
                                merged,
                                lower,
                                skip: Vec::new(),
                            }
                        }
                    };
                    serve_layer(&req, &view, &spec.name, rootless, status)
                });
            }
            return crate::linux::userns::run_in_namespaces(libc::CLONE_NEWNS, || {
                super::rootfs::enter_files(&spec, rootless)?;
                serve(&req, status)
            });
        }
        let st = state::read_state(&dirs, id)?;
        if let (Status::Running, Some(pid)) = (st.status, st.pid) {
            if req.op.reads_layer() {
                let view = view_running(pid, rootless, &spec)?;
                return serve_layer(&req, &view, &spec.name, rootless, status);
            }
            enter_running(pid, rootless, &spec.name)?;
            return serve(&req, status);
        }
        if Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("cannot reach the files of container {}", spec.name),
                "a start, or the cleanup of its last run, did not end within 10 seconds",
                "try again",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Join the namespaces of the container's process `pid` that hold its files, and make
/// its root this process's root.
fn enter_running(pid: u32, rootless: bool, name: &str) -> Result<()> {
    let fail = |e: std::io::Error| {
        Error::io(
            format!("cannot reach the files of container {name}"),
            &e,
            "check that the container is still running",
        )
    };
    // Open everything first: after joining the mount namespace, /proc paths change meaning.
    let open = |what: &str| File::open(format!("/proc/{pid}/{what}")).map_err(fail);
    let user = if rootless {
        Some(open("ns/user")?)
    } else {
        None
    };
    let mnt = open("ns/mnt")?;
    let root = open("root")?;
    for (ns, kind) in [
        (user.as_ref(), libc::CLONE_NEWUSER),
        (Some(&mnt), libc::CLONE_NEWNS),
    ] {
        let Some(ns) = ns else { continue };
        // SAFETY: setns on a namespace descriptor we opened; this process is single-threaded.
        if unsafe { libc::setns(ns.as_raw_fd(), kind) } != 0 {
            return Err(fail(std::io::Error::last_os_error()));
        }
    }
    // SAFETY: fchdir to the container's root directory, then make it this process's root.
    unsafe {
        if libc::fchdir(root.as_raw_fd()) != 0
            || libc::chroot(c".".as_ptr()) != 0
            || libc::chdir(c"/".as_ptr()) != 0
        {
            return Err(fail(std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// A running container's view: its files through `/proc/<pid>/root`, and its upper
/// directory, after joining its user namespace (rootless).
fn view_running(pid: u32, rootless: bool, spec: &crate::spec::ContainerSpec) -> Result<View> {
    let fail = |e: std::io::Error| {
        Error::io(
            format!("cannot reach the files of container {}", spec.name),
            &e,
            "check that the container is still running",
        )
    };
    let mountinfo = std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).map_err(fail)?;
    // Mount points inside the container, relative to its root: left out of an export.
    let skip = mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|p| p.trim_start_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if rootless {
        let user = File::open(format!("/proc/{pid}/ns/user")).map_err(fail)?;
        // SAFETY: setns on a namespace descriptor we opened; this process is single-threaded.
        if unsafe { libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) } != 0 {
            return Err(fail(std::io::Error::last_os_error()));
        }
    }
    let (upper, lower) = match &spec.rootfs {
        crate::spec::Rootfs::Overlay { upper, lower, .. } => (Some(upper.clone()), lower.clone()),
        crate::spec::Rootfs::Dir { path } => (None, path.clone()),
    };
    Ok(View {
        upper,
        merged: format!("/proc/{pid}/root"),
        lower,
        skip,
    })
}

/// Read the container's changes or files and report them.
fn serve_layer(
    req: &ArchiveRequest,
    view: &View,
    name: &str,
    rootless: bool,
    status: &mut StatusChannel,
) -> Result<()> {
    use crate::build::layer::{self, XattrSpace};
    let space = XattrSpace::for_rootless(rootless);
    let upper = || {
        view.upper
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| super::rootfs::no_image_layer(name))
    };
    let read_fail = |e: std::io::Error| {
        Error::io(
            format!("cannot read the files of container {name}"),
            &e,
            "check that the reader is still there",
        )
    };
    let stdout = || {
        // Not through std's stdout: a child that ends with _exit never flushes it.
        // SAFETY: fd 1 is this process's stdout; ManuallyDrop leaves it open.
        std::mem::ManuallyDrop::new(unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(1) })
    };
    match req.op {
        ArchiveOp::Changes => {
            let list = layer::changes(upper()?, Path::new(&view.lower)).map_err(read_fail)?;
            status.report(&StatusMessage::Done {
                output: serde_json::to_value(list).unwrap_or_default(),
            });
            Ok(())
        }
        ArchiveOp::Layer => {
            let upper = upper()?;
            status.report(&StatusMessage::Done {
                output: serde_json::Value::Null,
            });
            let mut raw = stdout();
            let mut out = std::io::BufWriter::with_capacity(256 * 1024, &mut *raw);
            layer::write_layer(upper, Path::new(&view.merged), space, &mut out)
                .and_then(|()| std::io::Write::flush(&mut out))
                .map_err(read_fail)
        }
        ArchiveOp::Export => {
            status.report(&StatusMessage::Done {
                output: serde_json::Value::Null,
            });
            let mut raw = stdout();
            let mut out = std::io::BufWriter::with_capacity(256 * 1024, &mut *raw);
            layer::write_tree(Path::new(&view.merged), &view.skip, &mut out)
                .and_then(|()| std::io::Write::flush(&mut out))
                .map_err(read_fail)
        }
        _ => unreachable!("only ops that read the layer come here"),
    }
}

/// Do the work in the container's root and report `done`. An error goes back to the
/// caller, who reports it (or, after `done`, ends with a nonzero code).
fn serve(req: &ArchiveRequest, status: &mut StatusChannel) -> Result<()> {
    let root = Path::new("/");
    let done = |status: &mut StatusChannel, output: serde_json::Value| {
        status.report(&StatusMessage::Done { output });
    };
    match req.op {
        ArchiveOp::Stat => {
            let stat = crate::archive::stat(root, &req.path)?;
            done(status, serde_json::to_value(stat).unwrap_or_default());
            Ok(())
        }
        ArchiveOp::Get => {
            let stat = crate::archive::stat(root, &req.path)?;
            done(status, serde_json::to_value(stat).unwrap_or_default());
            // Not through std's stdout: a child that ends with _exit never flushes it.
            // SAFETY: fd 1 is this process's stdout; ManuallyDrop leaves it open.
            let mut raw = std::mem::ManuallyDrop::new(unsafe {
                <File as std::os::fd::FromRawFd>::from_raw_fd(1)
            });
            let mut out = std::io::BufWriter::with_capacity(256 * 1024, &mut *raw);
            crate::archive::write_tar(root, &req.path, &mut out)?;
            std::io::Write::flush(&mut out).map_err(|e| {
                Error::io(
                    "cannot send the archive",
                    &e,
                    "check that the reader is still there",
                )
            })
        }
        ArchiveOp::Put => {
            let mut input = std::io::stdin().lock();
            crate::archive::extract(root, &req.path, &mut input, &req.put)?;
            done(status, serde_json::Value::Null);
            Ok(())
        }
        ArchiveOp::Layer | ArchiveOp::Changes | ArchiveOp::Export => {
            unreachable!("served by serve_layer")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_without_put_options_load() {
        let req: ArchiveRequest = serde_json::from_str(r#"{"op":"stat","path":"/etc"}"#).unwrap();
        assert_eq!(req.op, ArchiveOp::Stat);
        assert_eq!(req.put, PutOptions::default());
    }
}
