mod args;
mod run;

use args::{Cli, Cmd};
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Run(a) => run::run(a),
        Cmd::Shim { id } => openpod::shim::shim_main(&id),
    };
    std::process::exit(code);
}
