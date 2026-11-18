//! `openpod system service`: the Docker Engine API on a Unix socket.

use std::path::PathBuf;
use std::time::Duration;

pub fn service(socket: Option<PathBuf>, timeout: u64, print_units: bool) -> i32 {
    // Before anything else: the socket systemd passed, if any (no threads yet).
    let activated = openpod::api::activation_listener();
    let dirs = match openpod::paths::dirs() {
        Ok(d) => d,
        Err(e) => return crate::image::report(&e),
    };
    let socket = socket.unwrap_or_else(|| openpod::api::default_socket(&dirs));
    if print_units {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("openpod"));
        // SAFETY: geteuid has no preconditions.
        let rootless = unsafe { libc::geteuid() } != 0;
        print!("{}", openpod::api::units(&socket, &exe, rootless));
        return 0;
    }
    let opts = openpod::api::ServeOptions {
        socket,
        timeout: (timeout > 0).then(|| Duration::from_secs(timeout)),
        activated,
    };
    match openpod::api::serve(opts) {
        Ok(()) => 0,
        Err(e) => crate::image::report(&e),
    }
}
