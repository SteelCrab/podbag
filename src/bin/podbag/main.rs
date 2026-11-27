mod args;
#[cfg(unix)]
mod build;
mod check;
#[cfg(unix)]
mod container;
mod control;
mod dockerctx;
mod image;
mod lesson;
#[cfg(unix)]
mod manage;
mod network;
mod preferences;
mod registry;
#[cfg(unix)]
mod run;
#[cfg(unix)]
mod service;
mod tui;
mod volume;

use args::{Cli, Cmd};
use clap::Parser;

fn migrate(
    from: podbag::client::Choice,
    to: podbag::client::Choice,
    kind: &str,
    name: &str,
    destination_defaults: bool,
) -> i32 {
    use podbag::client::{Client, Kind, Target, migration, podbag_program};
    let connect =
        |choice| Target::find(Some(choice), &podbag_program()).map(|t| Client::new(t.endpoint));
    let kind = match kind {
        "container" => Kind::Containers,
        "image" => Kind::Images,
        "volume" => Kind::Volumes,
        _ => return 2,
    };
    let result = (|| {
        migration::migrate(
            &connect(&from)?,
            &connect(&to)?,
            kind,
            name,
            destination_defaults,
            |phase| eprintln!("{phase}"),
        )
    })();
    match result {
        Ok(()) => {
            println!("Verified {name} on {}", to.label());
            0
        }
        Err(e) => {
            eprintln!("podbag: {e}");
            1
        }
    }
}

fn main() {
    // A login a front end (macOS, Windows) sent with the command: kept in memory, gone
    // from the environment before any thread or child process starts.
    // SAFETY: the first thing in main; no other thread runs yet.
    unsafe { podbag::image::auth::take_front_end_logins() };
    // PID 1 of the macOS VM: podbag is its initramfs's /init, which the kernel starts as
    // `/init __vm-init`. Anywhere else (a container's entry point, `unshare -pf`), PID 1
    // is a CLI like any other.
    #[cfg(target_os = "linux")]
    if std::process::id() == 1
        && std::env::args_os()
            .nth(1)
            .is_some_and(|a| a == podbag::vm::proto::INIT_ARG)
    {
        podbag::vm::guest::vm_init_main();
    }
    #[cfg(unix)]
    if let Some(root) = std::env::var_os(podbag::volume_lesson::SCOPE) {
        let root = std::path::Path::new(&root);
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.len() >= 2 && args[0] == "lab" && args[1] == "docker-step" {
            std::process::exit(lesson::docker_step(root, args[2..].to_vec()));
        }
        std::process::exit(lesson::scoped(root, Cli::parse().cmd));
    }
    #[cfg(target_os = "macos")]
    if let Some(code) = macos_front(std::env::args().skip(1).collect()) {
        std::process::exit(code);
    }
    #[cfg(windows)]
    if let Some(code) = windows_front(std::env::args().skip(1).collect()) {
        std::process::exit(code);
    }
    let cli = Cli::parse();
    std::process::exit(dispatch(cli.cmd));
}

/// On Windows, commands run in podbag's WSL2 distribution; only `vm …`, help and the
/// version stay here.
#[cfg(windows)]
fn windows_front(args: Vec<String>) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("tui") => {
            // A host TUI can run Windows tools. Unix sockets are reached inside WSL.
            let cli = match parse(&args) {
                Ok(cli) => cli,
                Err(e) => e.exit(),
            };
            if let Cmd::Tui { engine, .. } = cli.cmd {
                let requested = engine.or_else(podbag::client::choice::saved).or_else(|| {
                    std::env::var("DOCKER_HOST")
                        .ok()
                        .filter(|v| v.starts_with("unix://"))
                        .and_then(|v| podbag::client::Choice::parse(&v))
                });
                if let Some(choice @ podbag::client::Choice::Socket(_)) = requested {
                    let mut args = args;
                    if !args
                        .iter()
                        .any(|a| a == "--engine" || a.starts_with("--engine="))
                    {
                        args.extend(["--engine".into(), choice.to_setting()]);
                    }
                    return Some(podbag::windows::forward(&args, &[]));
                }
            }
            None
        }
        // podbag.exe itself is replaced; the next command updates the engine in WSL.
        None | Some("-h" | "--help" | "help" | "-V" | "--version" | "upgrade") => None,
        // Windows' docker CLI and its contexts.
        Some("system") if args.get(1).is_some_and(|a| a == "docker") => None,
        // Checked here first: help and wrong arguments need no WSL.
        Some(_) => match parse(&args) {
            Err(e) => e.exit(),
            // Logins stay on Windows: in its config and credential manager.
            Ok(cli) if runs_here(&cli) => None,
            Ok(cli) => Some(match wsl_argv(&cli, args) {
                Ok(argv) => podbag::windows::forward(&argv, &forwarded_logins(&cli)),
                Err(e) => {
                    eprintln!("podbag: {e}");
                    125
                }
            }),
        },
    }
}

