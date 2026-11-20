use crate::args::LabCmd;
use podbag::volume_lesson;
#[cfg(unix)]
use std::path::Path;
use std::{fs, io};

pub fn command(cmd: LabCmd) -> i32 {
    let result = (|| {
        match cmd {
            #[cfg(unix)]
            LabCmd::Workspace => return workspace(),
            #[cfg(unix)]
            LabCmd::DesktopView {
                library,
                action,
                session,
                name,
            } => {
                let snapshot = volume_lesson::gui::view(
                    &library,
                    &action,
                    session.as_deref(),
                    name.as_deref().unwrap_or(""),
                )?;
                println!("{}", serde_json::to_string(&snapshot)?);
            }
            LabCmd::Install { file } => {
                use std::io::Read;
                let mut text = String::new();
                fs::File::open(file)?
                    .take(volume_lesson::LIMIT as u64 + 1)
                    .read_to_string(&mut text)?;
                volume_lesson::install(&volume_lesson::directory()?, &text)?;
                println!("Installed. Run podbag lab start.");
            }
            LabCmd::Remove => {
                volume_lesson::remove(&volume_lesson::directory()?)?;
                println!("Module removed.");
            }
            LabCmd::Start { lang, library } => {
                let dir = match library {
                    Some(dir) => dir,
                    None => volume_lesson::directory()?,
                };
                let lang = lang
                    .or_else(|| std::env::var("LANG").ok())
                    .unwrap_or_else(|| "en".into());
                #[cfg(windows)]
                {
                    let path =
                        podbag::wsl::wsl_path(&dir.to_string_lossy()).map_err(io::Error::other)?;
                    let code = podbag::windows::forward(
                        &[
                            "lab".into(),
                            "start".into(),
                            "--lang".into(),
                            lang,
                            "--library".into(),
                            path,
                        ],
                        &[],
                    );
                    if code != 0 {
                        return Err(io::Error::other("The lab did not finish in WSL."));
                    }
                }
                #[cfg(unix)]
                {
                    let installed = volume_lesson::read(&dir)?.ok_or_else(|| {
                        io::Error::other("Install the module with podbag lab install FILE first.")
                    })?;
                    if session(
                        installed.words(&lang),
                        (installed.package.format == volume_lesson::gui::FORMAT)
                            .then_some(dir.as_path()),
                    )? {
                        installed.finish(&dir)?;
                    }
                }
            }
        }
        Ok::<_, io::Error>(())
    })();
    if let Err(e) = result {
        eprintln!("podbag: {e}");
        return 1;
    }
    0
}

#[cfg(unix)]
fn practice_command(program: &Path, dir: &Path) -> io::Result<std::process::Command> {
    let exe = std::env::current_exe()?;
    let mut paths = vec![dir.join("bin")];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    let mut cmd = std::process::Command::new(program);
    cmd.current_dir(dir)
        .env(volume_lesson::SCOPE, dir)
        .env("PODBAG_BIN", exe)
        .env("PODBAG_DATA_DIR", dir.join("data"))
        .env("PODBAG_RUNTIME_DIR", dir.join("run"))
        .env("PODBAG_CONFIG_DIR", dir.join("config"))
        .env("PODBAG_UI_DIR", dir.join("ui"))
        .env(
            "DOCKER_HOST",
            format!("unix://{}", dir.join("api.sock").display()),
        )
        .env("DOCKER_CONFIG", dir.join("docker"))
        .env(
            "PATH",
            std::env::join_paths(paths).map_err(io::Error::other)?,
        )
        .env_remove("LISTEN_PID")
        .env_remove("LISTEN_FDS");
    Ok(cmd)
}

