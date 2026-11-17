mod args;
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
        Cmd::Shim { id } => openpod::shim::shim_main(&id),
        Cmd::Store { op } => openpod::store::ops::helper_main(&op),
    };
    std::process::exit(code);
}