/// The arguments with the Windows paths they name (`-v`, `--rootfs`, `load -i`,
/// `save -o`) as WSL sees them. Only as many values as clap read are changed, the first
/// ones in the arguments: those come before the image and the command it runs.
#[cfg(any(windows, test))]
fn wsl_argv(cli: &Cli, mut argv: Vec<String>) -> Result<Vec<String>, String> {
    use podbag::wsl::{wsl_path, wsl_volume};
    match &cli.cmd {
        Cmd::Run(a) | Cmd::Create(a) => {
            rewrite(&mut argv, Some('v'), "--volume", a.volume.len(), wsl_volume)?;
            let rootfs = usize::from(a.rootfs.is_some());
            rewrite(&mut argv, None, "--rootfs", rootfs, wsl_path)?;
        }
        Cmd::Load { input: Some(_), .. } => rewrite(&mut argv, Some('i'), "--input", 1, wsl_path)?,
        Cmd::Save {
            output: Some(_), ..
        }
        | Cmd::Export {
            output: Some(_), ..
        } => rewrite(&mut argv, Some('o'), "--output", 1, wsl_path)?,
        Cmd::Build(a) => {
            rewrite(
                &mut argv,
                Some('f'),
                "--file",
                usize::from(a.file.is_some()),
                wsl_path,
            )?;
            // The context is the one positional argument: the last word that is it.
            if a.context != "-"
                && let Some(i) = argv.iter().rposition(|w| *w == a.context)
            {
                argv[i] = wsl_path(&a.context)?;
            }
        }
        _ => {}
    }
    Ok(argv)
}

/// Change the values of the first `count` uses of an option: `--long=VALUE`,
/// `--long VALUE`, `-s VALUE`, or flags without values ending in it (`-itv VALUE`).
#[cfg(any(windows, test))]
fn rewrite(
    argv: &mut [String],
    short: Option<char>,
    long: &str,
    mut count: usize,
    change: fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    let takes_next = |a: &str| {
        a == long
            || short.is_some_and(|s| {
                a.strip_prefix('-').is_some_and(|f| {
                    !f.starts_with('-')
                        && f.ends_with(s)
                        && f[..f.len() - 1].chars().all(|c| "ditq".contains(c))
                })
            })
    };
    let mut i = 1;
    while count > 0 && i < argv.len() {
        let a = argv[i].clone();
        if a == "--" {
            break;
        }
        if let Some(v) = a.strip_prefix(long).and_then(|r| r.strip_prefix('=')) {
            argv[i] = format!("{long}={}", change(v)?);
            count -= 1;
        } else if takes_next(&a) && i + 1 < argv.len() {
            argv[i + 1] = change(&argv[i + 1])?;
            count -= 1;
            i += 1;
        }
        i += 1;
    }
    Ok(())
}

