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
pub struct RunArgs {
    /// Use this unpacked root filesystem directory instead of an image
    #[arg(long)]
    pub rootfs: Option<String>,
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

/// Parse a `-e` value like `docker run`: `NAME=VALUE`, or `NAME` to copy the caller's value.
/// A bare name that the caller does not have is skipped.
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
