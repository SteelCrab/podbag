//! The starter's side of a handover: give a running detached container to the keeper.

use super::proto::{self, Handover};
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The starter's socket to the keeper.
pub const KEEPER_FD: RawFd = 4;

/// The file in a container's directory with the settings of the client that started it:
/// the keeper's finisher runs with them.
pub const ENV_FILE: &str = "env.json";

fn failed(why: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::Runtime,
        "cannot give the container to the keeper",
        why.to_string(),
        "run the command again; `OPENPOD_KEEPER=0` starts containers without the keeper",
    )
}

/// Give container `id` (process `pid`) to the keeper with its control listener, lock and
/// output descriptors (`fds`, in that order). Returns when the keeper has taken it.
pub fn send(dirs: &Dirs, id: &str, pid: u32, tty: bool, fds: &[RawFd]) -> Result<()> {
    let env = super::settings();
    let env_path = dirs.container_dir(id).join(ENV_FILE);
    serde_json::to_vec(&env)
        .map_err(failed)
        .and_then(|text| std::fs::write(&env_path, text).map_err(failed))?;
    let _ = crate::store::perm::set_mode(&env_path, 0o600);
    let message = Handover {
        id: id.to_string(),
        pid,
        tty,
        data: dirs.data.to_string_lossy().into_owned(),
        runtime: dirs.runtime.to_string_lossy().into_owned(),
        config: dirs.config.to_string_lossy().into_owned(),
    };
    let line = serde_json::to_vec(&message).map_err(failed)?;
    // SAFETY: fd 4 is the socket the keeper gave this starter; it stays open until this
    // process ends, so it is borrowed here, never closed.
    let conn = std::mem::ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(KEEPER_FD) });
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    proto::send_with_fds(&conn, &line, fds).map_err(failed)?;
    match proto::recv_with_fds(&conn, 0) {
        Ok((answer, _)) if answer == b"ok" => Ok(()),
        Ok(_) => Err(failed("the keeper refused it")),
        Err(e) => Err(failed(e)),
    }
}

/// The settings saved by [`send`] for container `id`.
pub fn saved_env(dirs: &Dirs, id: &str) -> Vec<(String, String)> {
    std::fs::read(dirs.container_dir(id).join(ENV_FILE))
        .ok()
        .and_then(|text| serde_json::from_slice(&text).ok())
        .unwrap_or_default()
}
