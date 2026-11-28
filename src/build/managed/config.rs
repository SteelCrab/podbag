//! Keep the Engine request's registry credentials in memory and out of the worker.

use crate::image::auth::{AuthConfig, key_of};
use crate::{Error, Result};
use rustix::fs::{MemfdFlags, Mode, SealFlags, fchmod, fcntl_add_seals, memfd_create};
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Give buildctl a private Docker config without inheriting the service's logins.
pub(super) fn configure(
    command: &mut Command,
    workspace: &Path,
    logins: &[(String, AuthConfig)],
) -> Result<File> {
    let file = write_config(logins).map_err(config_error)?;
    let fd = file.as_raw_fd();
    std::os::unix::fs::symlink(format!("/proc/self/fd/{fd}"), workspace.join("config.json"))
        .map_err(config_error)?;
    command
        .env_remove("DOCKER_AUTH_CONFIG")
        .env("DOCKER_CONFIG", workspace);
    // SAFETY: fcntl is async-signal-safe. Only this child gets the descriptor across exec.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    };
    Ok(file)
}

fn write_config(logins: &[(String, AuthConfig)]) -> io::Result<File> {
    let mut auths: BTreeMap<_, _> = logins
        .iter()
        .map(|(registry, auth)| (key_of(registry), auth))
        .collect();
    // An empty auths map makes Docker auto-select a credential helper from the host.
    let empty = AuthConfig::default();
    if auths.is_empty() {
        auths.insert("no-credentials.invalid".to_owned(), &empty);
    }
    let mut file = File::from(memfd_create(
        "podbag-buildkit-auth",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?);
    fchmod(&file, Mode::RUSR | Mode::WUSR)?;
    serde_json::to_writer(&mut file, &serde_json::json!({ "auths": auths }))?;
    fcntl_add_seals(
        &file,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
    )?;
    Ok(file)
}

fn config_error(error: io::Error) -> Error {
    Error::io(
        "cannot prepare the private BuildKit registry config",
        &error,
        "check the Linux process filesystem and build workspace",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_client_uses_only_its_own_registry_config() {
        let workspace = tempfile::tempdir().unwrap();
        let mut client = Command::new("sh");
        client.env("DOCKER_AUTH_CONFIG", "service-fixture");
        let _config = configure(&mut client, workspace.path(), &[]).unwrap();
        let status = client
            .args([
                "-c",
                r#"test -z "${DOCKER_AUTH_CONFIG+x}" && test -r "$DOCKER_CONFIG/config.json""#,
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }
}
