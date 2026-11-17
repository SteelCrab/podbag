mod args;
#[cfg(unix)]
mod container;
mod image;
mod network;
#[cfg(unix)]
mod run;
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
        Cmd::Pull { image } => image::pull(&image),
        Cmd::Images => image::images(),
        Cmd::Rmi { images } => image::rmi(&images),
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
        Cmd::Shim { id } => openpod::shim::shim_main(&id),
        Cmd::Store { op } => openpod::store::ops::helper_main(&op),
        Cmd::Session => session_helper(),
    }
}

/// Image commands work everywhere; containers need Linux (on Windows: WSL2, later).
#[cfg(not(unix))]
fn dispatch(cmd: Cmd) -> i32 {
    match cmd {
        Cmd::Pull { image } => image::pull(&image),
        Cmd::Images => image::images(),
        Cmd::Rmi { images } => image::rmi(&images),
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
fn session_helper() -> i32 {
    openpod::linux::session::session_main()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn session_helper() -> i32 {
    unsupported()
}
