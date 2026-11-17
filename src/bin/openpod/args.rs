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
    #[command(name = "__shim", hide = true)]
    Shim { id: String },
}

#[derive(Args)]
pub struct RunArgs {
    /// Use this unpacked root filesystem directory
    #[arg(long)]
    pub rootfs: String,
    /// Set an environment variable (NAME=VALUE, or NAME to copy it from this shell)
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Working directory inside the container
    #[arg(short = 'w', long = "workdir")]
    pub workdir: Option<String>,
    /// Container hostname
    #[arg(long)]
    pub hostname: Option<String>,
    /// The command and its arguments
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
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
