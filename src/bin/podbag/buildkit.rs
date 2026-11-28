//! The terminal interface to the managed BuildKit worker.

use podbag::build::managed::{Options, ResultOutput};
use podbag::build::managed_worker::spawn;
use podbag::build::{BuildOptions, managed};
use podbag::{Error, Result};
use std::process::{Command, ExitStatus};

pub fn run(build: &BuildOptions, options: Options<'_>) -> Result<()> {
    match managed::run(build, options, run_client)? {
        ResultOutput::Image(image) => println!("{}", image.id),
        ResultOutput::Path(path) => println!("{}", path.display()),
    }
    Ok(())
}

fn run_client(command: &mut Command) -> Result<ExitStatus> {
    let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
    podbag::signals::block(&signals);
    let status = match spawn(command) {
        Ok(mut child) => {
            podbag::signals::forward_all_to(child.id(), &signals);
            child.wait()
        }
        Err(error) => Err(error),
    };
    podbag::signals::restore_default(&signals);
    status.map_err(|e| Error::io("cannot run buildctl", &e, "install buildctl and check PATH"))
}