/// On macOS, commands run in podbag's Linux VM; only `vm …`, help and the version stay
/// here.
#[cfg(target_os = "macos")]
fn macos_front(args: Vec<String>) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("__vm-host") => Some(podbag::macos::host::host_main()),
        Some("__vm-standby") => Some(podbag::macos::standby::standby_main()),
        // The Mac's program is replaced; the VM runs the guest it carries.
        None | Some("-h" | "--help" | "help" | "-V" | "--version" | "upgrade") => None,
        // Clients of the Docker API: they use the Mac's docker.sock, which starts the VM.
        Some("tui") => None,
        Some("system") if args.get(1).is_some_and(|a| a == "dial-stdio") => None,
        // The Mac's docker CLI and its contexts.
        Some("system") if args.get(1).is_some_and(|a| a == "docker") => None,
        // Helpers the library starts on this Mac (`__store`, `__shim`, …) run here.
        Some(helper) if helper.starts_with("__") => None,
        // Checked here first: help and wrong arguments need no VM.
        Some(_) => match parse(&args) {
            Err(e) => e.exit(),
            // Logins stay on this Mac: in its config and keychain.
            Ok(cli) if runs_here(&cli) => None,
            Ok(cli) => Some(podbag::macos::cli::forward(mac_command(&cli, args))),
        },
    }
}

/// Commands a front end (macOS, Windows) runs itself: they use the logins kept on this
/// side, and need no containers.
#[cfg(any(target_os = "macos", windows))]
fn runs_here(cli: &Cli) -> bool {
    matches!(
        cli.cmd,
        Cmd::Lab { .. }
            | Cmd::Login { .. }
            | Cmd::Logout { .. }
            | Cmd::Search { .. }
            | Cmd::Migrate { .. }
            | Cmd::Dashboard(_)
            | Cmd::Vm { .. }
    )
}

/// The login a forwarded command needs, as environment for the engine in the VM or WSL:
/// Docker's config and its credential helpers (the keychain on a Mac) are on this side.
/// `run` and `create` skip Docker Hub's official images, which need none: a credential
/// helper would slow every start.
#[cfg(any(target_os = "macos", windows))]
fn forwarded_logins(cli: &Cli) -> Vec<(String, String)> {
    use podbag::image::{auth, reference::Reference};
    let image = match &cli.cmd {
        Cmd::Pull { image } | Cmd::Push { image } => Some(image.as_str()),
        Cmd::Run(a) | Cmd::Create(a) if a.rootfs.is_none() => a.args.first().map(String::as_str),
        _ => None,
    };
    let Some(r) = image.and_then(|i| Reference::parse(i).ok()) else {
        return Vec::new();
    };
    let official = r.registry == "docker.io" && r.repository.starts_with("library/");
    if official && matches!(cli.cmd, Cmd::Run(_) | Cmd::Create(_)) {
        return Vec::new();
    }
    match auth::lookup(&r.registry) {
        Some(login) => vec![(
            auth::AUTH_ENV.to_string(),
            auth::encode_map(&[(r.registry, login)]),
        )],
        None => Vec::new(),
    }
}

#[cfg(any(target_os = "macos", windows, test))]
fn parse(args: &[String]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("podbag".to_string()).chain(args.iter().cloned()))
}

/// What the Mac's side must know of a command it sends to the VM: whether it reads stdin,
/// the Mac paths it names, and the Mac's values of `-e NAME`.
#[cfg(target_os = "macos")]
fn mac_command(cli: &Cli, argv: Vec<String>) -> podbag::macos::cli::Forward {
    let mut f = podbag::macos::cli::Forward {
        argv,
        stdin: false,
        paths: Vec::new(),
        env: forwarded_logins(cli),
    };
    match &cli.cmd {
        Cmd::Run(a) | Cmd::Create(a) => {
            f.stdin = matches!(cli.cmd, Cmd::Run(_)) && a.interactive && !a.detach;
            for v in &a.volume {
                if let Ok(podbag::MountRequest::Bind { source, .. }) =
                    podbag::volume::parse_volume_arg(v)
                {
                    f.paths.push(source);
                }
            }
            f.paths.extend(a.rootfs.clone());
            f.argv = fill_env(&f.argv, a.args.len());
        }
        Cmd::Exec(a) => {
            f.stdin = a.interactive;
            f.argv = fill_env(&f.argv, a.args.len());
        }
        Cmd::Start {
            attach,
            interactive,
            ..
        } => f.stdin = *attach && *interactive,
        Cmd::Load { input, .. } => match input {
            Some(p) => f.paths.push(p.display().to_string()),
            None => f.stdin = true,
        },
        Cmd::Save {
            output: Some(p), ..
        }
        | Cmd::Export {
            output: Some(p), ..
        } => f.paths.push(p.display().to_string()),
        Cmd::Build(a) => {
            if a.context == "-" {
                f.stdin = true;
            } else {
                f.paths.push(a.context.clone());
            }
            f.paths.extend(a.file.clone());
        }
        Cmd::System {
            cmd: args::SystemCmd::Service {
                socket: Some(p), ..
            },
        } => f.paths.push(p.display().to_string()),
        _ => {}
    }
    f
}

