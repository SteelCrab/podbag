//! Connect the desktop volume view to the temporary store opened by the practice shell.
use crate::client::{Client, Endpoint, Kind, VolumeSummary};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

pub const ACTIVE: &str = "active-volume-session";
pub const FORMAT: &str = "podbag-cli-gui-volumes-v1";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub session: String,
    pub stage: String,
    pub volumes: Vec<VolumeSummary>,
    pub detail: Option<serde_json::Value>,
}

/// One GUI session per installed library. The lifetime lock is released if the shell dies.
pub struct Registration {
    file: PathBuf,
    _lock: fs::File,
}

impl Registration {
    pub fn open(library: &Path, root: &Path) -> io::Result<Self> {
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(library.join("volume-session.lock"))?;
        lock.try_lock().map_err(|_| {
            io::Error::other("A practice shell is already open. Type exit in that shell first.")
        })?;
        let id = root
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| io::Error::other("Invalid practice directory."))?;
        let mut tmp = tempfile::NamedTempFile::new_in(library)?;
        tmp.write_all(id.as_bytes())?;
        let file = library.join(ACTIVE);
        fs::write(root.join("run/gui"), [])?;
        tmp.persist(&file)?;
        Ok(Self { file, _lock: lock })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
    }
}

pub fn view(
    library: &Path,
    action: &str,
    expected: Option<&str>,
    name: &str,
) -> io::Result<Option<Snapshot>> {
    let id = match fs::read_to_string(library.join(ACTIVE)) {
        Ok(id) => id,
        Err(e) if e.kind() == io::ErrorKind::NotFound && action == "status" => return Ok(None),
        Err(e) => return Err(e),
    };
    if !id
        .strip_prefix("podbag-volume-")
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric()))
    {
        return Err(io::Error::other("Invalid practice session."));
    }
    if action != "status" && expected != Some(id.as_str()) {
        return Err(io::Error::other(
            "This action belongs to a different practice session.",
        ));
    }
    let root = Path::new("/tmp").join(&id);
    let run = root.join("run");
    if !run.join("gui").is_file() {
        return if action == "status" {
            Ok(None)
        } else {
            Err(io::Error::other("The practice session has ended."))
        };
    }
    let client = Client::new(Endpoint::Socket {
        path: root.join("api.sock"),
        own: false,
    });
    let mut detail = None;
    match action {
        "status" => {}
        "inspect" => {
            detail = Some(
                client
                    .inspect(Kind::Volumes, name)
                    .map_err(io::Error::other)?,
            );
            if name == "my-api" {
                fs::write(run.join("gui-inspected"), [])?;
            }
        }
        "remove" => {
            if name == "my-api" && !run.join("gui-inspected").exists() {
                return Err(io::Error::other("Open the volume details first."));
            }
            client
                .remove(Kind::Volumes, name, false)
                .map_err(io::Error::other)?;
            if name == "my-api" {
                fs::write(run.join("gui-removed"), [])?;
            }
        }
        _ => return Err(io::Error::other("Unknown practice volume action.")),
    }
    let volumes = client.volumes().map_err(io::Error::other)?;
    let stage = if volumes.iter().any(|v| v.name == "my-api") {
        if run.join("gui-inspected").is_file() {
            "remove"
        } else {
            "inspect"
        }
    } else if root.join("verified").is_file() {
        "finish"
    } else if run.join("gui-removed").is_file() {
        "verify"
    } else {
        "create"
    };
    Ok(Some(Snapshot {
        session: id,
        stage: stage.into(),
        volumes,
        detail,
    }))
}
