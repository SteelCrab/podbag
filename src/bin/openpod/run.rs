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

fn start_and_wait(args: RunArgs) -> openpod::Result<i32> {
    // With -it, keystrokes go to the container as typed (raw mode); restored on return.
    let raw = args.tty && args.interactive && !args.detach;
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
        .tty(args.tty);
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
    if args.detach {
        let handle = c.start()?;
        println!("{}", handle.id());
        return Ok(0);
    }

    // Hold signals until the helper is running, so none is lost in between. Signals that
    // another process sends to the CLI are forwarded to the helper, like `docker run`.
    // Ctrl+C from the terminal is not forwarded: it reaches the container directly.
    let forwarded = [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP];
    let _raw = raw.then(raw_terminal).flatten();
    openpod::signals::block(&forwarded);
    match c.spawn_inherit() {
        Ok(r) => {
            openpod::signals::forward_to(r.helper_pid(), &forwarded);
            r.wait()
        }
        Err(e) => {
            openpod::signals::ignore(&forwarded);
            Err(e)
        }
    }
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