/// This helper owns the directory and service. Closing the parent terminal closes its
/// pipe, even when the launcher never gets a chance to run its destructors.
#[cfg(unix)]
fn workspace() -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let root = tempfile::Builder::new()
        .prefix("podbag-volume-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")?;
    let dir = root.path();
    fs::create_dir(dir.join("bin"))?;
    fs::create_dir(dir.join("run"))?;
    let exe = std::env::current_exe()?;
    symlink(&exe, dir.join("bin/podbag"))?;
    let socket = dir.join("api.sock");
    let mut service = practice_command(&exe, dir)?
        .args(["system", "service", "--socket"])
        .arg(&socket)
        .args(["--timeout", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    let serving = (|| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            if service.try_wait()?.is_some() || Instant::now() >= deadline {
                return Err(io::Error::other("The practice engine did not start."));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let mut out = io::stdout().lock();
        writeln!(out, "{}", dir.display())?;
        out.flush()?;
        io::copy(&mut io::stdin().lock(), &mut io::sink())?;
        Ok(())
    })();
    drop(service.stdin.take());
    let _ = service.kill();
    service.wait()?;
    root.close()?;
    serving
}

#[cfg(unix)]
fn session(words: &volume_lesson::Words, library: Option<&Path>) -> io::Result<bool> {
    use std::io::{BufRead, BufReader, IsTerminal};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if !io::stdin().is_terminal() {
        return Err(io::Error::other("Open the lab in an interactive terminal."));
    }
    let mut owner = Command::new(std::env::current_exe()?)
        .args(["lab", "workspace"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let mut registration = None;
    let result = (|| {
        let mut line = String::new();
        if BufReader::new(owner.stdout.take().unwrap()).read_line(&mut line)? == 0 {
            return Err(io::Error::other("The practice engine did not start."));
        }
        let dir = Path::new(line.trim_end());
        if let Some(library) = library {
            registration = Some(volume_lesson::gui::Registration::open(library, dir)?);
        }
        if let Some(intro) = &words.intro {
            println!("\n{}\n{}", intro.title, intro.text);
        }
        println!("\n{}\n", words.guide);
        if Path::new("/bin/sh").is_file() {
            practice_command(Path::new("/bin/sh"), dir)?
                .arg("-i")
                .env("ENV", "/dev/null")
                .env("HISTFILE", "/dev/null")
                .env("PS1", "volume-lab $ ")
                .status()?;
        } else {
            terminal_without_shell(dir)?;
        }
        Ok(dir.join("verified").is_file())
    })();
    drop(owner.stdin.take());
    if !owner.wait()?.success() {
        return Err(io::Error::other(
            "Could not finish cleaning the practice store.",
        ));
    }
    drop(registration);
    let complete = result?;
    println!(
        "{}",
        if complete {
            "Lab complete. Practice resources removed."
        } else {
            "Lab ended. Practice resources removed."
        }
    );
    Ok(complete)
}

/// The Podbag WSL distribution contains just the engine. Run each command as a real
/// child on the attached console so the normal TUI can take over the terminal.
#[cfg(unix)]
fn terminal_without_shell(dir: &Path) -> io::Result<()> {
    use std::io::Write;
    let exe = std::env::current_exe()?;
    loop {
        print!("volume-lab $ ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 || line.trim() == "exit" {
            return Ok(());
        }
        let Some(args) = shlex::split(&line) else {
            eprintln!("Close the quoted argument.");
            continue;
        };
        match args.split_first() {
            Some((name, args)) if name == "podbag" => {
                practice_command(&exe, dir)?.args(args).status()?;
            }
            Some(_) => eprintln!("Run podbag volume, podbag tui, or exit."),
            None => {}
        }
    }
}

#[cfg(unix)]
pub fn scoped(root: &Path, cmd: crate::args::Cmd) -> i32 {
    use crate::args::{Cmd, SystemCmd, VolumeCmd};
    use podbag::{Volume, client::Choice};
    let socket = root.join("api.sock");
    if !root.is_absolute()
        || !root.is_dir()
        || !podbag::paths::dirs().is_ok_and(|d| {
            d.data == root.join("data")
                && d.runtime == root.join("run")
                && d.config == root.join("config")
        })
    {
        eprintln!("podbag: this lab session is closed or its environment has changed.");
        return 1;
    }
    match cmd {
        Cmd::System {
            cmd:
                SystemCmd::Service {
                    socket: Some(path),
                    timeout: 0,
                    print_units: false,
                },
        } if path == socket => {
            // The supervising terminal owns the pipe. Its disappearance also stops the engine.
            std::thread::spawn(|| {
                let _ = io::copy(&mut io::stdin(), &mut io::sink());
                std::process::exit(0);
            });
            crate::service::service(Some(socket), 0, false)
        }
        Cmd::Volume { cmd } => {
            let check = matches!(cmd, VolumeCmd::Ls { .. });
            let create = matches!(&cmd, VolumeCmd::Create { name: Some(n), .. } if n == "my-api")
                && Volume::get("my-api").is_err_and(|e| e.kind == podbag::ErrorKind::NotFound);
            let result = crate::volume::run(cmd);
            if result == 0 && create {
                for marker in ["run/removed", "run/gui-inspected", "run/gui-removed"] {
                    let _ = fs::remove_file(root.join(marker));
                }
                let _ = fs::remove_file(root.join("verified"));
            }
            if result == 0
                && check
                && root
                    .join(if root.join("run/gui").exists() {
                        "run/gui-removed"
                    } else {
                        "run/removed"
                    })
                    .is_file()
                && Volume::list().is_ok_and(|volumes| volumes.iter().all(|v| v.name != "my-api"))
            {
                if let Err(e) = fs::write(root.join("verified"), []) {
                    eprintln!("podbag: {e}");
                    return 1;
                }
                eprintln!("Verified. Type exit to finish the lab.");
            }
            result
        }
        Cmd::Tui {
            once,
            no_mouse,
            engine: None,
        } => crate::tui::run(once, Some(Choice::Socket(socket)), !no_mouse),
        _ => {
            eprintln!(
                "podbag: this practice shell supports podbag volume and podbag tui. Type exit to leave."
            );
            1
        }
    }
}