/// `-e NAME` (no value) takes NAME's value from the Mac, as a local `run` takes it from
/// its own environment: the VM's is not the Mac's. Only the options before the
/// `trailing` arguments (image and command) are rewritten, and only when clap then reads
/// the same options with the values filled in.
#[cfg(target_os = "macos")]
fn fill_env(argv: &[String], trailing: usize) -> Vec<String> {
    let value = |name: &str| -> Option<String> {
        (!name.contains('=') && !name.is_empty())
            .then(|| std::env::var(name).ok().map(|v| format!("{name}={v}")))
            .flatten()
    };
    // `-e`, `--env`, or boolean short flags ending in e (`-ite`): the value comes next.
    let takes_next = |a: &str| {
        a == "--env"
            || a.strip_prefix('-').is_some_and(|f| {
                !f.starts_with('-')
                    && f.ends_with('e')
                    && f[..f.len() - 1].chars().all(|c| "dit".contains(c))
            })
    };
    let end = argv.len().saturating_sub(trailing);
    let mut out = argv.to_vec();
    let mut i = 0;
    while i < end {
        if takes_next(&argv[i]) && i + 1 < end {
            if let Some(v) = value(&argv[i + 1]) {
                out[i + 1] = v;
            }
            i += 2;
            continue;
        }
        if let Some(v) = argv[i].strip_prefix("--env=").and_then(value) {
            out[i] = format!("--env={v}");
        }
        i += 1;
    }
    let env_of = |cli: &Cli| match &cli.cmd {
        Cmd::Run(a) | Cmd::Create(a) => Some(a.env.clone()),
        Cmd::Exec(a) => Some(a.env.clone()),
        _ => None,
    };
    let expected = parse(argv)
        .ok()
        .and_then(|c| env_of(&c))
        .map(|env| env.iter().map(|e| value(e).unwrap_or(e.clone())).collect());
    match parse(&out) {
        Ok(c) if env_of(&c).is_some() && env_of(&c) == expected => out,
        _ => argv.to_vec(),
    }
}

