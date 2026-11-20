//! Running the Docker tools (the docker CLI, credential helpers, `podbag login`). Tests
//! give a fake runner instead, so they never touch the real `~/.docker`.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// What a program did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ran {
    /// It exited with status 0.
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Ran {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    /// The error text: stderr, else stdout, else `what failed`.
    pub fn error(&self, what: &str) -> String {
        let text = String::from_utf8_lossy(&self.stderr).trim().to_string();
        if !text.is_empty() {
            return text;
        }
        let text = self.stdout_text();
        if !text.is_empty() {
            return text;
        }
        format!("{what} failed")
    }
}

/// Runs `program args…` with `stdin` (if any) and waits for it.
pub type Run = dyn Fn(&Path, &[&str], Option<&[u8]>) -> io::Result<Ran>;

/// The real runner. `DOCKER_HOST` and `DOCKER_CONTEXT` are removed, so the docker CLI
/// reads and changes its saved context, not this program's environment.
pub fn system(program: &Path, args: &[&str], stdin: Option<&[u8]>) -> io::Result<Ran> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console window flashes up.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn()?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // Small inputs (a server name, a password): they fit the pipe.
        pipe.write_all(data)?;
    }
    let out = child.wait_with_output()?;
    Ok(Ran {
        ok: out.status.success(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}
