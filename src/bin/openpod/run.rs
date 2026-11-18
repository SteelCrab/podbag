use crate::args::{RunArgs, parse_env};
use openpod::{Container, ErrorKind, Image};

/// `openpod run`. Returns the process exit code.
pub fn run(args: RunArgs) -> i32 {
    match start_and_wait(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("openpod: {e}");
            e.exit_code()
        }
    }
}

/// `openpod create`: the container is made but not started. Prints its ID.
pub fn create(args: RunArgs) -> i32 {
    match build(args).and_then(|c| c.create()) {
        Ok(handle) => {
            println!("{}", handle.id());
            0
        }
        Err(e) => {
            eprintln!("openpod: {e}");
            e.exit_code()
        }
    }
}

fn start_and_wait(args: RunArgs) -> openpod::Result<i32> {
    // With -it, keystrokes go to the container as typed (raw mode); restored on return.
    let raw = args.tty && args.interactive && !args.detach;
    let detach = args.detach;
    let c = build(args)?;
    if detach {
        let handle = c.start()?;
        println!("{}", handle.id());
        return Ok(0);
    }
    foreground(raw, |_| c.spawn_inherit())
}

/// Run `start` with this terminal: raw mode for `-it`, signals forwarded to the helper.
pub fn foreground(
    raw: bool,
    start: impl FnOnce(()) -> openpod::Result<openpod::Running>,
) -> openpod::Result<i32> {
    // Hold signals until the helper is running, so none is lost in between. Signals that
    // another process sends to the CLI are forwarded to the helper, like `docker run`.
    // Ctrl+C from the terminal is not forwarded: it reaches the container directly.
    let forwarded = [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP];
    let _raw = raw.then(raw_terminal).flatten();
    openpod::signals::block(&forwarded);
    match start(()) {
        Ok(r) => {
            r.forward_signals(&forwarded);
            r.wait()
        }
        Err(e) => {
            openpod::signals::ignore(&forwarded);
            Err(e)
        }
    }
}

/// The container `run` and `create` describe.
fn build(args: RunArgs) -> openpod::Result<Container> {
    // Read before the arguments are taken apart below.
    let health = health_settings(&args)?;
    let mut c = match args.rootfs {
        Some(rootfs) => Container::from_rootfs(rootfs).command(args.args),
        None => {
            let (name, command) = args
                .args
                .split_first()
                .expect("clap requires at least one argument");
            let image = match Image::get(name) {
                Ok(image) => image,
                Err(e) if e.kind == ErrorKind::NotFound => Image::pull_with_progress(name)?,
                Err(e) => return Err(e),
            };
            let c = Container::from_image(&image);
            if command.is_empty() {
                c
            } else {
                c.command(command.iter().cloned())
            }
        }
    };
    c = c
        .interactive(args.interactive)
        .remove_on_exit(args.rm)
        .privileged(args.privileged)
        .tty(args.tty);
    if let Some(policy) = &args.restart {
        c = c.restart(policy.parse()?);
    }
    if let Some(settings) = health {
        c = c.health(settings);
    }
    if let Some(n) = args.name {
        c = c.name(n);
    }
    if let Some(u) = args.user {
        c = c.user(u);
    }
    if let Some(e) = args.entrypoint {
        c = c.entrypoint(if e.is_empty() { vec![] } else { vec![e] });
    }
    for (k, v) in args
        .env
        .iter()
        .filter_map(|e| parse_env(e, |k| std::env::var(k).ok()))
    {
        c = c.env(k, v);
    }
    if let Some(w) = args.workdir {
        c = c.workdir(w);
    }
    if let Some(h) = args.hostname {
        c = c.hostname(h);
    }
    if let Some(m) = args.memory {
        c = c.memory(openpod::cgroup::parse_memory(&m)?);
    }
    if let Some(n) = args.cpus {
        c = c.cpus(n);
    }
    if let Some(n) = args.pids_limit {
        c = c.pids_limit(n);
    }
    for name in args.cap_add {
        c = c.cap_add(name);
    }
    for name in args.cap_drop {
        c = c.cap_drop(name);
    }
    let mut networks = args.network.iter();
    if let Some(n) = networks.next() {
        c = c.network(openpod::network::parse_network_arg(n)?);
    }
    for n in networks {
        // `host` and `none` stay as given: the check refuses them with a reason.
        let name = match openpod::network::parse_network_arg(n)? {
            openpod::network::NetworkMode::Bridge { network } => network,
            _ => n.clone(),
        };
        c = c.extra_network(name, Vec::new());
    }
    for alias in &args.network_alias {
        c = c.network_alias(alias.clone());
    }
    for p in &args.publish {
        c = c.publish(openpod::network::parse_publish_arg(p)?);
    }
    for v in &args.volume {
        c = c.mount(openpod::volume::parse_volume_arg(v)?);
    }
    for t in &args.tmpfs {
        c = c.mount(openpod::volume::parse_tmpfs_arg(t)?);
    }
    for opt in &args.security_opt {
        c = match openpod::spec::parse_security_opt(opt)? {
            openpod::spec::SecurityOpt::NoNewPrivileges(yes) => c.no_new_privileges(yes),
            openpod::spec::SecurityOpt::SeccompUnconfined => c.seccomp(false),
        };
    }
    Ok(c)
}

/// Put this terminal in raw mode for `-t`; restored when the value is dropped.
#[cfg(target_os = "linux")]
pub fn raw_terminal() -> Option<openpod::linux::pty::RawMode> {
    openpod::linux::pty::RawMode::enable(0)
}

#[cfg(not(target_os = "linux"))]
pub fn raw_terminal() -> Option<()> {
    None
}

/// The `--health-*` options: None when none was given (the image's check, if any).
fn health_settings(args: &RunArgs) -> openpod::Result<Option<openpod::health::HealthSettings>> {
    use openpod::health::{HealthSettings, parse_duration};
    let given = args.health_cmd.is_some()
        || args.health_interval.is_some()
        || args.health_timeout.is_some()
        || args.health_retries.is_some()
        || args.health_start_period.is_some()
        || args.health_start_interval.is_some();
    if args.no_healthcheck {
        if given {
            return Err(openpod::Error::new(
                openpod::ErrorKind::InvalidInput,
                "cannot run the container",
                "--no-healthcheck does not go with the --health options",
                "drop one of them",
            ));
        }
        return Ok(Some(HealthSettings::none()));
    }
    if !given {
        return Ok(None);
    }
    let ms = |text: &Option<String>| -> openpod::Result<Option<u64>> {
        text.as_deref()
            .map(|t| parse_duration(t).map(|d| d.as_millis() as u64))
            .transpose()
    };
    Ok(Some(HealthSettings {
        test: args
            .health_cmd
            .as_ref()
            .map(|cmd| vec!["CMD-SHELL".to_string(), cmd.clone()])
            .unwrap_or_default(),
        interval_ms: ms(&args.health_interval)?,
        timeout_ms: ms(&args.health_timeout)?,
        retries: args.health_retries,
        start_period_ms: ms(&args.health_start_period)?,
        start_interval_ms: ms(&args.health_start_interval)?,
    }))
}
