use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "podbag", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run a command in a new container
    Run(RunArgs),
    /// Create a container without starting it
    Create(RunArgs),
    /// Start created or stopped containers
    Start {
        /// Attach to its output (and wait for it to end)
        #[arg(short, long)]
        attach: bool,
        /// Connect stdin (with --attach)
        #[arg(short, long)]
        interactive: bool,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Stop and start containers
    Restart {
        /// Seconds to wait before killing
        #[arg(short, long, default_value_t = 10)]
        time: u64,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Download an image from a registry
    Pull { image: String },
    /// List stored images
    Images,
    /// Find programs in an image that cannot run (wrong C library or CPU)
    CheckImage { image: String },
    /// Check that this system can run podbag containers
    Check {
        /// Print the AppArmor profile that lets podbag use user namespaces
        #[arg(long)]
        apparmor_profile: bool,
    },
    /// Load images from an archive (`docker save` format or OCI layout) on stdin or a file
    Load {
        /// Read from this file instead of stdin
        #[arg(short, long)]
        input: Option<std::path::PathBuf>,
        /// Print nothing
        #[arg(short, long)]
        quiet: bool,
    },
    /// Save images to an archive (`docker save` format) on stdout or a file
    Save {
        /// Write to this file instead of stdout
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
        #[arg(required = true)]
        images: Vec<String>,
    },
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
        /// Also remove the containers' anonymous volumes
        #[arg(short = 'v', long)]
        volumes: bool,
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
    #[command(name = "__archive", hide = true)]
    ArchiveHelper { id: String, request: String },
    /// The macOS VM's agent (it runs in the VM)
    #[command(name = "__vm-agent", hide = true)]
    VmAgent,
    /// Manage networks
    Network {
        #[command(subcommand)]
        cmd: NetworkCmd,
    },
    /// Manage volumes
    Volume {
        #[command(subcommand)]
        cmd: VolumeCmd,
    },
    /// Run podbag as a Kubernetes container runtime (CRI)
    Cri {
        #[command(subcommand)]
        cmd: CriCmd,
    },
    /// Manage the image store
    System {
        #[command(subcommand)]
        cmd: SystemCmd,
    },
    /// The keeper of a runtime directory
    #[command(name = "__keeper", hide = true)]
    Keeper { runtime: String },
    #[command(name = "__shim", hide = true)]
    Shim {
        id: String,
        /// detached or attached
        #[arg(long)]
        mode: Option<String>,
        #[arg(long)]
        keep_on_failure: bool,
    },
    #[command(name = "__store", hide = true)]
    Store { op: String },
    #[command(name = "__session", hide = true)]
    Session,
    /// Start an ended container again when its restart policy says so
    #[command(name = "__restart", hide = true)]
    Restarter { id: String },
    /// Run a container's health check once
    #[command(name = "__health", hide = true)]
    Probe {
        id: String,
        #[arg(long)]
        start_period: bool,
    },
    /// Replace the engine at DEST with one read from stdin (podbag.exe updates its WSL
    /// distribution this way)
    #[command(name = "__install-self", hide = true)]
    InstallSelf { dest: std::path::PathBuf },
}

#[derive(Subcommand)]
pub enum NetworkCmd {
    /// Create a bridge network
    Create {
        /// Subnet in CIDR form (default: the next free 10.78.<n>.0/24)
        #[arg(long)]
        subnet: Option<String>,
        /// A label, as key=value or key (may be repeated)
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
        name: String,
    },
    /// List networks
    Ls,
    /// Remove networks that no container uses
    Rm {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Show a network's details as JSON
    Inspect { name: String },
    /// Connect a stopped container to a further network (from its next start on)
    Connect {
        /// A further name of the container on this network (may be repeated)
        #[arg(long = "alias", value_name = "ALIAS")]
        aliases: Vec<String>,
        network: String,
        container: String,
    },
    /// Disconnect a stopped container from a further network
    Disconnect { network: String, container: String },
}

#[derive(Subcommand)]
pub enum VolumeCmd {
    /// Create a volume (a random name when none is given)
    Create {
        /// A label, as key=value or key (may be repeated)
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
        name: Option<String>,
    },
    /// List volumes
    Ls {
        /// Show only names
        #[arg(short, long)]
        quiet: bool,
    },
    /// Remove volumes that no container uses
    Rm {
        /// Do not report missing volumes
        #[arg(short, long)]
        force: bool,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Show a volume's details as JSON
    Inspect { name: String },
}

#[derive(Subcommand)]
pub enum CriCmd {
    /// Serve the Kubernetes CRI on a Unix socket, for a kubelet (as root)
    ///
    /// Point the kubelet at it with --container-runtime-endpoint unix://<socket>. Unlike
    /// the rest of podbag, this runs as long as the kubelet uses it.
    Serve {
        /// The socket
        #[arg(long, default_value = "/run/podbag/cri.sock")]
        socket: std::path::PathBuf,
        /// Where the CNI network configurations are
        #[arg(long, default_value = "/etc/cni/net.d")]
        cni_conf_dir: std::path::PathBuf,
        /// Where the CNI plugins are; several folders separated by ':'
        #[arg(long, default_value = "/opt/cni/bin")]
        cni_bin_dir: String,
        /// How the kubelet places cgroups
        #[arg(long, value_enum, default_value_t = CgroupDriverArg::Cgroupfs)]
        cgroup_driver: CgroupDriverArg,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CgroupDriverArg {
    Cgroupfs,
    Systemd,
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
    /// Serve the Docker Engine API on a Unix socket, for the docker CLI
    ///
    /// Point the CLI at it with DOCKER_HOST=unix://<socket>. Containers do not depend on
    /// the service; it ends after --timeout seconds without a connection.
    Service {
        /// The socket (default: docker.sock in the podbag runtime directory)
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// End after this many seconds without a connection (0: never)
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Print systemd units that start the service on the first connection
        #[arg(long)]
        print_units: bool,
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
    /// Every capability, no seccomp filter, unmasked kernel files, a writable /sys and
    /// cgroup (inside the user namespace when rootless)
    #[arg(long)]
    pub privileged: bool,
    /// Start it again after it ends: no, always, unless-stopped, on-failure[:max]
    #[arg(long, value_name = "POLICY")]
    pub restart: Option<String>,
    /// A command that checks the container's health (run with /bin/sh -c)
    #[arg(long = "health-cmd", value_name = "COMMAND")]
    pub health_cmd: Option<String>,
    /// Time between checks (30s, 1m, 500ms)
    #[arg(long = "health-interval", value_name = "DURATION")]
    pub health_interval: Option<String>,
    /// Time a check may take
    #[arg(long = "health-timeout", value_name = "DURATION")]
    pub health_timeout: Option<String>,
    /// Failures in a row before it is unhealthy
    #[arg(long = "health-retries", value_name = "N")]
    pub health_retries: Option<u32>,
    /// Time after the start in which failures do not count
    #[arg(long = "health-start-period", value_name = "DURATION")]
    pub health_start_period: Option<String>,
    /// Time between checks in the start period
    #[arg(long = "health-start-interval", value_name = "DURATION")]
    pub health_start_interval: Option<String>,
    /// No health check, not even the image's
    #[arg(long = "no-healthcheck")]
    pub no_healthcheck: bool,
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
    /// Mount a volume or host path (SOURCE:TARGET[:ro], or TARGET for a new volume)
    #[arg(short = 'v', long = "volume")]
    pub volume: Vec<String>,
    /// Mount a tmpfs (TARGET[:OPTIONS])
    #[arg(long)]
    pub tmpfs: Vec<String>,
    /// Connect to a network: bridge (default), host, none, or a network name; given
    /// again, the container joins further bridge networks too (eth1, eth2, …)
    #[arg(long, alias = "net")]
    pub network: Vec<String>,
    /// A further name on the container's (first) network, for the other containers there
    #[arg(long = "network-alias", alias = "net-alias", value_name = "ALIAS")]
    pub network_alias: Vec<String>,
    /// Publish a port ([IP:][HOST_PORT:]CONTAINER_PORT[/tcp|udp])
    #[arg(short = 'p', long = "publish")]
    pub publish: Vec<String>,
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

/// A signal by name (`TERM`, `SIGTERM`) or number.
#[cfg(unix)]
pub fn parse_signal(s: &str) -> Option<i32> {
    podbag::signals::parse(s)
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

    #[cfg(unix)]
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
