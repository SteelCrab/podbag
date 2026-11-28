//! Run BuildKit as a private, per-build OCI worker.

use super::managed_worker::{Workspace, read_tail};
use super::{BuildOptions, Built};
use crate::image::{Image, reference::Reference};
use crate::{Error, ErrorKind, Result};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

mod config;

#[derive(Default)]
pub struct Options<'a> {
    pub platform: Option<&'a str>,
    pub output: Option<&'a str>,
    pub secrets: &'a [String],
    pub ssh: &'a [String],
    pub cache_from: &'a [String],
    pub cache_to: &'a [String],
    pub registry: RegistryConfig,
}

#[derive(Default)]
pub enum RegistryConfig {
    #[default]
    Inherit,
    Request,
}

/// The image or file published by an exporter.
pub enum ResultOutput {
    Image(Built),
    Path(PathBuf),
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

pub fn run(
    build: &BuildOptions,
    options: Options<'_>,
    run_client: impl FnOnce(&mut Command) -> Result<ExitStatus>,
) -> Result<ResultOutput> {
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
            .is_some_and(|p| p.contains(',') || !crate::build::steps::is_this_platform(p))
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
    let workspace = Workspace::new(&crate::paths::dirs()?.data)?;
    let work = workspace.path();
    let sources = work.join("dockerfile");
    fs::create_dir(&sources).map_err(|e| io_error("stage the Dockerfile", e))?;
    fs::write(sources.join("Dockerfile"), &build.dockerfile)
        .and_then(|()| fs::write(sources.join("Dockerfile.dockerignore"), &build.ignore))
        .map_err(|e| io_error("stage the Dockerfile and ignore rules", e))?;

    let archive = work.join("image.tar");
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
        // Import by digest before adding mutable tags. Concurrent builds can reuse a tag.
        Output::Load => export_args("docker", &archive, None),
        Output::Oci(_) => export_args("oci", &archive, tags.first()),
        Output::Local(_) => export_args("local", staging.as_ref().unwrap().path(), None),
    };
    let addr = workspace.address();
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

    let daemon = workspace.start()?;
    let progress = work.join("progress.log");
    if build.quiet {
        client
            .stdout(Stdio::null())
            .stderr(File::create(&progress).map_err(|e| io_error("create the build log", e))?);
    }
    let _credentials = match options.registry {
        RegistryConfig::Inherit => None,
        RegistryConfig::Request => Some(config::configure(
            &mut client,
            work,
            &build.registry_logins,
        )?),
    };
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
    Ok(match export {
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
            let mut names = loaded.names;
            for name in tags {
                if !names.contains(&name) {
                    Image::tag(&id, &name)?;
                    names.push(name);
                }
            }
            ResultOutput::Image(Built { id, names })
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
            ResultOutput::Path(destination.into())
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
            ResultOutput::Path(destination.into())
        }
    })
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
