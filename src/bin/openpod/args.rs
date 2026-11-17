use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "openpod", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run a command in a new container
    Run(RunArgs),
    /// Download an image from a registry
    Pull { image: String },
    /// List stored images
    Images,
    /// Remove images
    Rmi {
        #[arg(required = true)]
        images: Vec<String>,
    },
    /// List containers
    Ps {
        /// Show all containers (default: only running ones)
        #[arg(short, long)]
        all: bool,
        /// Show only IDs
        #[arg(short, long)]
        quiet: bool,
    },
    /// Show a container's details as JSON
    Inspect { container: String },
    /// Show a container's output
    Logs {
        /// Keep printing new output until the container ends
        #[arg(short, long)]
        follow: bool,
        container: String,
    },
    /// Remove containers
    Rm {
        /// Kill running containers first
        #[arg(short, long)]
        force: bool,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Stop containers: SIGTERM, then SIGKILL after a timeout
    Stop {
        /// Seconds to wait before SIGKILL
        #[arg(short, long, default_value_t = 10)]
        time: u64,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Send a signal to containers
    Kill {
        /// Signal name or number
        #[arg(short, long, default_value = "KILL")]
        signal: String,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Run a command in a running container
    Exec(ExecArgs),
    #[command(name = "__exec", hide = true)]
    ExecHelper { id: String, request: String },
    /// Manage the image store
    System {
        #[command(subcommand)]
        cmd: SystemCmd,
    },
    #[command(name = "__shim", hide = true)]
    Shim { id: String },
    #[command(name = "__store", hide = true)]
    Store { op: String },
}

#[derive(Subcommand)]
pub enum SystemCmd {
    /// Show disk use
    Df,
    /// Remove every image that no container uses
    Prune {
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Args)]
pub struct ExecArgs {
    /// Keep stdin open and connect it to the command
    #[arg(short = 'i', long)]
    pub interactive: bool,
    /// Give the command a terminal
    #[arg(short = 't', long)]
    pub tty: bool,
    /// User to run as
    #[arg(short = 'u', long)]
    pub user: Option<String>,
    /// Working directory inside the container
    #[arg(short = 'w', long)]
    pub workdir: Option<String>,
    /// Set an environment variable
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// CONTAINER COMMAND...
    #[arg(required = true, num_args = 2.., trailing_var_arg = true, allow_hyphen_values = true, value_name = "CONTAINER COMMAND")]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct RunArgs {
    /// Remove the container when it exits
    #[arg(long)]
    pub rm: bool,
    /// Run in the background and print the container ID
    #[arg(short = 'd', long)]
    pub detach: bool,
    /// Container name
    #[arg(long)]
    pub name: Option<String>,
    /// Give the command a terminal
    #[arg(short = 't', long)]
    pub tty: bool,
    /// Keep stdin open and connect it to the command
    #[arg(short = 'i', long)]
    pub interactive: bool,
    /// Use this unpacked root filesystem directory instead of an image
    #[arg(long)]
    pub rootfs: Option<String>,
    /// Memory limit (like 512m or 2g)
    #[arg(short = 'm', long)]
    pub memory: Option<String>,
    /// Number of CPUs (like 0.5)
    #[arg(long)]
    pub cpus: Option<f64>,
    /// Process limit (0 or less: no limit)
    #[arg(long = "pids-limit", allow_hyphen_values = true)]
    pub pids_limit: Option<i64>,
    /// Add a Linux capability (or ALL)
    #[arg(long = "cap-add")]
    pub cap_add: Vec<String>,
    /// Drop a Linux capability (or ALL)
    #[arg(long = "cap-drop")]
    pub cap_drop: Vec<String>,
    /// Security option: no-new-privileges, seccomp=unconfined
    #[arg(long = "security-opt")]
    pub security_opt: Vec<String>,
    /// Set an environment variable (NAME=VALUE, or NAME to copy it from this shell)
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Working directory inside the container
    #[arg(short = 'w', long = "workdir")]
    pub workdir: Option<String>,
    /// Container hostname
    #[arg(long)]
    pub hostname: Option<String>,
    /// User to run as: name, uid, name:group or uid:gid
    #[arg(short = 'u', long = "user")]
    pub user: Option<String>,
    /// Replace the image's entrypoint ("" for none)
    #[arg(long)]
    pub entrypoint: Option<String>,
    /// IMAGE [COMMAND...], or only COMMAND... with --rootfs
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "IMAGE|COMMAND"
    )]
    pub args: Vec<String>,
}

/// A signal by name (`TERM`, `SIGTERM`) or number. Names give Linux numbers on every
/// host: the container always runs on Linux (on macOS and Windows, inside a VM).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn parse_signal(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return (n > 0 && n < 65).then_some(n);
    }
    let name = s.to_ascii_uppercase();
    let name = name.strip_prefix("SIG").unwrap_or(&name);
    Some(match name {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        "TERM" => 15,
        "CONT" => 18,
        "STOP" => 19,
        "WINCH" => 28,
        _ => return None,
    })
}

/// Parse a `-e` value like `docker run`: `NAME=VALUE`, or `NAME` to copy the caller's value.
/// A bare name that the caller does not have is skipped.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn parse_env(arg: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<(String, String)> {
    match arg.split_once('=') {
        Some((k, v)) => Some((k.to_string(), v.to_string())),
        None => lookup(arg).map(|v| (arg.to_string(), v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_signal_accepts_names_and_numbers() {
        assert_eq!(parse_signal("TERM"), Some(15));
        assert_eq!(parse_signal("sigterm"), Some(15));
        // Linux numbers, also when the CLI runs on macOS (where SIGUSR1 is 30).
        assert_eq!(parse_signal("USR1"), Some(10));
        assert_eq!(parse_signal("STOP"), Some(19));
        assert_eq!(parse_signal("9"), Some(9));
        assert_eq!(parse_signal("NOPE"), None);
        assert_eq!(parse_signal("0"), None);
    }

    #[test]
    fn parse_env_splits_at_the_first_equals_sign() {
        let none = |_: &str| None;
        assert_eq!(parse_env("A=b=c", none), Some(("A".into(), "b=c".into())));
        assert_eq!(
            parse_env("EMPTY=", none),
            Some(("EMPTY".into(), String::new()))
        );
    }

    #[test]
    fn parse_env_copies_a_bare_name_from_the_caller() {
        let host = |k: &str| (k == "TOKEN").then(|| "secret".to_string());
        assert_eq!(
            parse_env("TOKEN", host),
            Some(("TOKEN".into(), "secret".into()))
        );
        assert_eq!(parse_env("UNSET", host), None);
    }
}
