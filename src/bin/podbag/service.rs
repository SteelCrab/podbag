//! `podbag system service`: the Docker Engine API on a Unix socket.

use std::path::PathBuf;
use std::time::Duration;

pub fn service(socket: Option<PathBuf>, timeout: u64, print_units: bool) -> i32 {
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
    };
    match podbag::api::serve(opts) {
        Ok(()) => 0,
        Err(e) => crate::image::report(&e),
    }
}
