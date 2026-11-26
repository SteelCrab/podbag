//! A shell in a container, and terminal windows.

use podbag::client::{Choice, Target};
use std::path::Path;

/// A container id or name that is safe in a command line.
fn safe_id(id: &str) -> Result<&str, String> {
    if !id.starts_with('-')
        && !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
    {
        Ok(id)
    } else {
        Err(format!("invalid container id {id:?}"))
    }
}

/// A shell in a container: the command, and what to type by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub argv: Vec<String>,
    pub by_hand: String,
}

/// `podbag exec -it <id> sh` for podbag; `docker exec -it <id> sh` for another engine,
/// with `DOCKER_HOST` set to its socket.
pub fn shell_command(
    target: &Target,
    podbag: &Path,
    docker: &Path,
    id: &str,
) -> Result<Shell, String> {
    let id = safe_id(id)?;
    let tail = ["exec", "-it", id, "sh"].map(String::from);
    if target.choice == Choice::Podbag {
        let mut argv = vec![podbag.display().to_string()];
        argv.extend(tail);
        return Ok(Shell {
            argv,
            by_hand: format!("podbag exec -it {id} sh"),
        });
    }
    let mut argv: Vec<String> = target
        .cli(podbag, Some(docker))
        .ok_or("No CLI for this engine")?
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    argv.extend(tail);
    let quoted: Vec<_> = argv
        .iter()
        .map(|arg| {
            if cfg!(windows) {
                format!("'{}'", arg.replace('\'', "''"))
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect();
    let line = quoted.join(" ");
    Ok(Shell {
        argv,
        by_hand: if cfg!(windows) {
            format!("& {line}")
        } else {
            line
        },
    })
}

/// Open a terminal window with a shell in the container. When none can be opened, the
/// error is the command to run by hand.
pub fn open_shell(shell: &Shell) -> Result<(), String> {
    if open_terminal(Some(&shell.argv)) {
        Ok(())
    } else {
        Err(shell.by_hand.clone())
    }
}

/// Open a terminal window that runs `argv` (None: a plain shell). False when no
/// terminal app was found.
pub fn open_terminal(argv: Option<&[String]>) -> bool {
    let Some(argv) = argv else {
        return if cfg!(target_os = "macos") {
            std::process::Command::new("open")
                .args(["-a", "Terminal"])
                .spawn()
                .is_ok()
        } else if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "start", "cmd"])
                .spawn()
                .is_ok()
        } else {
            linux_terminal(&[])
        };
    };
    let quoted: Vec<String> = argv
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect();
    let line = quoted.join(" ");
    if cfg!(target_os = "macos") {
        let script = format!(
            "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
            line.replace('\\', "\\\\").replace('"', "\\\"")
        );
        std::process::Command::new("osascript")
            .args(["-e", &script])
            .spawn()
            .is_ok()
    } else if cfg!(windows) {
        {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .creation_flags(0x10)
                    .spawn()
                    .is_ok()
            }
            #[cfg(not(windows))]
            {
                false
            }
        }
    } else {
        linux_terminal(argv)
    }
}

/// The first terminal found: $TERMINAL, then the common ones.
fn linux_terminal(argv: &[String]) -> bool {
    let mut candidates: Vec<(String, &str)> = Vec::new();
    if let Ok(t) = std::env::var("TERMINAL") {
        candidates.push((t, "-e"));
    }
    for (t, flag) in [
        ("x-terminal-emulator", "-e"),
        ("gnome-terminal", "--"),
        ("konsole", "-e"),
        ("xfce4-terminal", "-x"),
        ("kitty", "--"),
        ("alacritty", "-e"),
        ("foot", "--"),
        ("xterm", "-e"),
    ] {
        candidates.push((t.to_string(), flag));
    }
    candidates.into_iter().any(|(t, flag)| {
        let mut cmd = std::process::Command::new(&t);
        if !argv.is_empty() {
            cmd.arg(flag).args(argv);
        }
        cmd.spawn().is_ok()
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use podbag::client::Endpoint;

    fn docker_at(path: impl Into<std::path::PathBuf>) -> Target {
        Target {
            choice: Choice::Docker,
            endpoint: Endpoint::Socket {
                path: path.into(),
                own: false,
            },
        }
    }

    #[test]
    fn docker_shells_use_the_docker_cli_and_its_socket() {
        let target = docker_at("/x/docker.sock");
        let shell = shell_command(
            &target,
            Path::new("/opt/podbag"),
            Path::new("/usr/bin/docker"),
            "web",
        )
        .unwrap();
        assert_eq!(
            shell.argv,
            [
                "/usr/bin/docker",
                "--host=unix:///x/docker.sock",
                "exec",
                "-it",
                "web",
                "sh"
            ]
        );
        assert!(shell.by_hand.contains("--host=unix:///x/docker.sock"));
        let podbag = Target {
            choice: Choice::Podbag,
            endpoint: Endpoint::Socket {
                path: "/p.sock".into(),
                own: true,
            },
        };
        let shell = shell_command(
            &podbag,
            Path::new("/opt/podbag"),
            Path::new("docker"),
            "web",
        )
        .unwrap();
        assert_eq!(shell.argv, ["/opt/podbag", "exec", "-it", "web", "sh"]);
        assert_eq!(shell.by_hand, "podbag exec -it web sh");
    }

    #[test]
    fn shell_commands_take_only_plain_ids() {
        let t = Target {
            choice: Choice::Podbag,
            endpoint: Endpoint::Socket {
                path: "/p.sock".into(),
                own: true,
            },
        };
        let docker = Path::new("docker");
        let shell = shell_command(&t, Path::new("/usr/local/bin/podbag"), docker, "web-1").unwrap();
        assert_eq!(
            shell.argv,
            ["/usr/local/bin/podbag", "exec", "-it", "web-1", "sh"]
        );
        assert!(shell_command(&t, Path::new("podbag"), docker, "web; rm -rf /").is_err());
        assert!(shell_command(&t, Path::new("podbag"), docker, "").is_err());
    }
}
