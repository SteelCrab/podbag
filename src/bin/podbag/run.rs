use crate::args::{RunArgs, parse_env};
use podbag::{Container, ErrorKind, Image};

/// `podbag run`. Returns the process exit code.
pub fn run(args: RunArgs) -> i32 {
    match start_and_wait(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("podbag: {e}");
            e.exit_code()
        }
    }
}

/// `podbag create`: the container is made but not started. Prints its ID.
pub fn create(args: RunArgs) -> i32 {
    match build(args).and_then(|c| c.create()) {
        Ok(handle) => {
            println!("{}", handle.id());
            0
        }
        Err(e) => {
            eprintln!("podbag: {e}");
            e.exit_code()
        }
    }
}

fn start_and_wait(args: RunArgs) -> podbag::Result<i32> {
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
    start: impl FnOnce(()) -> podbag::Result<podbag::Running>,
) -> podbag::Result<i32> {
    // Hold signals until the helper is running, so none is lost in between. Signals that
    // another process sends to the CLI are forwarded to the helper, like `docker run`.
    // Ctrl+C from the terminal is not forwarded: it reaches the container directly.
    let forwarded = [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP];
    let _raw = raw.then(raw_terminal).flatten();
    podbag::signals::block(&forwarded);
    match start(()) {
        Ok(r) => {
            r.forward_signals(&forwarded);
            r.wait()
        }
        Err(e) => {
            podbag::signals::ignore(&forwarded);
            Err(e)
        }
    }
}

/// The container `run` and `create` describe.
fn build(args: RunArgs) -> podbag::Result<Container> {
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
        .read_only(args.read_only)
        .tty(args.tty);
    for device in args.devices {
        c = c.device(device);
    }
    for request in args.gpus {
        c = c.gpus(request);
    }
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
        c = c.memory(podbag::cgroup::parse_memory(&m)?);
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
        c = c.network(podbag::network::parse_network_arg(n)?);
    }
    for n in networks {
        // `host` and `none` stay as given: the check refuses them with a reason.
        let name = match podbag::network::parse_network_arg(n)? {
            podbag::network::NetworkMode::Bridge { network } => network,
            _ => n.clone(),
        };
        c = c.extra_network(name, Vec::new());
    }
    for alias in &args.network_alias {
        c = c.network_alias(alias.clone());
    }
    for p in &args.publish {
        c = c.publish(podbag::network::parse_publish_arg(p)?);
    }
    for entry in &args.add_host {
        c = c.add_host(podbag::netfiles::ExtraHost::parse(entry)?);
    }
    for server in &args.dns {
        c = c.dns_server(podbag::netfiles::parse_dns_server(server)?);
    }
    for domain in &args.dns_search {
        podbag::netfiles::check_dns_search(domain)?;
        c = c.dns_search(domain.clone());
    }
    for option in &args.dns_option {
        podbag::netfiles::check_dns_option(option)?;
        c = c.dns_option(option.clone());
    }
    for v in &args.volume {
        c = c.mount(podbag::volume::parse_volume_arg(v)?);
    }
    for t in &args.tmpfs {
        c = c.mount(podbag::volume::parse_tmpfs_arg(t)?);
    }
    for opt in &args.security_opt {
        c = match podbag::spec::parse_security_opt(opt)? {
            podbag::spec::SecurityOpt::NoNewPrivileges(yes) => c.no_new_privileges(yes),
            podbag::spec::SecurityOpt::SeccompUnconfined => c.seccomp(false),
        };
    }
    Ok(c)
}

/// Put this terminal in raw mode for `-t`; restored when the value is dropped.
#[cfg(target_os = "linux")]
pub fn raw_terminal() -> Option<podbag::linux::pty::RawMode> {
    podbag::linux::pty::RawMode::enable(0)
}

#[cfg(not(target_os = "linux"))]
pub fn raw_terminal() -> Option<()> {
    None
}

/// The `--health-*` options: None when none was given (the image's check, if any).
fn health_settings(args: &RunArgs) -> podbag::Result<Option<podbag::health::HealthSettings>> {
    use podbag::health::{HealthSettings, parse_duration};
    let given = args.health_cmd.is_some()
        || args.health_interval.is_some()
        || args.health_timeout.is_some()
        || args.health_retries.is_some()
        || args.health_start_period.is_some()
        || args.health_start_interval.is_some();
    if args.no_healthcheck {
        if given {
            return Err(podbag::Error::new(
                podbag::ErrorKind::InvalidInput,
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
    let ms = |text: &Option<String>| -> podbag::Result<Option<u64>> {
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
