//! Non-interactive access to the operations shared with the dashboards.
pub mod engine;
pub mod kube;
pub mod settings;
pub mod tutorial;

use clap::Subcommand;
use serde::Serialize;
use std::io::{self, Write};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub enum Cmd {
    /// Manage resources and select the active Docker-compatible engine
    Engine(engine::Args),
    /// List or copy tagged images between engines
    Import(engine::ImportArgs),
    /// Discover Kubernetes contexts, inspect resources and migrate them
    Kube(kube::Args),
    /// Read or change dashboard preferences and Desktop startup
    Settings(settings::Args),
    /// Install or run a volume and HTTP tutorial
    Tutorial(tutorial::Args),
}

pub fn run(cmd: Cmd) -> i32 {
    let result = match cmd {
        Cmd::Engine(args) => engine::run(args),
        Cmd::Import(args) => engine::import(args),
        Cmd::Kube(args) => kube::run(args),
        Cmd::Settings(args) => settings::run(args),
        Cmd::Tutorial(args) => tutorial::run(args),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("podbag: {err}");
            1
        }
    }
}

fn json(value: impl Serialize) -> Result {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &value)?;
    stdout.write_all(b"\n")?;
    Ok(())
}
