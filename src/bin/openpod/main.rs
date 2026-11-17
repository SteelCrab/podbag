mod args;
mod container;
mod image;
mod run;

use args::{Cli, Cmd};
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
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
        Cmd::Rm { force, containers } => container::rm(&containers, force),
        Cmd::Stop { time, containers } => container::stop(&containers, time),
        Cmd::Kill { signal, containers } => container::kill(&containers, &signal),
        Cmd::Exec(a) => container::exec(a),
        Cmd::ExecHelper { id, request } => exec_helper(&id, &request),
        Cmd::Shim { id } => openpod::shim::shim_main(&id),
        Cmd::Store { op } => openpod::store::ops::helper_main(&op),
    };
    std::process::exit(code);
}

#[cfg(target_os = "linux")]
fn exec_helper(id: &str, request: &str) -> i32 {
    openpod::linux::exec::exec_main(id, request)
}

#[cfg(not(target_os = "linux"))]
fn exec_helper(_id: &str, _request: &str) -> i32 {
    image::report(&openpod::Error::new(
        openpod::ErrorKind::Unsupported,
        "cannot run containers on this operating system",
        "openpod runs containers only on Linux",
        "run openpod on Linux",
    ))
}