#[cfg(unix)]
fn dispatch(cmd: Cmd) -> i32 {
    match cmd {
        Cmd::Lab { cmd } => lesson::command(cmd),
        Cmd::Run(a) => run::run(a),
        Cmd::Create(a) => run::create(a),
        Cmd::Start {
            attach,
            interactive,
            containers,
        } => container::start(&containers, attach, interactive),
        Cmd::Restart { time, containers } => container::restart(&containers, time),
        Cmd::Dashboard(cmd) => control::run(cmd),
        #[cfg(target_os = "macos")]
        Cmd::Vm { args } => podbag::macos::cli::vm_command(&args),
        Cmd::Migrate {
            from,
            to,
            kind,
            name,
            destination_defaults,
        } => migrate(from, to, &kind, &name, destination_defaults),
        Cmd::Pull { image } => image::pull(&image),
        Cmd::Push { image } => registry::push(&image),
        Cmd::Tag { source, target } => registry::tag(&source, &target),
        Cmd::Login {
            server,
            username,
            password,
            password_stdin,
        } => registry::login(server.as_deref(), username, password, password_stdin),
        Cmd::Logout { server } => registry::logout(server.as_deref()),
        Cmd::Search {
            term,
            limit,
            no_trunc,
        } => registry::search(&term, limit, no_trunc),
        Cmd::Images => image::images(),
        Cmd::CheckImage { image } => image::check_image(&image),
        Cmd::Check { apparmor_profile } => check::run(apparmor_profile),
        Cmd::Rmi { images } => image::rmi(&images),
        Cmd::Load { input, quiet } => image::load(input.as_deref(), quiet),
        Cmd::Save { output, images } => image::save(output.as_deref(), &images),
        Cmd::Build(a) => build::build(a),
        Cmd::Commit {
            change,
            container: c,
            image,
            ..
        } => build::commit(&c, image, change),
        Cmd::Export {
            output,
            container: c,
        } => build::export(&c, output.as_deref()),
        Cmd::Diff { container: c } => build::diff(&c),
        Cmd::System {
            cmd:
                args::SystemCmd::Service {
                    socket,
                    timeout,
                    print_units,
                },
        } => service::service(socket, timeout, print_units),
        Cmd::System {
            cmd: args::SystemCmd::DialStdio { socket },
        } => service::dial_stdio(socket),
        Cmd::Tui {
            once,
            engine,
            no_mouse,
        } => tui::run(once, engine, !no_mouse),
        Cmd::System { cmd } => image::system(cmd),
        Cmd::Cri { cmd } => cri(cmd),
        Cmd::Ps { all, quiet } => container::ps(all, quiet),
        Cmd::Inspect { container: c } => container::inspect(&c),
        Cmd::Logs {
            follow,
            container: c,
        } => container::logs(&c, follow),
        Cmd::Rm {
            force,
            volumes,
            containers,
        } => container::rm(&containers, force, volumes),
        Cmd::Volume { cmd } => volume::run(cmd),
        Cmd::Network { cmd } => network::run(cmd),
        Cmd::Stop { time, containers } => container::stop(&containers, time),
        Cmd::Kill { signal, containers } => container::kill(&containers, &signal),
        Cmd::Exec(a) => container::exec(a),
        Cmd::Stats {
            no_stream,
            all,
            containers,
        } => manage::stats(&containers, all, no_stream),
        Cmd::Top { container, ps_args } => manage::top(&container, &ps_args),
        Cmd::Pause { containers } => manage::pause(&containers),
        Cmd::Unpause { containers } => manage::unpause(&containers),
        Cmd::Rename {
            container,
            new_name,
        } => manage::rename(&container, &new_name),
        Cmd::Update(a) => manage::update(&a),
        Cmd::ExecHelper { id, request } => exec_helper(&id, &request),
        Cmd::ArchiveHelper { id, request } => archive_helper(&id, &request),
        #[cfg(target_os = "linux")]
        Cmd::VmAgent => podbag::vm::agent::vm_agent_main(),
        #[cfg(not(target_os = "linux"))]
        Cmd::VmAgent => unsupported(),
        #[cfg(target_os = "linux")]
        Cmd::Keeper { runtime } => podbag::keeper::keeper_main(&runtime),
        #[cfg(not(target_os = "linux"))]
        Cmd::Keeper { .. } => {
            eprintln!("podbag: the keeper runs on Linux only");
            1
        }
        Cmd::Shim {
            id,
            mode,
            keep_on_failure,
        } => {
            let mode = match mode.as_deref() {
                Some("attached") => Some(podbag::helper::ShimMode::Attached),
                Some("detached") => Some(podbag::helper::ShimMode::Detached),
                _ => None,
            };
            podbag::shim::shim_main(&id, mode, keep_on_failure)
        }
        Cmd::Store { op } => podbag::store::ops::helper_main(&op),
        Cmd::Session => session_helper(),
        #[cfg(target_os = "linux")]
        Cmd::PodInit => podbag::cri::pod::pause_main(),
        #[cfg(not(target_os = "linux"))]
        Cmd::PodInit => unsupported(),
        Cmd::Restarter { id } => podbag::restart::restart_main(&id),
        Cmd::Probe { id, start_period } => podbag::health::probe_main(&id, start_period),
        Cmd::InstallSelf { dest } => install_self(&dest),
        Cmd::Upgrade {
            check,
            version,
            yes,
            json,
        } => upgrade(check, version, yes, json),
    }
}

fn upgrade(check: bool, version: Option<String>, yes: bool, json: bool) -> i32 {
    podbag::upgrade::upgrade_main(podbag::upgrade::Options {
        check,
        version,
        yes,
        json,
    })
}

#[cfg(unix)]
fn install_self(dest: &std::path::Path) -> i32 {
    match podbag::wsl::install_self(dest, &mut std::io::stdin().lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("podbag: cannot install {}: {e}", dest.display());
            1
        }
    }
}

