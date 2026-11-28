//! The private Docker socket used only while Kind is working on a local cluster.
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::host;
use crate::vm::proto::KIND_API_PORT;

pub(crate) struct Socket {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
}

impl Socket {
    pub(crate) fn open() -> Result<Self, String> {
        let control = host::paths()?.vm_sock;
        connect(&control).map_err(|e| format!(
            "The Podbag VM does not accept local Kubernetes requests: {e}. Update the VM guest and try again."
        ))?;
        // Short enough for Darwin's Unix socket path limit. The directory is private.
        let dir = tempfile::Builder::new()
            .prefix("podbag-kind-")
            .tempdir_in("/tmp")
            .map_err(|e| e.to_string())?;
        let socket =
            UnixListener::bind(dir.path().join("docker.sock")).map_err(|e| e.to_string())?;
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let listener = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match socket.accept() {
                    Ok((client, _)) => {
                        let control = control.clone();
                        thread::spawn(move || {
                            if let Ok(guest) = connect(&control) {
                                splice(client, guest);
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            dir,
            stop,
            listener: Some(listener),
        })
    }

    pub(crate) fn host(&self) -> String {
        format!("unix://{}", self.dir.path().join("docker.sock").display())
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

fn connect(control: &Path) -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(control)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "PORT {KIND_API_PORT}")?;
    let mut reply = [0; 3];
    stream.read_exact(&mut reply)?;
    if &reply != b"OK\n" {
        return Err(std::io::Error::other("unexpected response from VM host"));
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

fn splice(mut client: UnixStream, mut guest: UnixStream) {
    // Darwin passes the nonblocking listener mode to the accepted stream.
    if client.set_nonblocking(false).is_err() {
        return;
    }
    let (Ok(mut from_client), Ok(mut to_guest)) = (client.try_clone(), guest.try_clone()) else {
        return;
    };
    thread::scope(|scope| {
        scope.spawn(move || {
            let _ = std::io::copy(&mut from_client, &mut to_guest);
            let _ = to_guest.shutdown(Shutdown::Write);
        });
        let _ = std::io::copy(&mut guest, &mut client);
        let _ = client.shutdown(Shutdown::Write);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_both_directions_when_the_listener_was_nonblocking() {
        let (proxy_client, mut client) = UnixStream::pair().unwrap();
        let (proxy_guest, mut guest) = UnixStream::pair().unwrap();
        proxy_client.set_nonblocking(true).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let relay = thread::spawn(move || splice(proxy_client, proxy_guest));
        thread::sleep(Duration::from_millis(25));
        client.write_all(b"request").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut request = String::new();
        guest.read_to_string(&mut request).unwrap();
        assert_eq!(request, "request");
        guest.write_all(b"response").unwrap();
        guest.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert_eq!(response, "response");
        relay.join().unwrap();
    }
}
