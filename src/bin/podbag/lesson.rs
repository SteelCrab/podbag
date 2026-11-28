use crate::args::LabCmd;
use podbag::volume_lesson::{self, Module};
#[cfg(unix)]
mod containers;
#[cfg(unix)]
use std::path::Path;
use std::{fs, io};

/// The module's title, for the TUI panel. Practice sessions are unix only.
#[cfg(unix)]
pub const TITLE: &str = "PODBAG_TRAINING_TITLE";

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
                tui,
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
                let tui = tui_track(&module, tui)?;
                let lang = lang
                    .or_else(|| std::env::var("LANG").ok())
                    .unwrap_or_else(|| "en".into());
                #[cfg(windows)]
                {
                    let path =
                        podbag::wsl::wsl_path(&dir.to_string_lossy()).map_err(io::Error::other)?;
                    let mut args: Vec<String> = vec![
                        "lab".into(),
                        "start".into(),
                        module.id().into(),
                        "--lang".into(),
                        lang,
                        "--library".into(),
                        path,
                    ];
                    if tui {
                        args.push("--tui".into());
                    }
                    let code = podbag::windows::forward(&args, &[]);
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
                    if session(words, guide, &dir, &module, course, tui)? {
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

/// Whether `--tui` can start MODULE: course modules only.
fn tui_track(module: &Module, tui: bool) -> io::Result<bool> {
    if tui && !matches!(module, Module::Course(_)) {
        return Err(io::Error::other("The TUI track is for course modules."));
    }
    Ok(tui)
}

/// The TUI in a course session: (once, mouse) for `podbag tui` without --engine.
#[cfg(unix)]
fn course_tui(cmd: &crate::args::Cmd) -> Option<(bool, bool)> {
    match *cmd {
        crate::args::Cmd::Tui {
            once,
            no_mouse,
            engine: None,
        } => Some((once, !no_mouse)),
        _ => None,
    }
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
            format!(
                "unix://{}",
                volume_lesson::containers::socket(dir).display()
            ),
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
    tui: bool,
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
        // Quitting the TUI ends the session the same way `exit` ends the shell.
        if tui {
            practice_command(&std::env::current_exe()?, dir)?
                .arg("tui")
                .env(TITLE, &words.title)
                .status()?;
        } else {
            shell(dir, prompt, Some(&words.title))?;
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

/// The practice shell in DIR with PROMPT: /bin/sh -i, or the line reader without it.
/// PROMPT is the one `session` picks (`training $ `, `container-lab $ `,
/// `volume-lab $ `), so the legacy labs keep their prompts. Unix only, like `session`.
/// Its commands get this process's `TITLE`, as in the TUI that `session` started.
#[cfg(unix)]
pub(crate) fn training_shell(dir: &Path, prompt: &str) -> io::Result<()> {
    shell(dir, prompt, None)
}

/// `training_shell` with TITLE set for its commands, when given.
#[cfg(unix)]
fn shell(dir: &Path, prompt: &str, title: Option<&str>) -> io::Result<()> {
    if Path::new("/bin/sh").is_file() {
        let mut sh = practice_command(Path::new("/bin/sh"), dir)?;
        if let Some(title) = title {
            sh.env(TITLE, title);
        }
        sh.arg("-i")
            .env("ENV", "/dev/null")
            .env("HISTFILE", "/dev/null")
            .env("PS1", prompt)
            .status()?;
        return Ok(());
    }
    terminal_without_shell(dir, prompt, title)
}

/// The Podbag WSL distribution contains just the engine. Run each command as a real
/// child on the attached console so the normal TUI can take over the terminal.
#[cfg(unix)]
fn terminal_without_shell(dir: &Path, prompt: &str, title: Option<&str>) -> io::Result<()> {
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
                let mut cmd = practice_command(&exe, dir)?;
                if let Some(title) = title {
                    cmd.env(TITLE, title);
                }
                cmd.args(args).status()?;
            }
            Some(_) => {
                eprintln!("Run the podbag commands shown in the practice guide, or type exit.")
            }
            None => {}
        }
    }
}

/// `podbag lab docker-step ARGS` from a course's `bin/docker`.
#[cfg(unix)]
pub fn docker_step(root: &Path, args: Vec<std::ffi::OsString>) -> i32 {
    let result = if root.is_absolute() && volume_lesson::course::Attempt::active(root) {
        containers::docker_step(root, args)
    } else {
        Err(io::Error::other(
            "This practice session has no Training course.",
        ))
    };
    result.unwrap_or_else(|e| {
        eprintln!("podbag: {e}");
        1
    })
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
        let course = volume_lesson::course::Attempt::active(root);
        if course && let Some((once, mouse)) = course_tui(&cmd) {
            return crate::tui::run(
                once,
                Some(Choice::Socket(volume_lesson::containers::socket(root))),
                mouse,
            );
        }
        let result = if course {
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
                    buildkit: false,
                },
        } if path == socket => {
            // The supervising terminal owns the pipe. Its disappearance also stops the engine.
            std::thread::spawn(|| {
                let _ = io::copy(&mut io::stdin(), &mut io::sink());
                std::process::exit(0);
            });
            crate::service::service(Some(socket), 0, false, false)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_course_modules_have_a_tui_track() {
        assert!(tui_track(&Module::Course("web-basics".into()), true).unwrap());
        assert!(!tui_track(&Module::Containers, false).unwrap());
        assert_eq!(
            tui_track(&Module::Containers, true)
                .unwrap_err()
                .to_string(),
            "The TUI track is for course modules."
        );
    }

    #[test]
    fn a_course_shell_opens_only_the_practice_tui() {
        let parsed = |a: &[&str]| {
            crate::parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .unwrap()
                .cmd
        };
        assert_eq!(course_tui(&parsed(&["tui"])), Some((false, true)));
        assert_eq!(
            course_tui(&parsed(&["tui", "--no-mouse"])),
            Some((false, false))
        );
        // Never another engine.
        assert_eq!(course_tui(&parsed(&["tui", "--engine", "docker"])), None);
        assert_eq!(course_tui(&parsed(&["ps"])), None);
    }
}
