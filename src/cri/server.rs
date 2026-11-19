//! `podbag cri serve`: the CRI socket a kubelet connects to.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::Arc;

use super::grpc;
use super::h2::{self, Handler};
use super::service::{Cri, Options};
use crate::{Error, ErrorKind, Result};

/// Serves the CRI until the process is stopped. Runs as root: the kubelet does, and pods
/// need root's network and cgroups.
pub fn serve(options: Options) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "podbag cri serve",
            "the CRI needs root",
            "run it as root, like the kubelet",
        ));
    }
    let socket = options.socket.clone();
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::io(format!("make {}", dir.display()), &e, ""))?;
    }
    // A socket left from an earlier run.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).map_err(|e| {
        Error::io(
            format!("listen on {}", socket.display()),
            &e,
            "check that no other runtime uses it",
        )
    })?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))
        .map_err(|e| Error::io(format!("set the mode of {}", socket.display()), &e, ""))?;
    eprintln!("podbag cri: serving the CRI on {}", socket.display());

    let handler: Arc<dyn Handler> = Arc::new(grpc::Server(Arc::new(Cri::new(options))));
    for conn in listener.incoming() {
        match conn {
            Ok(conn) => {
                let handler = Arc::clone(&handler);
                let _ = std::thread::Builder::new()
                    .name("cri-conn".into())
                    .spawn(move || {
                        if let Err(e) = h2::serve(conn, handler) {
                            eprintln!("podbag cri: a connection ended: {e}");
                        }
                    });
            }
            Err(e) => eprintln!("podbag cri: accept: {e}"),
        }
    }
    Ok(())
}
