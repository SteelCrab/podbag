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

    // Hold signals until the helper is running, so none is lost in between. Signals that
    // another process sends to the CLI are forwarded to the helper, like `docker run`.
    // Ctrl+C from the terminal is not forwarded: it reaches the container directly.
    let forwarded = [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP];
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