/// Image commands work everywhere; containers need Linux (on Windows: WSL2, later).
#[cfg(not(unix))]
fn dispatch(cmd: Cmd) -> i32 {
    match cmd {
        Cmd::Lab { cmd } => lesson::command(cmd),
        Cmd::Tui {
            once,
            engine,
            no_mouse,
        } => tui::run(once, engine, !no_mouse),
        Cmd::Dashboard(cmd) => control::run(cmd),
        #[cfg(windows)]
        Cmd::Vm { args } => podbag::windows::vm_command(&args),
        Cmd::Migrate {
            from,
            to,
            kind,
            name,
            destination_defaults,
        } => migrate(from, to, &kind, &name, destination_defaults),
        Cmd::Pull { image } => image::pull(&image),
        Cmd::Push { image } => registry::push(&image),
        Cmd::Tag { source, target } => registry::tag(&source, &target),
        Cmd::Login {
            server,
            username,
            password,
            password_stdin,
        } => registry::login(server.as_deref(), username, password, password_stdin),
        Cmd::Logout { server } => registry::logout(server.as_deref()),
        Cmd::Search {
            term,
            limit,
            no_trunc,
        } => registry::search(&term, limit, no_trunc),
        Cmd::Images => image::images(),
        Cmd::CheckImage { image } => image::check_image(&image),
        Cmd::Check { apparmor_profile } => check::run(apparmor_profile),
        Cmd::Rmi { images } => image::rmi(&images),
        Cmd::Load { input, quiet } => image::load(input.as_deref(), quiet),
        Cmd::Save { output, images } => image::save(output.as_deref(), &images),
        Cmd::System { cmd } => image::system(cmd),
        Cmd::Cri { cmd } => cri(cmd),
        Cmd::Volume { cmd } => volume::run(cmd),
        Cmd::Network { cmd } => network::run(cmd),
        Cmd::Upgrade {
            check,
            version,
            yes,
            json,
        } => upgrade(check, version, yes, json),
        _ => unsupported(),
    }
}

