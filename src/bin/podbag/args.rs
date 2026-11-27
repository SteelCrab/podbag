use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "podbag", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Install or run a hands-on volume lesson
    Lab {
        #[command(subcommand)]
        cmd: LabCmd,
    },
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
        #[arg(short, long, allow_negative_numbers = true)]
        time: Option<i64>,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Download an image from a registry
    Pull { image: String },
    /// Upload an image to a registry
    Push { image: String },
    /// Give an image another name, like REGISTRY/NAME:TAG for a push
    Tag { source: String, target: String },
    /// Log in to a registry (Docker Hub when none is given)
    Login {
        /// The registry, like ghcr.io or localhost:5000
        server: Option<String>,
        #[arg(short, long)]
        username: Option<String>,
        /// The password (shows in the process list: prefer --password-stdin)
        #[arg(short, long)]
        password: Option<String>,
        /// Read the password from stdin
        #[arg(long)]
        password_stdin: bool,
    },
    /// Log out from a registry (Docker Hub when none is given)
    Logout { server: Option<String> },
    /// Search Docker Hub (or REGISTRY/TERM) for images
    Search {
        term: String,
        /// The most results to show
        #[arg(long, default_value_t = 25)]
        limit: u32,
        /// Show whole descriptions
        #[arg(long)]
        no_trunc: bool,
    },
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
    /// Build an image from a Dockerfile
    Build(BuildArgs),
    /// Make an image from a container's changes
    Commit {
        /// A Dockerfile instruction for the image's config (CMD, ENV, EXPOSE, …)
        #[arg(short, long = "change")]
        change: Vec<String>,
        /// Ignored: podbag keeps no image history
        #[arg(short, long)]
        message: Option<String>,
        /// Ignored: podbag keeps no image history
        #[arg(short, long)]
        author: Option<String>,
        container: String,
        /// The new image's name (REPOSITORY[:TAG])
        image: Option<String>,
    },
    /// Write a container's files (without its volumes) as a tar archive
    Export {
        /// Write to this file instead of stdout
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
        container: String,
    },
    /// List the files a container changed: A added, C changed, D deleted
    Diff { container: String },
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
        #[arg(short, long, allow_negative_numbers = true)]
        time: Option<i64>,
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
    /// Copy a container, image or volume between engines and verify the destination
    Migrate {
        #[arg(long, value_parser = parse_engine)]
        from: podbag::client::Choice,
        #[arg(long, value_parser = parse_engine)]
        to: podbag::client::Choice,
        /// Use the destination logging driver and process limits for the copied container
        #[arg(long)]
        destination_defaults: bool,
        #[arg(value_parser = ["container", "image", "volume"])]
        kind: String,
        name: String,
    },
    #[command(flatten)]
    Dashboard(crate::control::Cmd),
    /// Manage containers, images, volumes and networks in the terminal
    ///
    /// A client of the Docker API: podbag, Docker, or a socket (the saved choice, else
    /// DOCKER_HOST when it is a Unix socket, else podbag). Press ? for the keys.
    Tui {
        /// Draw one screen as text, without a terminal, and exit
        #[arg(long, hide = true)]
        once: bool,
        /// The engine for this run: podbag, docker, podman, kiri, context:NAME or unix:///path
        #[arg(long, value_name = "ENGINE", value_parser = parse_engine)]
        engine: Option<podbag::client::Choice>,
        /// Leave the mouse to the terminal (press m to change it)
        #[arg(long)]
        no_mouse: bool,
    },
    /// Show the resource use of containers (default: the running ones)
    Stats {
        /// Print one table and end
        #[arg(long)]
        no_stream: bool,
        /// Show all containers (default: only running ones)
        #[arg(short, long)]
        all: bool,
        containers: Vec<String>,
    },
    /// Show the processes of a running container (ps options are passed to the host's ps)
    Top {
        container: String,
        /// ps options, like -o pid,comm
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "PS OPTIONS"
        )]
        ps_args: Vec<String>,
    },
    /// Freeze every process of containers
    Pause {
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Let the processes of paused containers run again
    Unpause {
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Rename a container
    Rename { container: String, new_name: String },
    /// Change the resource limits and restart policy of containers
    Update(UpdateArgs),
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
    /// The init that holds a Kubernetes pod's PID namespace
    #[command(name = "__pause", hide = true)]
    PodInit,
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
    /// Manage the Podbag VM. Run podbag vm --help for platform-specific commands
    #[cfg(any(target_os = "macos", windows))]
    #[command(disable_help_flag = true)]
    Vm {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Replace podbag with a newer release from GitHub
    ///
    /// The download must match the release's SHA256SUMS before it runs; the old program
    /// stays as podbag.old.
    Upgrade {
        /// Only say whether a newer release exists
        #[arg(long)]
        check: bool,
        /// Install this release (the only way to go back to an older one)
        #[arg(long, value_name = "X.Y.Z")]
        version: Option<String>,
        /// Do not ask before replacing podbag
        #[arg(short, long)]
        yes: bool,
        /// With --check: print the result as JSON (for apps)
        #[arg(long)]
        json: bool,
    },
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
pub enum LabCmd {
    #[cfg(unix)]
    #[command(hide = true)]
    Workspace {
        #[arg(long)]
        containers: bool,
    },
    #[cfg(unix)]
    #[command(hide = true)]
    DesktopView {
        library: std::path::PathBuf,
        action: String,
        session: Option<String>,
        name: Option<String>,
        #[arg(long, conflicts_with = "course")]
        containers: bool,
        #[arg(long)]
        course: bool,
    },
    /// Install the downloaded .podbag module
    Install { file: std::path::PathBuf },
    /// List installed practice modules
    List,
    /// Open a shell in a new temporary practice store
    Start {
        module: Option<String>,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long, hide = true)]
        library: Option<std::path::PathBuf>,
    },
    /// Remove the installed module and its progress
    Remove { module: Option<String> },
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
    /// Read every stored file and check it against its hash
    ///
    /// Exits with 1 when a file is damaged. Damaged files cannot be repaired in place:
    /// remove the images that use them (--fix), then pull or load them again.
    Verify {
        /// Remove the images that use damaged files (not those a container uses)
        #[arg(long)]
        fix: bool,
    },
    /// Compress images that no container uses; with no image named, those unused for compress_idle_after
    Compress { images: Vec<String> },
    /// Decompress images now, so that their next container starts at once
    Decompress {
        #[arg(required = true)]
        images: Vec<String>,
    },
    /// Show or change image compression (idle: compress images that no container used for a while)
    Compression {
        /// Save this mode in config.toml
        #[arg(value_parser = ["idle", "off"])]
        mode: Option<String>,
        /// Save how long an image stays unused before it is compressed (7d, 36h, 90m, 45s)
        #[arg(long, value_name = "AGE")]
        after: Option<String>,
        /// Print the state as JSON
        #[arg(long)]
        json: bool,
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
    /// Connect stdin and stdout to the Docker API socket
    ///
    /// For programs that cannot reach the socket themselves, such as the desktop app on
    /// Windows. It starts the service when none runs.
    DialStdio {
        /// Connect to this existing Unix socket
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Where the docker command goes, and switch it to podbag or back
    Docker {
        #[command(subcommand)]
        cmd: DockerCmd,
    },
}

#[derive(Subcommand)]
pub enum DockerCmd {
    /// Show where the docker command goes (podbag, Docker Desktop, OrbStack, …)
    Status,
    /// Make the docker command use podbag (a docker context named podbag)
    ///
    /// It creates the context when it is missing and remembers the context in use.
    /// Other contexts are never changed or removed.
    Use,
    /// Go back to the context in use before `podbag system docker use`
    Restore,
}

#[derive(Args)]
pub struct UpdateArgs {
    /// Block I/O weight, 10 to 1000
    #[arg(long)]
    pub blkio_weight: Option<u16>,
    /// The CPU period in microseconds (with --cpu-quota)
    #[arg(long)]
    pub cpu_period: Option<i64>,
    /// CPU time in microseconds per period (-1: no limit)
    #[arg(long, allow_hyphen_values = true)]
    pub cpu_quota: Option<i64>,
    /// CPU shares, a relative weight (default 1024)
    #[arg(short = 'c', long)]
    pub cpu_shares: Option<i64>,
    /// Number of CPUs (like 0.5)
    #[arg(long)]
    pub cpus: Option<f64>,
    /// CPUs the container may use (0-3, 0,1)
    #[arg(long)]
    pub cpuset_cpus: Option<String>,
    /// Memory nodes the container may use (0-3, 0,1)
    #[arg(long)]
    pub cpuset_mems: Option<String>,
    /// Memory limit (like 512m or 2g)
    #[arg(short = 'm', long)]
    pub memory: Option<String>,
    /// Memory the kernel tries to leave the container (like 256m)
    #[arg(long)]
    pub memory_reservation: Option<String>,
    /// Memory plus swap (-1: swap without limit)
    #[arg(long, allow_hyphen_values = true)]
    pub memory_swap: Option<String>,
    /// Process limit (0 or less: no limit)
    #[arg(long, allow_hyphen_values = true)]
    pub pids_limit: Option<i64>,
    /// Restart policy: no, always, unless-stopped, on-failure[:max]
    #[arg(long, value_name = "POLICY")]
    pub restart: Option<String>,
    #[arg(required = true)]
    pub containers: Vec<String>,
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
pub struct BuildArgs {
    /// Name the image (REPOSITORY[:TAG]); may be given more than once
    #[arg(short, long = "tag")]
    pub tag: Vec<String>,
    /// The Dockerfile (default: CONTEXT/Dockerfile)
    #[arg(short, long)]
    pub file: Option<String>,
    /// A build argument NAME=VALUE (NAME alone takes this environment's value)
    #[arg(long = "build-arg")]
    pub build_arg: Vec<String>,
    /// The stage to build
    #[arg(long)]
    pub target: Option<String>,
    /// Do not use the build cache
    #[arg(long)]
    pub no_cache: bool,
    /// Pull base images even when they are stored
    #[arg(long)]
    pub pull: bool,
    /// A label KEY=VALUE for the image
    #[arg(long)]
    pub label: Vec<String>,
    /// The network of RUN steps: bridge (default), host, none, or a network's name
    #[arg(long)]
    pub network: Option<String>,
    /// Print only the image ID
    #[arg(short, long)]
    pub quiet: bool,
    /// A directory, or - for a tar archive (or a Dockerfile) on stdin
    pub context: String,
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
    /// Mount the container's root filesystem read-only
    #[arg(long)]
    pub read_only: bool,
    /// Map a device as root on Linux (HOST[:CONTAINER][:rwm])
    #[arg(long = "device", value_name = "DEVICE")]
    pub devices: Vec<podbag::spec::Device>,
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
    /// Add a line to /etc/hosts (NAME:IP or NAME=IP; IP may be host-gateway)
    #[arg(long = "add-host", value_name = "NAME:IP")]
    pub add_host: Vec<String>,
    /// Use this DNS server (replaces the default ones)
    #[arg(long, value_name = "IP")]
    pub dns: Vec<String>,
    /// Use this DNS search domain ("." for none)
    #[arg(long = "dns-search", value_name = "DOMAIN")]
    pub dns_search: Vec<String>,
    /// Set a DNS resolver option (like ndots:2)
    #[arg(long = "dns-option", alias = "dns-opt", value_name = "OPTION")]
    pub dns_option: Vec<String>,
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

/// `--engine`: podbag, docker, or a socket.
pub(crate) fn parse_engine(text: &str) -> Result<podbag::client::Choice, String> {
    podbag::client::Choice::parse(text)
        .ok_or_else(|| "use podbag, docker, podman, kiri, context:NAME or unix:///path".to_string())
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
