//! A locally registered, trusted Docker v1 volume driver over a Unix socket.

use super::DriverVolume;
use crate::client::wire::{self, Body};
use crate::paths::Dirs;
use crate::spec::ContainerSpec;
use crate::{Error, ErrorKind, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MEDIA_TYPE: &str = "application/vnd.docker.plugins.v1.1+json";
const TIMEOUT: Duration = Duration::from_secs(10);

fn failure(driver: &str, operation: &str, why: impl ToString) -> Error {
    Error::new(
        ErrorKind::Runtime,
        format!("volume driver {driver} failed {operation}"),
        why.to_string(),
        "check the configured driver socket and service",
    )
}

fn invoke(dirs: &Dirs, driver: &str, operation: &str, body: Value) -> Result<Value> {
    // SAFETY: geteuid has no preconditions.
    super::require_rootful("external volume drivers", unsafe { libc::geteuid() } != 0)?;
    super::check_driver_name(driver)?;
    let fail = |e: std::io::Error| failure(driver, operation, e);
    let registry = dirs.config.join("volume-drivers");
    let registry_fd = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&registry)
        .map_err(fail)?;
    let metadata = registry_fd.metadata().map_err(fail)?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(failure(
            driver,
            operation,
            "volume-drivers must be a root-owned directory writable only by root",
        ));
    }
    // Pin the directory across validation and connect, even if its parent can be renamed.
    let file = format!("{driver}.sock");
    let socket = PathBuf::from(format!("/proc/self/fd/{}", registry_fd.as_raw_fd())).join(&file);
    if !fs::symlink_metadata(&socket)
        .map_err(fail)?
        .file_type()
        .is_socket()
    {
        return Err(failure(
            driver,
            operation,
            format!("{} is not a Unix socket", registry.join(file).display()),
        ));
    }
    let stream = connect(&socket, TIMEOUT).map_err(fail)?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(fail)?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(fail)?;
    let encoded = body.to_string();
    let response = wire::send(
        Box::new(stream),
        "POST",
        &format!("/{operation}"),
        &[("Accept", MEDIA_TYPE), ("Content-Type", MEDIA_TYPE)],
        Body::Json(encoded.as_bytes()),
    )
    .map_err(fail)?;
    if !(200..300).contains(&response.status) {
        return Err(failure(
            driver,
            operation,
            format!("HTTP status {}", response.status),
        ));
    }
    let bytes = response.bytes(64 * 1024).map_err(fail)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| failure(driver, operation, e))?;
    if !value.is_object() {
        return Err(failure(
            driver,
            operation,
            "the driver response must be a JSON object",
        ));
    }
    match value.get("Err") {
        Some(Value::String(message)) if !message.is_empty() => {
            Err(failure(driver, operation, message))
        }
        Some(value) if !value.is_string() && !value.is_null() => {
            Err(failure(driver, operation, "Err must be a string"))
        }
        _ => Ok(value),
    }
}

/// Connect within `limit`. A blocking connect waits as long as the driver's queue of
/// connections is full; a non-blocking one fails at once then, so it is retried.
fn connect(path: &Path, limit: Duration) -> std::io::Result<UnixStream> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let deadline = Instant::now() + limit;
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    let address = SocketAddrUnix::new(path)?;
    loop {
        match rustix::net::connect(&socket, &address) {
            Ok(()) => break,
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::AGAIN) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(rustix::io::Errno::AGAIN) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the driver accepts no connections",
                ));
            }
            Err(e) => return Err(e.into()),
        }
    }
    // The read and write timeouts work on a blocking socket.
    rustix::io::ioctl_fionbio(&socket, false)?;
    Ok(UnixStream::from(socket))
}

pub(super) fn create(dirs: &Dirs, name: &str, plugin: &DriverVolume) -> Result<()> {
    let active = invoke(dirs, &plugin.name, "Plugin.Activate", json!({}))?;
    if !active["Implements"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item == "VolumeDriver"))
    {
        return Err(failure(
            &plugin.name,
            "Plugin.Activate",
            "the plugin does not implement VolumeDriver",
        ));
    }
    invoke(
        dirs,
        &plugin.name,
        "VolumeDriver.Create",
        json!({"Name": name, "Opts": plugin.options}),
    )
    .map(drop)
}

fn request_phase(
    dirs: &Dirs,
    driver: &str,
    name: &str,
    id: &str,
    operation: &str,
) -> Result<Value> {
    invoke(dirs, driver, operation, json!({"Name":name,"ID":id}))
}

fn leases(dirs: &Dirs, name: &str) -> PathBuf {
    dirs.volume_dir(name).join("leases")
}

fn lease_error(name: &str, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot update mounts of volume {name}"),
        &e,
        "check the Podbag data directory",
    )
}

fn unmount(dirs: &Dirs, driver: &str, name: &str, id: &str) -> Result<()> {
    super::check_name(name)?;
    crate::id::check(id)?;
    let path = leases(dirs, name).join(id);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(lease_error(name, e)),
    }
    request_phase(dirs, driver, name, id, "VolumeDriver.Unmount")?;
    fs::remove_file(&path)
        .and_then(|()| File::open(leases(dirs, name))?.sync_all())
        .map_err(|e| lease_error(name, e))
}