#[cfg(not(target_os = "linux"))]
fn unsupported() -> i32 {
    image::report(&podbag::Error::new(
        podbag::ErrorKind::Unsupported,
        "cannot run containers on this operating system",
        "podbag runs containers only on Linux",
        "run podbag on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn exec_helper(id: &str, request: &str) -> i32 {
    podbag::linux::exec::exec_main(id, request)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn exec_helper(_id: &str, _request: &str) -> i32 {
    unsupported()
}

#[cfg(target_os = "linux")]
fn archive_helper(id: &str, request: &str) -> i32 {
    podbag::linux::archive::archive_main(id, request)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn archive_helper(_id: &str, _request: &str) -> i32 {
    unsupported()
}

#[cfg(target_os = "linux")]
fn session_helper() -> i32 {
    podbag::linux::session::session_main()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn session_helper() -> i32 {
    unsupported()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn via_wsl(args: &[&str]) -> Result<Vec<String>, String> {
        let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let cli = parse(&argv).expect("valid arguments");
        wsl_argv(&cli, argv)
    }

    #[test]
    fn tui_takes_an_engine_and_no_mouse() {
        use podbag::client::Choice;
        let tui = |args: &[&str]| {
            let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            parse(&argv).map(|cli| match cli.cmd {
                Cmd::Tui {
                    engine, no_mouse, ..
                } => (engine, no_mouse),
                _ => panic!("not tui"),
            })
        };
        assert_eq!(tui(&["tui"]).unwrap(), (None, false));
        assert_eq!(
            tui(&["tui", "--engine", "docker", "--no-mouse"]).unwrap(),
            (Some(Choice::Docker), true)
        );
        assert_eq!(
            tui(&["tui", "--engine=podbag"]).unwrap(),
            (Some(Choice::Podbag), false)
        );
        assert_eq!(
            tui(&["tui", "--engine", "unix:///run/x.sock"]).unwrap().0,
            Some(Choice::Socket("/run/x.sock".into()))
        );
        let e = tui(&["tui", "--engine", "tcp://1.2.3.4:2375"])
            .err()
            .unwrap();
        assert!(e.to_string().contains("podbag, docker"), "{e}");
    }

    #[test]
    fn windows_paths_in_volumes_become_wsl_paths_but_not_in_the_command() {
        let out = via_wsl(&[
            "run",
            "--rm",
            "-v",
            r"C:\Users\me\app:/app:ro",
            "-itv",
            r".\data:/data",
            "alpine",
            "ls",
            "-v",
            r"C:\x",
        ])
        .unwrap();
        assert_eq!(
            out,
            [
                "run",
                "--rm",
                "-v",
                "/mnt/c/Users/me/app:/app:ro",
                "-itv",
                "./data:/data",
                "alpine",
                "ls",
                "-v",
                r"C:\x",
            ]
        );
    }

    #[test]
    fn long_forms_rootfs_and_named_volumes() {
        let out = via_wsl(&[
            "run",
            r"--volume=D:\cache:/cache",
            "-v",
            "data:/data",
            "--rootfs",
            r"E:\root",
            "true",
        ])
        .unwrap();
        assert_eq!(
            out,
            [
                "run",
                "--volume=/mnt/d/cache:/cache",
                "-v",
                "data:/data",
                "--rootfs",
                "/mnt/e/root",
                "true",
            ]
        );
    }

    #[test]
    fn load_and_save_files() {
        assert_eq!(
            via_wsl(&["load", "-qi", r"C:\images\a.tar"]).unwrap(),
            ["load", "-qi", "/mnt/c/images/a.tar"]
        );
        assert_eq!(
            via_wsl(&["save", "--output", r"out\a.tar", "alpine"]).unwrap(),
            ["save", "--output", "out/a.tar", "alpine"]
        );
        assert_eq!(via_wsl(&["ps", "-a"]).unwrap(), ["ps", "-a"]);
    }

    #[test]
    fn build_context_dockerfile_and_export_files() {
        assert_eq!(
            via_wsl(&["build", "-t", "app", "-f", r"C:\src\Dockerfile", r"C:\src"]).unwrap(),
            [
                "build",
                "-t",
                "app",
                "-f",
                "/mnt/c/src/Dockerfile",
                "/mnt/c/src"
            ]
        );
        assert_eq!(
            via_wsl(&["build", "-q", "."]).unwrap(),
            ["build", "-q", "."]
        );
        assert_eq!(via_wsl(&["build", "-"]).unwrap(), ["build", "-"]);
        assert_eq!(
            via_wsl(&["export", "-o", r"out\c.tar", "c"]).unwrap(),
            ["export", "-o", "out/c.tar", "c"]
        );
    }

    #[test]
    fn network_paths_are_refused() {
        assert!(via_wsl(&["run", "-v", r"\\server\share:/data", "alpine"]).is_err());
    }
}

/// `podbag cri serve`.
#[cfg(target_os = "linux")]
fn cri(cmd: args::CriCmd) -> i32 {
    use podbag::cri::service::{CgroupDriver, Options};
    let args::CriCmd::Serve {
        socket,
        cni_conf_dir,
        cni_bin_dir,
        cgroup_driver,
    } = cmd;
    let options = Options {
        socket,
        cni_conf_dir,
        cni_bin_dirs: cni_bin_dir
            .split(':')
            .filter(|d| !d.is_empty())
            .map(Into::into)
            .collect(),
        cgroup_driver: match cgroup_driver {
            args::CgroupDriverArg::Cgroupfs => CgroupDriver::Cgroupfs,
            args::CgroupDriverArg::Systemd => CgroupDriver::Systemd,
        },
    };
    match podbag::cri::server::serve(options) {
        Ok(()) => 0,
        Err(e) => image::report(&e),
    }
}

/// `podbag cri serve` runs on Linux nodes only.
#[cfg(not(target_os = "linux"))]
fn cri(_cmd: args::CriCmd) -> i32 {
    image::report(&podbag::Error::new(
        podbag::ErrorKind::Unsupported,
        "podbag cri serve",
        "the CRI runs on Linux nodes, where the kubelet runs",
        "run it on a Linux machine",
    ))
}
