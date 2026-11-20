use super::practice_command;
use crate::args::Cmd;
use podbag::volume_lesson::{self, containers as lab};
use std::{
    fs,
    io::{self, Write},
    path::Path,
    process::{Command, Stdio},
};

fn engine_command(root: &Path) -> io::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut cmd = practice_command(&exe, root)?;
    cmd.env_remove(volume_lesson::SCOPE);
    #[cfg(target_os = "macos")]
    {
        // Keep the normal VM and its disk separate. Reuse only the read-only guest files.
        let guest = podbag::macos::host::paths()
            .map_err(io::Error::other)?
            .guest;
        cmd.env("PODBAG_VM_GUEST", guest)
            .env("PODBAG_MAC_HOME", root)
            .env("PODBAG_VM_CPUS", "2")
            .env("PODBAG_VM_MEMORY", "768")
            .env("PODBAG_VM_IDLE", "0");
    }
    Ok(cmd)
}

fn ready(root: &Path) -> io::Result<()> {
    fs::write(root.join("run/container-lab"), [])?;
    let mut out = io::stdout().lock();
    writeln!(out, "{}", root.display())?;
    out.flush()?;
    drop(out);
    let result = io::copy(&mut io::stdin().lock(), &mut io::sink());
    fs::write(root.join("run/closing"), [])?;
    result.map(drop)
}

pub fn workspace() -> io::Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::Builder::new()
        .prefix(lab::PREFIX)
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")?;
    let dir = root.path();
    fs::create_dir(dir.join("bin"))?;
    fs::create_dir(dir.join("run"))?;
    symlink(std::env::current_exe()?, dir.join("bin/podbag"))?;
    let (result, cleanup) = serve(dir)?;
    if let Err(e) = cleanup {
        return Err(io::Error::other(format!(
            "Could not clean {}: {e}",
            root.keep().display()
        )));
    }
    root.close()?;
    result
}

#[cfg(target_os = "macos")]
fn serve(root: &Path) -> io::Result<(io::Result<()>, io::Result<()>)> {
    let result = engine_command(root)?
        .args(["vm", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .and_then(|status| {
            if !status.success() {
                return Err(io::Error::other("Could not start the practice VM."));
            }
            ready(root)
        });
    let cleanup = engine_command(root)?
        .args(["vm", "stop"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other("Could not stop the practice VM."))
            }
        });
    Ok((result, cleanup))
}

#[cfg(not(target_os = "macos"))]
fn serve(root: &Path) -> io::Result<(io::Result<()>, io::Result<()>)> {
    use std::time::{Duration, Instant};
    let socket = root.join("api.sock");
    let mut service = engine_command(root)?
        .args(["system", "service", "--socket"])
        .arg(&socket)
        .args(["--timeout", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()?;
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            if service.try_wait()?.is_some() || Instant::now() >= deadline {
                return Err(io::Error::other("Could not start the practice engine."));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        ready(root)
    })();
    let cleanup = (|| {
        let client = lab::client(root);
        if socket.exists() {
            for row in client.containers().map_err(io::Error::other)? {
                client
                    .remove(podbag::client::Kind::Containers, &row.id, true)
                    .map_err(io::Error::other)?;
            }
        }
        Ok(())
    })();
    let _ = service.kill();
    service.wait()?;
    Ok((result, cleanup))
}

pub fn scoped(root: &Path, cmd: Cmd) -> io::Result<i32> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let verify = match cmd {
        Cmd::Run(_)
            if args
                .iter()
                .map(|arg| arg.to_str().unwrap_or(""))
                .eq(lab::START.iter().copied()) =>
        {
            if lab::client(root)
                .containers()
                .map_err(io::Error::other)?
                .iter()
                .any(|c| c.name() == lab::NAME)
            {
                return Err(io::Error::other(
                    "hello-web already exists. Continue in Practice containers.",
                ));
            }
            false
        }
        Cmd::Ps { all, quiet } => all && !quiet,
        _ => {
            return Err(io::Error::other(
                "Use the commands shown in the container lab, or type exit.",
            ));
        }
    };
    // Observe the stopped state before running ps so an overlapping GUI stop
    // cannot finish a check whose output still showed a running container.
    let checked = verify
        && lab::client(root)
            .containers()
            .map_err(io::Error::other)?
            .iter()
            .any(|row| row.name() == lab::NAME && lab::stopped_in_desktop(root, row));
    let status = engine_command(root)?.args(args).status()?;
    if checked && status.success() {
        fs::write(root.join("verified"), [])?;
        eprintln!("Verified. Type exit to finish the lab.");
    }
    Ok(status.code().unwrap_or(1))
}

pub fn course_command(root: &Path) -> io::Result<i32> {
    use volume_lesson::course::{Attempt, Step};
    let _lock = Attempt::lock(root)?;
    let mut attempt = Attempt::read(root)?;
    let step = attempt.step()?;
    let line = step
        .command(attempt.port)
        .and_then(|line| shlex::split(&line));
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !line.as_ref().is_some_and(|line| {
        args.iter()
            .eq(line.iter().skip(1).map(std::ffi::OsStr::new))
    }) {
        return Err(io::Error::other(
            "Use the command shown for the current Training step, or type exit.",
        ));
    }
    if matches!(step, Step::RunWeb | Step::RunFirst) && attempt.refresh_port(root)? {
        return Err(io::Error::other(format!(
            "That port is in use. Copy the updated Training command for port {}.",
            attempt.port
        )));
    }
    let mut engine = engine_command(root)?;
    engine.args(args);
    let (status, output) = if step == Step::Read {
        let result = engine.stderr(Stdio::inherit()).output()?;
        io::stdout().write_all(&result.stdout)?;
        (result.status, result.stdout)
    } else {
        (engine.status()?, Vec::new())
    };
    if !status.success() {
        return Ok(status.code().unwrap_or(1));
    }
    step.verify_cli(root, &output)?;
    attempt.advance(root)?;
    if attempt.step()? == Step::Finish {
        eprintln!("Verified. Type exit to finish the lab.");
    }
    Ok(0)
}
