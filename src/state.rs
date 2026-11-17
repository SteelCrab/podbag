//! Container directories and their settings files.

use crate::paths::Dirs;
use crate::spec::ContainerSpec;
use crate::{Error, ErrorKind, Result, id};
use std::io::Write;

const CONFIG: &str = "config.json";

/// Create the container directory (mode 0700) and write its settings atomically.
pub fn write_config(dirs: &Dirs, spec: &ContainerSpec) -> Result<()> {
    id::check(&spec.id)?;
    let dir = dirs.container_dir(&spec.id);
    let fail = |e: std::io::Error| {
        Error::io(
            format!("cannot save container {}", spec.id),
            &e,
            format!("check that {} is writable", dirs.data.display()),
        )
    };
    create_private_dir(&dir).map_err(fail)?;

    let json = serde_json::to_vec_pretty(spec).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "cannot encode container settings",
            e.to_string(),
            "report this bug",
        )
    })?;
    let tmp = dir.join(format!(".{CONFIG}.tmp"));
    let mut f = std::fs::File::create(&tmp).map_err(fail)?;
    f.write_all(&json)
        .and_then(|_| f.sync_all())
        .map_err(fail)?;
    std::fs::rename(&tmp, dir.join(CONFIG)).map_err(fail)
}

pub fn read_config(dirs: &Dirs, id: &str) -> Result<ContainerSpec> {
    id::check(id)?;
    let path = dirs.container_dir(id).join(CONFIG);
    let bytes = std::fs::read(&path).map_err(|e| {
        let kind = if e.kind() == std::io::ErrorKind::NotFound {
            ErrorKind::NotFound
        } else {
            ErrorKind::Runtime
        };
        Error::new(
            kind,
            format!("cannot load container {id}"),
            format!("{}: {e}", path.display()),
            "check the container ID",
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            format!("cannot load container {id}"),
            format!("{} is damaged: {e}", path.display()),
            "remove the container directory",
        )
    })
}

/// Remove a container directory. Removing a directory that does not exist succeeds.
pub fn remove_container_dir(dirs: &Dirs, id: &str) -> Result<()> {
    id::check(id)?;
    let dir = dirs.container_dir(id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(
            format!("cannot remove container directory {}", dir.display()),
            &e,
            "remove it by hand",
        )),
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(parent) = dir.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::paths::Dirs;
    use crate::spec::spec_fixture;

    fn dirs_in(tmp: &std::path::Path) -> Dirs {
        Dirs {
            data: tmp.join("data"),
            runtime: tmp.join("run"),
            config: tmp.join("cfg"),
        }
    }

    #[test]
    fn config_round_trips_and_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (dirs, spec) = (dirs_in(tmp.path()), spec_fixture());
        write_config(&dirs, &spec).unwrap();
        assert_eq!(read_config(&dirs, &spec.id).unwrap(), spec);
        let mode = std::fs::metadata(dirs.container_dir(&spec.id))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let names: Vec<_> = std::fs::read_dir(dirs.container_dir(&spec.id))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.json")]);
    }

    #[test]
    fn missing_container_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "0123abcd".repeat(8);
        let e = read_config(&dirs_in(tmp.path()), &id).unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(e.what, format!("cannot load container {id}"));
    }

    #[test]
    fn invalid_ids_never_touch_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(tmp.path());
        let keep = dirs.data.join("keep");
        std::fs::create_dir_all(&keep).unwrap();
        for bad in ["..", "../..", ""] {
            assert_eq!(
                remove_container_dir(&dirs, bad).unwrap_err().kind,
                ErrorKind::InvalidInput
            );
            assert_eq!(
                read_config(&dirs, bad).unwrap_err().kind,
                ErrorKind::InvalidInput
            );
        }
        let mut spec = spec_fixture();
        spec.id = "../escape".into();
        assert_eq!(
            write_config(&dirs, &spec).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert!(keep.exists());
    }

    #[test]
    fn remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let (dirs, spec) = (dirs_in(tmp.path()), spec_fixture());
        write_config(&dirs, &spec).unwrap();
        remove_container_dir(&dirs, &spec.id).unwrap();
        assert!(!dirs.container_dir(&spec.id).exists());
        remove_container_dir(&dirs, &spec.id).unwrap();
    }
}
