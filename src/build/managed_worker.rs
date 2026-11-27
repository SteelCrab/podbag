//! A private BuildKit worker shared by CLI builds and the opt-in Engine API.

use crate::{Error, ErrorKind, Result};
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Workspace {
    work: tempfile::TempDir,
    _socket_dir: tempfile::TempDir,
    socket: PathBuf,
}

impl Workspace {
    pub fn new(data: &Path) -> Result<Self> {
        let builds = data.join("builds");
        fs::create_dir_all(&builds).map_err(|e| io_error("create the build directory", e))?;
        let work = tempfile::Builder::new()
            .prefix("buildkit-")
            .tempdir_in(builds)
            .map_err(|e| io_error("create private BuildKit state", e))?;
        let socket_dir = tempfile::Builder::new()
            .prefix("podbag-bk-")
            .tempdir_in("/tmp")
            .map_err(|e| io_error("create the private BuildKit socket directory", e))?;
        let socket = socket_dir.path().join("buildkit.sock");
        Ok(Self {
            work,
            _socket_dir: socket_dir,
            socket,
        })
    }

    pub fn path(&self) -> &Path {
        self.work.path()
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn address(&self) -> String {
        format!("unix://{}", self.socket.display())
    }

    pub fn start(&self) -> Result<Process> {
        let log = self.work.path().join("buildkitd.log");
        let mut daemon = start_daemon(self.work.path(), &self.address(), &log)?;
        wait_for_socket(&mut daemon, &self.socket, &log)?;
        Ok(daemon)
    }
}

// The worker's snapshots can contain subordinate user IDs. Shut down and remove its
// state from the same user namespace, where those paths can still be removed.
const ROOTLESS_WORKER: &str = r#"
state=$1
shift
"$@" &
worker=$!
cleanup() {
    if [ -d "$state" ]; then
        find "$state" -xdev -depth -delete
    fi
}
stop() {
    trap '' INT TERM HUP
    kill -TERM 0 2>/dev/null || :
    wait "$worker" 2>/dev/null || :
    cleanup
    exit 143
}
trap stop INT TERM HUP
wait "$worker"
status=$?
trap '' INT TERM HUP
cleanup
exit "$status"
"#;

fn start_daemon(work: &Path, addr: &str, log: &Path) -> Result<Process> {
    // SAFETY: geteuid has no pointer arguments or side effects.
    let rootless = unsafe { libc::geteuid() } != 0;
    let config = work.join("buildkitd.toml");
    fs::write(&config, "").map_err(|e| io_error("create the private BuildKit config", e))?;
    let mut command = if rootless {
        let mut cmd = Command::new("rootlesskit");
        cmd.args(["--net=pasta", "--disable-host-loopback", "--copy-up=/etc"])
            .arg("--state-dir")
            .arg(work.join("rootless"))
            .args(["/bin/sh", "-c", ROOTLESS_WORKER, "podbag-buildkit"])
            .arg(work.join("state"))
            .args(["buildkitd", "--rootless", "--oci-worker-net=host"]);
        cmd
    } else {
        let mut cmd = Command::new("buildkitd");
        cmd.arg("--oci-worker-net=bridge");
        cmd
    };
    command
        .env_clear()
        .envs(std::env::vars_os().filter(|(key, _)| {
            matches!(
                key.to_str(),
                Some(
                    "PATH"
                        | "HOME"
                        | "USER"
                        | "LOGNAME"
                        | "LANG"
                        | "LC_ALL"
                        | "XDG_RUNTIME_DIR"
                        | "HTTP_PROXY"
                        | "HTTPS_PROXY"
                        | "NO_PROXY"
                        | "http_proxy"
                        | "https_proxy"
                        | "no_proxy"
                )
            )
        }))
        .args([
            "--addr",
            addr,
            "--oci-worker=true",
            "--containerd-worker=false",
            "--cdi-disabled",
        ])
        .arg("--oci-worker-snapshotter=native")
        .arg("--root")
        .arg(work.join("state"))
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(log).map_err(|e| io_error("create the BuildKit daemon log", e))?);
    spawn(&mut command).map(|child| Process { child })
        .map_err(|e| Error::io(
            "cannot start the private BuildKit backend",
            &e,
            "install buildkitd, buildctl and runc; rootless builds also need RootlessKit and pasta",
        ))
}

fn wait_for_socket(daemon: &mut Process, socket: &Path, log: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if socket.metadata().is_ok_and(|m| m.file_type().is_socket()) {
            return Ok(());
        }
        let exited = daemon
            .child
            .try_wait()
            .map_err(|e| io_error("check the BuildKit worker", e))?;
        if exited.is_some() || Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::Runtime,
                "the private BuildKit backend did not start",
                read_tail(log),
                "check the BuildKit worker and its Linux prerequisites",
            ));
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

pub fn spawn(command: &mut Command) -> io::Result<Child> {
    let parent = std::process::id() as i32;
    command.process_group(0);
    // SAFETY: the child uses only async-signal-safe calls before exec.
    unsafe {
        command.pre_exec(move || {
            crate::signals::unblock_all();
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        })
    };
    command.spawn()
}

pub struct Process {
    child: Child,
}

impl Process {
    pub(crate) fn running(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        let pid = self.child.id() as i32;
        // SAFETY: we own this living child and its private process group.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: this is still the unreaped child process group.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

pub fn read_tail(path: &Path) -> String {
    std::fs::File::open(path)
        .and_then(|mut file| {
            use std::io::Read;
            let len = file.metadata()?.len();
            file.seek(SeekFrom::Start(len.saturating_sub(4096)))?;
            let mut bytes = Vec::new();
            file.take(4096).read_to_end(&mut bytes)?;
            Ok(String::from_utf8_lossy(&bytes).trim().to_string())
        })
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "the process returned no diagnostics".into())
}

fn io_error(what: &str, error: io::Error) -> Error {
    Error::io(
        format!("cannot {what}"),
        &error,
        "check permissions and disk space",
    )
}
