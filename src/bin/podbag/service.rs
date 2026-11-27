//! `podbag system service`: the Docker Engine API on a Unix socket.

use std::path::PathBuf;
use std::time::Duration;

pub fn service(socket: Option<PathBuf>, timeout: u64, print_units: bool, buildkit: bool) -> i32 {
    // Before anything else: the socket systemd passed, if any (no threads yet).
    let activated = podbag::api::activation_listener();
    let dirs = match podbag::paths::dirs() {
        Ok(d) => d,
        Err(e) => return crate::image::report(&e),
    };
    let socket = socket.unwrap_or_else(|| podbag::api::default_socket(&dirs));
    if print_units {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("podbag"));
        // SAFETY: geteuid has no preconditions.
        let rootless = unsafe { libc::geteuid() } != 0;
        print!("{}", podbag::api::units(&socket, &exe, rootless));
        return 0;
    }
    let opts = podbag::api::ServeOptions {
        socket,
        timeout: (timeout > 0).then(|| Duration::from_secs(timeout)),
        activated,
        managed_buildkit: buildkit,
    };
    match podbag::api::serve(opts) {
        Ok(()) => 0,
        Err(e) => crate::image::report(&e),
    }
}

/// `podbag system dial-stdio`: stdin and stdout as one connection to the API socket,
/// until the socket closes.
pub fn dial_stdio(socket: Option<PathBuf>) -> i32 {
    use podbag::client::{Endpoint, podbag_program};
    use std::io::Write;
    let exe = podbag_program();
    let endpoint = match socket {
        Some(path) => Ok(Endpoint::Socket { path, own: false }),
        None => Endpoint::find(&exe),
    };
    let path = match endpoint.and_then(|e| {
        e.ensure_running(&exe, true)?;
        match e {
            Endpoint::Socket { path, .. } => Ok(path),
            Endpoint::Command(_) => Err("the Docker API is not on a socket here".into()),
        }
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("podbag: {e}");
            return 1;
        }
    };
    let socket = match std::os::unix::net::UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("podbag: cannot connect to {}: {e}", path.display());
            return 1;
        }
    };
    let Ok(mut to_socket) = socket.try_clone() else {
        return 1;
    };
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_socket);
        let _ = to_socket.shutdown(std::net::Shutdown::Write);
    });
    let mut out = std::io::stdout().lock();
    let mut from_socket = socket;
    let copied = std::io::copy(&mut from_socket, &mut out);
    let _ = out.flush();
    i32::from(copied.is_err())
}