pub(super) fn remove(dirs: &Dirs, name: &str, plugin: &DriverVolume) -> Result<()> {
    let dir = leases(dirs, name);
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|e| lease_error(name, e))?;
                let id = entry.file_name();
                let id = id
                    .to_str()
                    .ok_or_else(|| lease_error(name, std::io::Error::other("invalid mount ID")))?;
                if crate::state::shim_running(dirs, id) {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("cannot remove volume {name}"),
                        "a running container still holds its mount",
                        "stop and remove the container first",
                    ));
                }
                unmount(dirs, &plugin.name, name, id)?;
            }
            fs::remove_dir(&dir).map_err(|e| lease_error(name, e))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(lease_error(name, e)),
    }
    invoke(
        dirs,
        &plugin.name,
        "VolumeDriver.Remove",
        json!({"Name": name}),
    )
    .map(drop)
}

fn mount(dirs: &Dirs, driver: &str, name: &str, id: &str) -> Result<String> {
    super::check_name(name)?;
    crate::id::check(id)?;
    let dir = leases(dirs, name);
    fs::create_dir_all(&dir)
        .and_then(|()| File::create_new(dir.join(id)))
        .and_then(|file| file.sync_all())
        // Sync both directory entries before asking the plugin to allocate a mount.
        .and_then(|()| File::open(&dir)?.sync_all())
        .and_then(|()| File::open(dirs.volume_dir(name))?.sync_all())
        .map_err(|e| lease_error(name, e))?;
    let response = request_phase(dirs, driver, name, id, "VolumeDriver.Mount")?;
    let path = Path::new(response["Mountpoint"].as_str().unwrap_or_default());
    if !path.is_absolute() || !path.is_dir() {
        return Err(failure(
            driver,
            "VolumeDriver.Mount",
            "Mountpoint must be an absolute directory on the engine host",
        ));
    }
    let path = fs::canonicalize(path).map_err(|e| failure(driver, "VolumeDriver.Mount", e))?;
    if path.parent().is_none() {
        return Err(failure(
            driver,
            "VolumeDriver.Mount",
            "Mountpoint cannot be the host root directory",
        ));
    }
    path.into_os_string().into_string().map_err(|_| {
        failure(
            driver,
            "VolumeDriver.Mount",
            "Mountpoint is not valid UTF-8",
        )
    })
}

/// Reuse the same caller ID for each start of a container, after releasing any old mounts.
pub(crate) fn release(dirs: &Dirs, spec: &ContainerSpec) -> Result<()> {
    let mut result = Ok(());
    for (name, driver) in drivers(spec) {
        let released = unmount(dirs, driver, name, &spec.id);
        if result.is_ok() {
            result = released;
        }
    }
    result
}

fn drivers(spec: &ContainerSpec) -> BTreeMap<&str, &str> {
    spec.mounts
        .iter()
        .filter_map(|m| match &m.source {
            super::MountSource::Driver { name, driver } => Some((name.as_str(), driver.as_str())),
            _ => None,
        })
        .collect()
}

pub(crate) fn has_mounts(spec: &ContainerSpec) -> bool {
    spec.mounts
        .iter()
        .any(|m| matches!(m.source, super::MountSource::Driver { .. }))
}

pub(crate) struct Mounts<'a> {
    dirs: &'a Dirs,
    spec: &'a ContainerSpec,
    release_on_drop: bool,
}

impl<'a> Mounts<'a> {
    pub(crate) fn prepare(
        dirs: &'a Dirs,
        spec: &'a ContainerSpec,
        run_spec: &mut ContainerSpec,
    ) -> Result<Self> {
        release(dirs, spec)?;
        let mounts = Self {
            dirs,
            spec,
            release_on_drop: true,
        };
        attach(dirs, run_spec)?;
        Ok(mounts)
    }

    pub(crate) fn release(mut self) {
        self.release_on_drop = false;
        self.cleanup();
    }

    /// Keep the leases: a later start or `volume rm` releases them.
    pub(crate) fn keep(mut self) {
        self.release_on_drop = false;
    }

    fn cleanup(&self) {
        if let Err(e) = release(self.dirs, self.spec) {
            eprintln!("podbag: {e}");
        }
    }
}

impl Drop for Mounts<'_> {
    fn drop(&mut self) {
        if self.release_on_drop {
            self.cleanup();
        }
    }
}

/// Resolve external mounts to bind paths before spawning the container. The caller must
/// hold the shim lock and release the leases on failure or after the container exits.
fn attach(dirs: &Dirs, spec: &mut ContainerSpec) -> Result<()> {
    let mut paths = BTreeMap::new();
    for (name, driver) in drivers(spec) {
        paths.insert(name.to_string(), mount(dirs, driver, name, &spec.id)?);
    }
    for m in &mut spec.mounts {
        if let super::MountSource::Driver { name, .. } = &m.source {
            m.source = super::MountSource::Bind {
                path: paths[name].clone(),
                propagation: Default::default(),
                shallow_ro: false,
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver that stops accepting fills its queue of connections; the next connect
    /// must give up instead of waiting for room.
    #[test]
    fn connecting_to_a_full_queue_ends_in_time() {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("driver.sock");
        let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        rustix::net::bind(&listener, &SocketAddrUnix::new(&path).unwrap()).unwrap();
        // Room for one waiting connection, which nothing accepts.
        rustix::net::listen(&listener, 0).unwrap();
        let (sent, got) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut queued = Vec::new();
            let start = Instant::now();
            let result = loop {
                match connect(&path, Duration::from_millis(100)) {
                    Ok(stream) => queued.push(stream),
                    Err(e) => break e,
                }
            };
            sent.send((result.kind(), start.elapsed())).unwrap();
        });
        let (kind, took) = got
            .recv_timeout(Duration::from_secs(5))
            .expect("connect waited for room in the queue");
        assert_eq!(kind, std::io::ErrorKind::TimedOut);
        assert!(took < Duration::from_secs(2), "{took:?}");
        drop(listener);
    }
}
