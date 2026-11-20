//! The CLI and Desktop see the same temporary container store.
use crate::client::{Client, ContainerSummary, Endpoint};
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

pub const ACTIVE: &str = "active-container-session";
pub const FORMAT: &str = "podbag-container-basics-v1";
pub const PREFIX: &str = "podbag-container-";
pub const IMAGE: &str = "docker.io/library/busybox:1.37.0";
pub const NAME: &str = "hello-web";
pub const START: &[&str] = &[
    "run",
    "-d",
    "--name",
    NAME,
    "--network",
    "none",
    IMAGE,
    "httpd",
    "-f",
    "-p",
    "8080",
];

#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub session: String,
    pub stage: String,
    pub containers: Vec<ContainerSummary>,
}

pub fn client(root: &Path) -> Client {
    Client::new(Endpoint::Socket {
        #[cfg(target_os = "macos")]
        path: root.join("run/docker.sock"),
        #[cfg(not(target_os = "macos"))]
        path: root.join("api.sock"),
        own: false,
    })
}

pub fn view(
    library: &Path,
    action: &str,
    expected: Option<&str>,
    name: &str,
) -> io::Result<Option<Snapshot>> {
    let Some((id, root)) = super::gui::session_root(library, ACTIVE, PREFIX, action, expected)?
    else {
        return Ok(None);
    };
    let run = root.join("run");
    if !run.join("container-lab").is_file() || run.join("closing").exists() {
        return if action == "status" {
            Ok(None)
        } else {
            Err(io::Error::other("The practice session has ended."))
        };
    }
    let client = client(&root);
    let containers = || client.containers().map_err(io::Error::other);
    let mut rows = containers()?;
    if action != "status" {
        // Accept only the exact ID shown in this practice session.
        let target = rows
            .iter()
            .find(|c| c.name() == NAME && c.id == name)
            .ok_or_else(|| io::Error::other("The practice container was not found."))?;
        match action {
            "stop" if target.running() => {
                client
                    .container_action(&target.id, "stop")
                    .map_err(io::Error::other)?;
                fs::write(run.join("desktop-stopped"), &target.id)?;
                rows = containers()?;
            }
            _ => {
                return Err(io::Error::other(
                    "The practice container cannot perform that action.",
                ));
            }
        }
    }
    let stage = match rows.iter().find(|c| c.name() == NAME) {
        Some(row) if row.running() => "stop",
        Some(row) if stopped_in_desktop(&root, row) => {
            if root.join("verified").is_file() {
                "finish"
            } else {
                "check"
            }
        }
        Some(_) => "restart",
        None => "run",
    };
    Ok(Some(Snapshot {
        session: id,
        stage: stage.into(),
        containers: rows,
    }))
}

pub fn stopped_in_desktop(root: &Path, row: &ContainerSummary) -> bool {
    row.state == "exited"
        && fs::read_to_string(root.join("run/desktop-stopped")).is_ok_and(|id| id == row.id)
}
