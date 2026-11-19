mod args;
mod check;
#[cfg(unix)]
mod container;
mod image;
mod network;
#[cfg(unix)]
mod run;
#[cfg(unix)]
mod service;
mod volume;

use args::{Cli, Cmd};
use clap::Parser;

fn main() {
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
    #[cfg(target_os = "macos")]
    if let Some(code) = macos_front(std::env::args().skip(1).collect()) {
        std::process::exit(code);
    }
    let cli = Cli::parse();
    std::process::exit(dispatch(cli.cmd));
}

/// On macOS, commands run in podbag's Linux VM; only `vm …`, help and the version stay
/// here.
#[cfg(target_os = "macos")]
fn macos_front(args: Vec<String>) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("__vm-host") => Some(podbag::macos::host::host_main()),
        Some("__vm-standby") => Some(podbag::macos::standby::standby_main()),
        Some("vm") => Some(podbag::macos::cli::vm_command(&args[1..])),
        None | Some("-h" | "--help" | "help" | "-V" | "--version") => None,
        // Helpers the library starts on this Mac (`__store`, `__shim`, …) run here.
        Some(helper) if helper.starts_with("__") => None,
        // Checked here first: help and wrong arguments need no VM.
        Some(_) => match parse(&args) {
            Err(e) => e.exit(),
            Ok(cli) => Some(podbag::macos::cli::forward(mac_command(&cli, args))),
        },
    }
}

#[cfg(target_os = "macos")]
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
        } => f.paths.push(p.display().to_string()),
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
        Cmd::Run(a) => run::run(a),
        Cmd::Create(a) => run::create(a),
        Cmd::Start {
            attach,
            interactive,
            containers,
        } => container::start(&containers, attach, interactive),
        Cmd::Restart { time, containers } => container::restart(&containers, time),
        Cmd::Pull { image } => image::pull(&image),
        Cmd::Images => image::images(),
        Cmd::CheckImage { image } => image::check_image(&image),
        Cmd::Check { apparmor_profile } => check::run(apparmor_profile),
        Cmd::Rmi { images } => image::rmi(&images),
        Cmd::Load { input, quiet } => image::load(input.as_deref(), quiet),
        Cmd::Save { output, images } => image::save(output.as_deref(), &images),
        Cmd::System {
            cmd:
                args::SystemCmd::Service {
                    socket,
                    timeout,
                    print_units,
                },
        } => service::service(socket, timeout, print_units),
        Cmd::System { cmd } => image::system(cmd),
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
        Cmd::Restarter { id } => podbag::restart::restart_main(&id),
        Cmd::Probe { id, start_period } => podbag::health::probe_main(&id, start_period),
    }
}

/// Image commands work everywhere; containers need Linux (on Windows: WSL2, later).
#[cfg(not(unix))]
fn dispatch(cmd: Cmd) -> i32 {
    match cmd {
        Cmd::Pull { image } => image::pull(&image),
        Cmd::Images => image::images(),
        Cmd::CheckImage { image } => image::check_image(&image),
        Cmd::Check { apparmor_profile } => check::run(apparmor_profile),
        Cmd::Rmi { images } => image::rmi(&images),
        Cmd::Load { input, quiet } => image::load(input.as_deref(), quiet),
        Cmd::Save { output, images } => image::save(output.as_deref(), &images),
        Cmd::System { cmd } => image::system(cmd),
        Cmd::Volume { cmd } => volume::run(cmd),
        Cmd::Network { cmd } => network::run(cmd),
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
