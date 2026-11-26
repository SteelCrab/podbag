use crate::args::LabCmd;
use podbag::volume_lesson::{self, Module};
#[cfg(unix)]
mod containers;
#[cfg(unix)]
use std::path::Path;
use std::{fs, io};

pub fn command(cmd: LabCmd) -> i32 {
    let result = (|| {
        match cmd {
            #[cfg(unix)]
            LabCmd::Workspace { containers: true } => return containers::workspace(),
            #[cfg(unix)]
            LabCmd::Workspace { containers: false } => return workspace(),
            #[cfg(unix)]
            LabCmd::DesktopView {
                library,
                action,
                session,
                name,
                containers,
                course,
            } => {
                if course {
                    let snapshot = volume_lesson::course::view(
                        &library,
                        &action,
                        session.as_deref(),
                        name.as_deref().unwrap_or(""),
                    )?;
                    println!("{}", serde_json::to_string(&snapshot)?);
                    return Ok(());
                }
                if containers {
                    let snapshot = volume_lesson::containers::view(
                        &library,
                        &action,
                        session.as_deref(),
                        name.as_deref().unwrap_or(""),
                    )?;
                    println!("{}", serde_json::to_string(&snapshot)?);
                    return Ok(());
                }
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
                let module = volume_lesson::install(&volume_lesson::directory()?, &text)?;
                println!("Installed. Run podbag lab start {}.", module.id());
            }
            LabCmd::List => {
                let dir = volume_lesson::directory()?;
                refresh(&dir);
                for record in volume_lesson::list(&dir)? {
                    let module = Module::of(&record.package)?;
                    println!(
                        "{}	{}	{}",
                        module.id(),
                        if record.completed {
                            "Complete"
                        } else {
                            "Installed"
                        },
                        record.words("en").title
                    );
                }
            }
            LabCmd::Remove { module } => {
                let dir = volume_lesson::directory()?;
                volume_lesson::remove(&dir, &volume_lesson::selected(&dir, module.as_deref())?)?;
                println!("Module removed.");
            }
            LabCmd::Start {
                module,
                lang,
                library,
            } => {
                let dir = match library {
                    Some(dir) => dir,
                    None => volume_lesson::directory()?,
                };
                if module
                    .as_deref()
                    .is_some_and(|id| volume_lesson::needs_sync(&dir, id))
                {
                    refresh(&dir);
                }
                let module = volume_lesson::selected(&dir, module.as_deref())?;
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
                            module.id().into(),
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
                    let installed = volume_lesson::read(&dir, &module)?.ok_or_else(|| {
                        io::Error::other("Install the module with podbag lab install FILE first.")
                    })?;
                    let course = match module {
                        Module::Course(_) => Some(installed.course()?),
                        _ => None,
                    };
                    let words = installed.words(&lang);
                    let guide = installed.guide_url(&lang);
                    if session(words, guide, &dir, &module, course)? {
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

/// Syncs the Training catalog; offline, the installed modules stay as they are.
fn refresh(dir: &std::path::Path) {
    match volume_lesson::catalog::sync(dir) {
        Ok(synced) => {
            for (id, why) in synced.failed {
                eprintln!("podbag: Training module {id} was not updated: {why}");
            }
        }
        Err(e) => eprintln!("podbag: could not refresh the Training list: {e}"),
    }
}

#[cfg(unix)]
fn session(
    words: &volume_lesson::Words,
    guide: Option<String>,
    library: &Path,
    module: &Module,
    course: Option<volume_lesson::course::Course>,
) -> io::Result<bool> {
    use std::io::{BufRead, BufReader, IsTerminal};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if !io::stdin().is_terminal() {
        return Err(io::Error::other("Open the lab in an interactive terminal."));
    }
    let container_lab = *module == Module::Containers || course.is_some();
    let prompt = if course.is_some() {
        "training $ "
    } else if container_lab {
        "container-lab $ "
    } else {
        "volume-lab $ "
    };
    let mut command = Command::new(std::env::current_exe()?);
    command.args(["lab", "workspace"]);
    if container_lab {
        command.arg("--containers");
    }
    let mut owner = command
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
        if let Some(course) = &course {
            volume_lesson::course::Attempt::start(dir, course)?;
        }
        registration = match module {
            Module::Course(_) => Some(volume_lesson::gui::Registration::open_for(
                library,
                dir,
                volume_lesson::course::ACTIVE,
            )?),
            Module::Containers => Some(volume_lesson::gui::Registration::open_for(
                library,
                dir,
                volume_lesson::containers::ACTIVE,
            )?),
            Module::Volumes => Some(volume_lesson::gui::Registration::open(library, dir)?),
            Module::Tui => None,
        };
        if let Some(url) = &guide {
            println!("\n{}\n{url}\n", words.title);
        } else {
            println!("\n{}\n", words.guide);
        }
        if Path::new("/bin/sh").is_file() {
            practice_command(Path::new("/bin/sh"), dir)?
                .arg("-i")
                .env("ENV", "/dev/null")
                .env("HISTFILE", "/dev/null")
                .env("PS1", prompt)
                .status()?;
        } else {
            terminal_without_shell(dir, prompt)?;
        }
        if course.is_some() {
            return Ok(volume_lesson::course::Attempt::read(dir)?
                .step()?
                .is_finish());
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
fn terminal_without_shell(dir: &Path, prompt: &str) -> io::Result<()> {
    use std::io::Write;
    let exe = std::env::current_exe()?;
    loop {
        print!("{prompt}");
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
            Some(_) => {
                eprintln!("Run the podbag commands shown in the practice guide, or type exit.")
            }
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
    if root.join("run/container-lab").is_file() {
        let result = if volume_lesson::course::Attempt::active(root) {
            containers::course_command(root)
        } else {
            containers::scoped(root, cmd)
        };
        return match result {
            Ok(code) => code,
            Err(e) => {
                eprintln!("podbag: {e}");
                1
            }
        };
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
