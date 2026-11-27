//! Run BuildKit as a private, per-build OCI worker.

use podbag::build::BuildOptions;
use podbag::image::{Image, reference::Reference};
use podbag::{Error, ErrorKind, Result};
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub struct Options<'a> {
    pub platform: Option<&'a str>,
    pub output: Option<&'a str>,
    pub secrets: &'a [String],
    pub ssh: &'a [String],
    pub cache_from: &'a [String],
    pub cache_to: &'a [String],
}

enum Output<'a> {
    Load,
    Oci(&'a Path),
    Local(&'a Path),
}

fn output(value: Option<&str>) -> Result<Output<'_>> {
    let Some(value) = value else {
        return Ok(Output::Load);
    };
    match value.split_once(",dest=") {
        Some(("type=oci", path)) if !path.is_empty() => Ok(Output::Oci(Path::new(path))),
        Some(("type=local", path)) if !path.is_empty() => Ok(Output::Local(Path::new(path))),
        _ => Err(invalid(
            "invalid BuildKit output",
            "give type=oci,dest=FILE or type=local,dest=DIR",
        )),
    }
}

pub fn run(build: &BuildOptions, options: Options<'_>) -> Result<()> {
    let export = output(options.output)?;
    if let Output::Local(destination) = export
        && fs::symlink_metadata(destination).is_ok()
    {
        return Err(invalid(
            "the local BuildKit output directory already exists",
            "give a new directory for --output type=local",
        ));
    }
    let mut tags = Vec::new();
    for name in &build.tags {
        let reference = Reference::parse(name)?;
        if reference.digest.is_some() {
            return Err(invalid("invalid image tag", "give a name with a tag"));
        }
        let name = reference.name();
        if !tags.contains(&name) {
            tags.push(name);
        }
    }
    if options.platform.is_some_and(|p| p.is_empty()) {
        return Err(invalid(
            "invalid BuildKit platform",
            "give an OS and architecture",
        ));
    }
    if matches!(export, Output::Load)
        && options
            .platform
            .is_some_and(|p| p.contains(',') || !podbag::build::steps::is_this_platform(p))
    {
        return Err(invalid(
            "cannot load a foreign or multi-platform result",
            "export it with --output type=oci,dest=FILE or type=local,dest=DIR",
        ));
    }
    if (!matches!(export, Output::Load) && tags.len() > 1)
        || (matches!(export, Output::Local(_)) && !tags.is_empty())
    {
        return Err(invalid(
            "invalid image tags for this exporter",
            "use at most one tag for an OCI archive and no tag for local output",
        ));
    }
    let network = match build.network.as_deref() {
        None | Some("default" | "bridge") => None,
        Some("none") => Some("none"),
        Some(_) => {
            return Err(invalid(
                "the managed BuildKit network is unsupported",
                "use --network default or --network none",
            ));
        }
    };
    let root = podbag::paths::dirs()?.data.join("builds");
    fs::create_dir_all(&root).map_err(|e| io_error("create the build directory", e))?;
    let work = tempfile::Builder::new()
        .prefix("buildkit-")
        .tempdir_in(root)
        .map_err(|e| io_error("create private BuildKit state", e))?;
    let socket = tempfile::Builder::new()
        .prefix("podbag-bk-")
        .tempdir_in("/tmp")
        .map_err(|e| io_error("create the private BuildKit socket directory", e))?;
    let sources = work.path().join("dockerfile");
    fs::create_dir(&sources).map_err(|e| io_error("stage the Dockerfile", e))?;
    fs::write(sources.join("Dockerfile"), &build.dockerfile)
        .and_then(|()| fs::write(sources.join("Dockerfile.dockerignore"), &build.ignore))
        .map_err(|e| io_error("stage the Dockerfile and ignore rules", e))?;

    let archive = work.path().join("image.tar");
    let staging = match export {
        Output::Local(destination) => Some(
            tempfile::Builder::new()
                .prefix(".podbag-build-")
                .tempdir_in(parent(destination))
                .map_err(|e| io_error("stage the local BuildKit output", e))?,
        ),
        _ => None,
    };
    let output = match export {
        Output::Load => export_args("docker", &archive, tags.first()),
        Output::Oci(_) => export_args("oci", &archive, tags.first()),
        Output::Local(_) => export_args("local", staging.as_ref().unwrap().path(), None),
    };
    let socket_file = socket.path().join("buildkit.sock");
    let addr = format!("unix://{}", socket_file.display());
    let mut client = Command::new("buildctl");
    client
        .stdin(Stdio::null())
        .args([
            "--addr",
            &addr,
            "--timeout",
            "10",
            "build",
            "--frontend",
            "dockerfile.v0",
        ])
        .args(["--progress", "plain", "--output", &output])
        .arg("--local")
        .arg(local_arg("context", &build.context))
        .arg("--local")
        .arg(local_arg("dockerfile", &sources));
    for (key, value) in &build.build_args {
        client.args(["--opt", &format!("build-arg:{key}={value}")]);
    }
    for (key, value) in &build.labels {
        client.args(["--opt", &format!("label:{key}={value}")]);
    }
    for (key, value) in [
        ("target", build.target.as_deref()),
        ("platform", options.platform),
        ("force-network-mode", network),
        ("image-resolve-mode", build.pull.then_some("pull")),
    ] {
        if let Some(value) = value {
            client.args(["--opt", &format!("{key}={value}")]);
        }
    }
    if build.no_cache {
        client.arg("--no-cache");
    }
    for value in options.secrets {
        if !valid_secret(value) {
            return Err(invalid(
                "invalid BuildKit secret",
                "use id=NAME,src=FILE or id=NAME,env=VARIABLE",
            ));
        }
        client.args(["--secret", value]);
    }
    for value in options.ssh {
        client.args(["--ssh", &ssh_socket(value)?]);
    }
    for (flag, values, path_key) in [
        ("--import-cache", options.cache_from, "src"),
        ("--export-cache", options.cache_to, "dest"),
    ] {
        for value in values {
            if !valid_cache(value, path_key) {
                return Err(invalid(
                    "invalid BuildKit cache",
                    "use a local directory or registry reference without inline credentials",
                ));
            }
            client.args([flag, value]);
        }
    }

    let log = work.path().join("buildkitd.log");
    let mut daemon = start_daemon(work.path(), &addr, &log)?;
    wait_for_socket(&mut daemon, &socket_file, &log)?;
    let progress = work.path().join("progress.log");
    if build.quiet {
        client
            .stdout(Stdio::null())
            .stderr(File::create(&progress).map_err(|e| io_error("create the build log", e))?);
    }
    let status = run_client(&mut client)?;
    if !status.success() {
        let detail = if build.quiet {
            read_tail(&progress)
        } else {
            status.to_string()
        };
        return Err(Error::new(
            ErrorKind::Runtime,
            "BuildKit build failed",
            detail,
            "check the Dockerfile and build output",
        ));
    }
    // Close the backend before publishing the result, even if importing fails.
    drop(daemon);
    match export {
        Output::Load => {
            let mut file =
                File::open(&archive).map_err(|e| io_error("open the result archive", e))?;
            let loaded = Image::load(&mut file)?;
            let id = match loaded.untagged.first() {
                Some(id) => id.clone(),
                None => {
                    let first = loaded.names.first().ok_or_else(|| {
                        invalid(
                            "BuildKit returned no image",
                            "check the result archive exporter",
                        )
                    })?;
                    Image::get(first)?.digest
                }
            };
            for name in &tags {
                if !loaded.names.contains(name) {
                    Image::tag(&id, name)?;
                }
            }
            println!("{id}");
        }
        Output::Oci(destination) => {
            let publish = || -> io::Result<()> {
                let mut result = tempfile::NamedTempFile::new_in(parent(destination))?;
                io::copy(&mut File::open(&archive)?, &mut result)?;
                result.as_file().sync_all()?;
                result.persist(destination)?;
                Ok(())
            };
            publish().map_err(|e| io_error("publish the OCI archive", e))?;
            println!("{}", destination.display());
        }
        Output::Local(destination) => {
            rustix::fs::renameat_with(
                rustix::fs::CWD,
                staging.as_ref().unwrap().path(),
                rustix::fs::CWD,
                destination,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|e| io_error("publish the local result (use a new directory)", e.into()))?;
            println!("{}", destination.display());
        }
    }
    Ok(())
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

fn run_client(command: &mut Command) -> Result<ExitStatus> {
    let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
    podbag::signals::block(&signals);
    let status = match spawn(command) {
        Ok(mut child) => {
            podbag::signals::forward_all_to(child.id(), &signals);
            child.wait()
        }
        Err(error) => Err(error),
    };
    podbag::signals::restore_default(&signals);
    status.map_err(|e| Error::io("cannot run buildctl", &e, "install buildctl and check PATH"))
}

fn spawn(command: &mut Command) -> io::Result<Child> {
    let parent = std::process::id() as i32;
    command.process_group(0);
    // SAFETY: the child uses only async-signal-safe calls before exec.
    unsafe {
        command.pre_exec(move || {
            podbag::signals::unblock_all();
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

struct Process {
    child: Child,
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

fn valid_secret(value: &str) -> bool {
    let mut id = false;
    let mut input = false;
    for part in value.split(',') {
        let Some((key, value)) = part.split_once('=').filter(|(_, value)| !value.is_empty()) else {
            return false;
        };
        match key {
            "id" if !id && valid_id(value) => id = true,
            "src" | "env" if !input => input = true,
            _ => return false,
        }
    }
    id && input
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn ssh_socket(value: &str) -> Result<String> {
    let (id, path) = match value.split_once('=') {
        Some((id, path)) => (id, PathBuf::from(path)),
        None => (
            value,
            PathBuf::from(std::env::var_os("SSH_AUTH_SOCK").unwrap_or_default()),
        ),
    };
    let invalid = || {
        invalid(
            "invalid BuildKit SSH agent",
            "set SSH_AUTH_SOCK or give --ssh ID=/path/to/agent.sock",
        )
    };
    if !valid_id(id)
        || !path
            .metadata()
            .is_ok_and(|meta| meta.file_type().is_socket())
    {
        return Err(invalid());
    }
    let path = path
        .to_str()
        .filter(|s| !s.contains([',', '"', '\n', '\r']))
        .ok_or_else(invalid)?;
    Ok(format!("{id}={path}"))
}

fn valid_cache(value: &str, local_path: &str) -> bool {
    let fields: Vec<_> = value
        .split(',')
        .filter_map(|field| field.split_once('='))
        .collect();
    let get = |name| {
        fields
            .iter()
            .find(|(key, _)| key == &name)
            .map(|(_, val)| *val)
    };
    if fields.len() != value.split(',').count()
        || fields.iter().any(|(key, value)| {
            value.is_empty()
                || !matches!(
                    *key,
                    "type"
                        | "src"
                        | "dest"
                        | "ref"
                        | "mode"
                        | "compression"
                        | "compression-level"
                        | "force-compression"
                        | "oci-mediatypes"
                        | "image-manifest"
                        | "ignore-error"
                        | "digest"
                        | "tag"
                )
        })
    {
        return false;
    }
    if fields.iter().filter(|(key, _)| *key == "type").count() != 1 {
        return false;
    }
    match get("type") {
        Some("local") => get(local_path).is_some(),
        Some("registry") => get("ref").is_some_and(|name| Reference::parse(name).is_ok()),
        _ => false,
    }
}

fn export_args(kind: &str, destination: &Path, tag: Option<&String>) -> String {
    let field = |key: &str, value: &str| {
        let field = format!("{key}={value}");
        if field.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", field.replace('"', "\"\""))
        } else {
            field
        }
    };
    let mut value = format!(
        "type={kind},{}",
        field("dest", &destination.to_string_lossy())
    );
    if let Some(tag) = tag {
        value.push(',');
        value.push_str(&field("name", tag));
    }
    value
}

fn local_arg(key: &str, path: &Path) -> std::ffi::OsString {
    let mut arg = std::ffi::OsString::from(format!("{key}="));
    arg.push(path.as_os_str());
    arg
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn read_tail(path: &Path) -> String {
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

fn invalid(what: &str, next: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        what,
        "unsupported BuildKit options",
        next,
    )
}

fn io_error(what: &str, error: io::Error) -> Error {
    Error::io(
        format!("cannot {what}"),
        &error,
        "check permissions and disk space",
    )
}
