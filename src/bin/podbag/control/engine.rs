use super::{Result, json};
use crate::args::parse_engine;
use clap::{Subcommand, ValueEnum};
use podbag::client::{self, Choice, Client, Kind, Target, choice, podbag_program};
use podbag::dashboard::{importable_images, local_engines};
use serde_json::json;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

#[derive(clap::Args)]
pub struct Args {
    /// Use podbag, docker, podman, kiri, context:NAME or unix:///PATH for this command
    #[arg(long, global = true, value_parser = parse_engine)]
    target: Option<Choice>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Resource {
    #[value(alias = "containers")]
    Container,
    #[value(alias = "images")]
    Image,
    #[value(alias = "volumes")]
    Volume,
    #[value(alias = "networks")]
    Network,
}

impl From<Resource> for Kind {
    fn from(resource: Resource) -> Self {
        match resource {
            Resource::Container => Self::Containers,
            Resource::Image => Self::Images,
            Resource::Volume => Self::Volumes,
            Resource::Network => Self::Networks,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Action {
    Start,
    Stop,
    Restart,
    Pause,
    #[value(alias = "resume")]
    Unpause,
}

impl Action {
    fn api_name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Pause => "pause",
            Self::Unpause => "unpause",
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List built-in engine choices and Docker contexts
    Choices,
    /// List local engine endpoints available for migration
    Local,
    /// Save the engine choice for the dashboards and engine commands
    Use {
        #[arg(value_parser = parse_engine)]
        engine: Choice,
    },
    /// Show the selected engine and its API version
    Status,
    /// List a resource type as JSON
    #[command(alias = "ls")]
    List { kind: Resource },
    /// Print a resource's details as JSON
    Inspect { kind: Resource, name: String },
    /// Start, stop, restart, pause or unpause a container
    Action { action: Action, container: String },
    /// Rename a container
    Rename { container: String, name: String },
    /// Remove a resource
    #[command(alias = "rm")]
    Remove {
        kind: Resource,
        name: String,
        /// Allow removing a running container or an image in use
        #[arg(short, long)]
        force: bool,
    },
    /// Remove unused resources of one type
    Prune {
        kind: Resource,
        /// Confirm removal of unused resources
        #[arg(long, required = true)]
        force: bool,
    },
    /// Create a named volume
    CreateVolume { name: String },
    /// Stream engine events as one JSON object per line
    Events,
    /// Print container logs
    Logs {
        container: String,
        #[arg(short, long)]
        follow: bool,
        #[arg(long, default_value_t = 1000)]
        tail: usize,
    },
    /// Print a snapshot of container resource usage
    Stats { container: String },
    /// Print the processes of a container
    Top { container: String },
    /// Run a command in a container on the selected engine
    Exec {
        #[arg(short, long)]
        interactive: bool,
        #[arg(short, long)]
        tty: bool,
        container: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Pull an image into the selected engine
    Pull {
        image: String,
        /// Qualify a short image name using a registry preset
        #[arg(long)]
        registry: Option<String>,
    },
    /// Search Docker Hub through the selected engine
    Search {
        term: String,
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
}

fn connect(target: &Target, exe: &Path) -> Result<Client> {
    target.ensure_running(exe, true)?;
    Ok(Client::new(target.endpoint.clone()))
}

pub fn run(args: Args) -> Result {
    let exe = podbag_program();
    let cmd = match args.cmd {
        Cmd::Choices => {
            return json(
                choice::engine_choices()
                    .iter()
                    .map(Choice::to_setting)
                    .collect::<Vec<_>>(),
            );
        }
        Cmd::Local => {
            return json(
                local_engines(&exe)
                    .iter()
                    .map(|t| t.choice.to_setting())
                    .collect::<Vec<_>>(),
            );
        }
        Cmd::Use { engine } => {
            Target::find(Some(&engine), &exe)?;
            choice::save(&engine)?;
            return json(json!({"engine": engine.to_setting()}));
        }
        cmd => cmd,
    };

    let target = Target::find(args.target.as_ref(), &exe)?;
    // Hand the terminal and command to the CLI for this endpoint.
    if let Cmd::Exec {
        interactive,
        tty,
        container,
        command,
    } = cmd
    {
        if container.starts_with('-') || container.is_empty() {
            return Err("Enter a container name or ID".into());
        }
        let docker = client::find_docker_cli();
        let argv = target
            .cli(&exe, docker.as_deref())
            .ok_or("This engine needs its Docker-compatible CLI to run exec")?;
        let mut exec = Command::new(&argv[0]);
        exec.args(&argv[1..]).arg("exec");
        if interactive {
            exec.arg("-i");
        }
        if tty {
            exec.arg("-t");
        }
        let status = exec.arg(container).args(command).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    let client = connect(&target, &exe)?;
    match cmd {
        Cmd::Status => json(json!({
            "engine": target.choice.to_setting(),
            "endpoint": target.endpoint.describe(),
            "version": client.version()?,
        })),
        Cmd::List { kind } => match Kind::from(kind) {
            Kind::Containers => json(client.containers()?),
            Kind::Images => json(client.images()?),
            Kind::Volumes => json(client.volumes()?),
            Kind::Networks => json(client.networks()?),
        },
        Cmd::Inspect { kind, name } => json(client.inspect(kind.into(), &name)?),
        Cmd::Action { action, container } => {
            Ok(client.container_action(&container, action.api_name())?)
        }
        Cmd::Rename { container, name } => Ok(client.rename(&container, &name)?),
        Cmd::Remove { kind, name, force } => Ok(client.remove(kind.into(), &name, force)?),
        Cmd::Prune { kind, force } => {
            if !force {
                return Err("Use --force to remove unused resources".into());
            }
            json(client.prune(kind.into())?)
        }
        Cmd::CreateVolume { name } => {
            if name.trim().is_empty() {
                return Err("Enter a volume name".into());
            }
            client.create_volume(&name)?;
            json(json!({"name": name}))
        }
        Cmd::Events => {
            for event in client.events()? {
                json(event?)?;
                io::stdout().lock().flush()?;
            }
            Ok(())
        }
        Cmd::Logs {
            container,
            follow,
            tail,
        } => {
            let mut stdout = io::stdout().lock();
            for line in client.logs(&container, tail, follow)? {
                writeln!(stdout, "{}", line?)?;
            }
            Ok(())
        }
        Cmd::Stats { container } => json(client.container_json(&container, "stats")?),
        Cmd::Top { container } => json(client.container_json(&container, "top")?),
        Cmd::Pull { image, registry } => {
            let name = super::settings::qualify_image(&image, registry.as_deref())?;
            let auth = target
                .allows_registry_auth()
                .then(|| client::registry_auth(&name))
                .flatten();
            client.pull_auth(&name, auth.as_deref(), |line| eprintln!("{line}"))?;
            json(json!({"image": name}))
        }
        Cmd::Search { term, limit } => json(client.search(&term, limit)?),
        Cmd::Choices | Cmd::Local | Cmd::Use { .. } | Cmd::Exec { .. } => unreachable!(),
    }
}

#[derive(clap::Args)]
pub struct ImportArgs {
    /// Source engine
    #[arg(long, global = true, default_value = "docker", value_parser = parse_engine)]
    from: Choice,
    /// Destination engine
    #[arg(long, global = true, default_value = "podbag", value_parser = parse_engine)]
    to: Choice,
    #[command(subcommand)]
    cmd: Import,
}

#[derive(Subcommand)]
enum Import {
    /// List source tags, their sizes and whether each tag exists at the destination
    List,
    /// Copy one or more tagged images to the destination
    Images {
        #[arg(required = true)]
        images: Vec<String>,
    },
}

pub fn import(args: ImportArgs) -> Result {
    let exe = podbag_program();
    let source = Target::find(Some(&args.from), &exe)?;
    let destination = Target::find(Some(&args.to), &exe)?;
    if source.endpoint.same_target(&destination.endpoint) {
        return Err("Choose two different engines".into());
    }
    let source = connect(&source, &exe)?;
    // Verify the source before starting a destination VM.
    source.ping()?;
    let destination = connect(&destination, &exe)?;
    match args.cmd {
        Import::List => json(importable_images(&source, &destination)?),
        Import::Images { images } => {
            destination.import_images(&source, &images)?;
            json(json!({"images": images, "destination": args.to.to_setting()}))
        }
    }
}
