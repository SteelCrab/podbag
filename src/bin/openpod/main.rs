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
    let cli = Cli::parse();
    std::process::exit(dispatch(cli.cmd));
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
        Cmd::Keeper { runtime } => openpod::keeper::keeper_main(&runtime),
        #[cfg(not(target_os = "linux"))]
        Cmd::Keeper { .. } => {
            eprintln!("openpod: the keeper runs on Linux only");
            1
        }
        Cmd::Shim {
            id,
            mode,
            keep_on_failure,
        } => {
            let mode = match mode.as_deref() {
                Some("attached") => Some(openpod::helper::ShimMode::Attached),
                Some("detached") => Some(openpod::helper::ShimMode::Detached),
                _ => None,
            };
            openpod::shim::shim_main(&id, mode, keep_on_failure)
        }
        Cmd::Store { op } => openpod::store::ops::helper_main(&op),
        Cmd::Session => session_helper(),
        Cmd::Restarter { id } => openpod::restart::restart_main(&id),
        Cmd::Probe { id, start_period } => openpod::health::probe_main(&id, start_period),
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
    image::report(&openpod::Error::new(
        openpod::ErrorKind::Unsupported,
        "cannot run containers on this operating system",
        "openpod runs containers only on Linux",
        "run openpod on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn exec_helper(id: &str, request: &str) -> i32 {
    openpod::linux::exec::exec_main(id, request)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn exec_helper(_id: &str, _request: &str) -> i32 {
    unsupported()
}

#[cfg(target_os = "linux")]
fn archive_helper(id: &str, request: &str) -> i32 {
    openpod::linux::archive::archive_main(id, request)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn archive_helper(_id: &str, _request: &str) -> i32 {
    unsupported()
}

#[cfg(target_os = "linux")]
fn session_helper() -> i32 {
    openpod::linux::session::session_main()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn session_helper() -> i32 {
    unsupported()
}
