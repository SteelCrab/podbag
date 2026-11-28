//! Where the `docker` command goes, and switching it to podbag and back. Only the
//! library's functions: they read the docker CLI's files, and change only podbag's own
//! context.

use podbag::client::{DockerTarget, previous_context};
use serde::Serialize;
use std::path::Path;

/// The `docker` command, as the window shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerCommand {
    /// The docker CLI is installed (else nothing can be switched).
    pub installed: bool,
    /// Where it goes: `podbag`, `Docker Desktop`, `OrbStack`, … or the host.
    pub name: String,
    pub is_podbag: bool,
    /// `DOCKER_HOST` is set here: shells that set it do not follow a context switch.
    pub from_env: bool,
    pub context: Option<String>,
    /// The context "switch back" returns to.
    pub previous: Option<String>,
}

pub fn describe(target: DockerTarget, installed: bool, previous: Option<String>) -> DockerCommand {
    DockerCommand {
        installed,
        name: target.name,
        is_podbag: target.is_podbag,
        from_env: target.from_env,
        context: target.context,
        previous,
    }
}

/// The `docker` command now. It reads files only.
pub fn current(podbag_socket: &Path) -> DockerCommand {
    describe(
        DockerTarget::current(podbag_socket),
        podbag::client::find_docker_cli().is_some(),
        previous_context(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_desktop_is_named() {
        let home = tempfile::tempdir().unwrap();
        let docker = home.path().join(".docker");
        std::fs::create_dir_all(&docker).unwrap();
        std::fs::write(
            docker.join("config.json"),
            r#"{"currentContext": "desktop-linux"}"#,
        )
        .unwrap();
        let env = |_: &str| None;
        let t = DockerTarget::find(&env, Some(home.path()), Path::new("/p/docker.sock"));
        let d = describe(t, true, Some("desktop-linux".into()));
        assert_eq!(d.name, "Docker Desktop");
        assert!(!d.is_podbag && !d.from_env && d.installed);
        assert_eq!(d.context.as_deref(), Some("desktop-linux"));
    }

    #[test]
    fn docker_host_wins_over_contexts() {
        let home = tempfile::tempdir().unwrap();
        let sock = home.path().join("podbag.sock");
        std::fs::write(&sock, "").unwrap();
        let host = format!("unix://{}", sock.display());
        let env = move |k: &str| (k == "DOCKER_HOST").then(|| host.clone());
        let t = DockerTarget::find(&env, Some(home.path()), &sock);
        let d = describe(t, false, None);
        assert!(d.is_podbag && d.from_env && !d.installed);
        assert_eq!(d.name, "podbag");
        assert_eq!(d.context, None);
    }
}
