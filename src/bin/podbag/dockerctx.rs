//! `podbag system docker status|use|restore`: where the `docker` command goes, and
//! switching it to podbag (a docker CLI context named `podbag`) and back. The same
//! library functions serve the TUI and the desktop app.

use crate::args::DockerCmd;
use podbag::client::{DockerTarget, Endpoint, PODBAG_CONTEXT, choice, podbag_program};
use std::path::PathBuf;

pub fn run(cmd: DockerCmd) -> i32 {
    match go(cmd) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("podbag: {e}");
            1
        }
    }
}

/// podbag's API socket on this system.
fn podbag_socket() -> Result<PathBuf, String> {
    match Endpoint::podbag_default(&podbag_program())? {
        Endpoint::Socket { path, .. } => Ok(path),
        Endpoint::Command(_) => Err(
            "on Windows, podbag's Docker API is not yet reachable from Windows programs, so \
             the docker CLI cannot use it; run docker commands as podbag commands instead"
                .into(),
        ),
    }
}

fn docker_cli() -> Result<PathBuf, String> {
    choice::find_docker_cli().ok_or_else(|| {
        "the docker CLI is not installed; podbag's own commands work without it".into()
    })
}

/// A note when DOCKER_HOST is set in this shell: it wins over contexts here.
fn env_note(t: &DockerTarget) {
    if t.from_env {
        println!(
            "DOCKER_HOST is set in this shell ({}): it wins over contexts. Unset it to use \
             the current context.",
            t.host
        );
    }
}

fn go(cmd: DockerCmd) -> Result<(), String> {
    match cmd {
        DockerCmd::Status => {
            let socket = podbag_socket().unwrap_or_default();
            let t = DockerTarget::current(&socket);
            match &t.context {
                Some(c) => println!("docker → {} (context {c}, {})", t.name, t.host),
                None => println!("docker → {} ({})", t.name, t.host),
            }
            env_note(&t);
            if choice::find_docker_cli().is_none() {
                println!("The docker CLI is not installed.");
            } else if !t.is_podbag {
                println!("To use podbag: podbag system docker use");
            } else if let Some(prev) = choice::find_docker_cli().and(previous()) {
                println!("To go back to {prev}: podbag system docker restore");
            }
            Ok(())
        }
        DockerCmd::Use => {
            let docker = docker_cli()?;
            let socket = podbag_socket()?;
            podbag::client::use_podbag(&docker, &socket)?;
            println!(
                "docker → podbag (context {PODBAG_CONTEXT}, unix://{})",
                socket.display()
            );
            if let Some(prev) = previous() {
                println!("`podbag system docker restore` goes back to {prev}.");
            }
            if cfg!(target_os = "linux") {
                println!(
                    "The docker CLI needs podbag's API running: `podbag system service`, or \
                     its systemd units (`podbag system service --print-units`)."
                );
            }
            env_note(&DockerTarget::current(&socket));
            Ok(())
        }
        DockerCmd::Restore => {
            let docker = docker_cli()?;
            let now = podbag::client::switch_back(&docker)?;
            println!("docker → context {now}");
            let socket = podbag_socket().unwrap_or_default();
            env_note(&DockerTarget::current(&socket));
            Ok(())
        }
    }
}

fn previous() -> Option<String> {
    podbag::client::previous_context()
}
