//! Use the host NVIDIA Container Toolkit's device plan, with Podbag's mount and cgroup rules.

use super::Request;
use crate::spec::Device;
use crate::volume::{Mount, MountSource};
use crate::{Error, ErrorKind, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const KIND: &str = "nvidia.com/gpu";
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/usr/local/sbin";
const INSTALL: &str =
    "install or update the NVIDIA driver and NVIDIA Container Toolkit on the Linux host";
// NVIDIA's June 2026 toolkit security update fixes a root filesystem race.
const MIN_VERSION: (u32, u32, u32) = (1, 19, 1);
const HOOKS: &[&str] = &[
    "create-symlinks",
    "create-soname-symlinks",
    "enable-cuda-compat",
    "update-ldcache",
    "disable-device-node-modification",
    "update-application-profile",
];

pub(crate) struct Setup {
    pub devices: Vec<Device>,
    pub mounts: Vec<Mount>,
    pub env: Vec<(String, String)>,
    hooks: Vec<Vec<String>>,
    hook_program: PathBuf,
}

#[derive(Deserialize)]
struct Spec {
    kind: String,
    devices: Vec<Entry>,
    #[serde(default, rename = "containerEdits")]
    common: Edits,
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    #[serde(default, rename = "containerEdits")]
    edits: Edits,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct Edits {
    device_nodes: Vec<Node>,
    mounts: Vec<Bind>,
    hooks: Vec<Hook>,
    env: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Node {
    path: String,
    host_path: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    major: Option<u32>,
    minor: Option<u32>,
    permissions: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bind {
    host_path: String,
    container_path: String,
    options: Vec<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hook {
    hook_name: String,
    path: String,
    args: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
}

pub(crate) fn setup(requests: &[&Request]) -> Result<Option<Setup>> {
    if requests.is_empty() {
        return Ok(None);
    }
    // SAFETY: geteuid does not have any preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "cannot request NVIDIA GPUs",
            "NVIDIA device mappings and driver mounts require root on the Linux host",
            "run podbag as root or omit the GPU request",
        ));
    }
    let toolkit = tool("nvidia-ctk")?.ok_or_else(|| {
        Error::new(
            ErrorKind::MissingRequirement,
            "cannot request NVIDIA GPUs",
            "nvidia-ctk is missing from /usr/bin and /usr/local/bin",
            INSTALL,
        )
    })?;
    let hook_program = tool("nvidia-cdi-hook")?.unwrap_or_else(|| toolkit.clone());
    check_version(&toolkit)?;
    if hook_program != toolkit {
        check_version(&hook_program)?;
    }
    let mut generate = command(&toolkit);
    generate.args([
        "cdi",
        "generate",
        "--format=json",
        "--mode=nvml",
        "--disable-hook=all",
        "--feature-flag=no-additional-gids-for-device-nodes",
        "--nvidia-cdi-hook-path",
    ]);
    generate.arg(&hook_program);
    for hook in HOOKS {
        generate.args(["--enable-hook", hook]);
    }
    let bytes = output(generate.output(), "cannot discover NVIDIA GPUs")?;
    let spec: Spec = serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    let mut setup = Setup {
        devices: Vec::new(),
        mounts: Vec::new(),
        env: Vec::new(),
        hooks: Vec::new(),
        hook_program,
    };
    let edits = spec.select(requests)?;
    for edit in edits {
        setup.apply(edit)?;
    }
    if setup.devices.is_empty() {
        return Err(invalid("the selected GPUs have no device nodes"));
    }
    Ok(Some(setup))
}

impl Spec {
    fn select(&self, requests: &[&Request]) -> Result<Vec<&Edits>> {
        if self.kind != KIND {
            return Err(invalid(
                "the toolkit did not produce an NVIDIA GPU specification",
            ));
        }
        let mut names = BTreeMap::new();
        let mut indices = BTreeMap::new();
        for entry in &self.devices {
            if names.insert(entry.name.as_str(), &entry.edits).is_some() {
                return Err(invalid("the toolkit returned the same GPU name twice"));
            }
            if let Ok(index) = entry.name.parse::<u32>()
                && indices.insert(index, entry.name.as_str()).is_some()
            {
                return Err(invalid("the toolkit returned the same GPU index twice"));
            }
        }
        let mut selected = vec![&self.common];
        let mut seen = HashSet::new();
        for request in requests {
            let ids = if request.device_ids.is_empty() {
                let count = if request.count == -1 {
                    indices.len()
                } else {
                    request.count as usize
                };
                if count == 0 || count > indices.len() {
                    return Err(invalid(
                        "the requested count exceeds the available NVIDIA GPUs",
                    ));
                }
                indices.values().copied().take(count).collect::<Vec<_>>()
            } else {
                request.device_ids.iter().map(String::as_str).collect()
            };
            for id in ids {
                if id == "all" {
                    return Err(invalid("use a count of all instead of the device name all"));
                }
                let edits = names
                    .get(id)
                    .ok_or_else(|| invalid(format!("NVIDIA device {id:?} is unavailable")))?;
                if seen.insert(id) {
                    selected.push(edits);
                }
            }
        }
        Ok(selected)
    }
}

impl Setup {
    fn apply(&mut self, edits: &Edits) -> Result<()> {
        for node in &edits.device_nodes {
            let device = node.device()?;
            if let Some(prior) = self
                .devices
                .iter()
                .find(|d| d.container == device.container)
            {
                if prior != &device {
                    return Err(invalid(format!(
                        "NVIDIA device {} is ambiguous",
                        device.container
                    )));
                }
            } else {
                self.devices.push(device);
            }
        }
        for bind in &edits.mounts {
            let Some(mount) = bind.mount()? else {
                continue;
            };
            if let Some(prior) = self.mounts.iter().find(|m| m.target == mount.target) {
                if prior != &mount {
                    return Err(invalid(format!(
                        "NVIDIA mount {} is ambiguous",
                        mount.target
                    )));
                }
            } else {
                self.mounts.push(mount);
            }
        }
        for hook in &edits.hooks {
            let args = hook.arguments(&self.hook_program)?;
            if !self.hooks.contains(&args) {
                self.hooks.push(args);
            }
        }
        for env in &edits.env {
            match env.split_once('=') {
                Some((key @ "NVIDIA_VISIBLE_DEVICES", "void")) => {
                    self.env.push((key.into(), "void".into()));
                }
                Some((key @ "NVIDIA_CTK_LIBCUDA_DIR", dir)) if driver_target(dir) => {
                    self.env.push((key.into(), dir.into()));
                }
                _ => {
                    return Err(invalid(
                        "the toolkit returned an unsupported NVIDIA environment setting",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Run the stock createContainer hooks in the child's private mount namespace.
    /// Driver files are mounted, but caller bind mounts and the image's command are not in use.
    pub(crate) fn run_hooks(&self, root: &Path) -> Result<()> {
        if self.hooks.is_empty() {
            return Ok(());
        }
        let bundle = tempfile::tempdir_in("/tmp").map_err(hook_io)?;
        let spec = serde_json::json!({ "root": { "path": root } });
        std::fs::write(bundle.path().join("config.json"), spec.to_string()).map_err(hook_io)?;
        let state = serde_json::json!({
            "ociVersion": "1.0.2",
            "status": "creating",
            "bundle": bundle.path(),
            "pid": std::process::id(),
        })
        .to_string();
        for args in &self.hooks {
            let mut child = command(&self.hook_program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(hook_io)?;
            let sent = child
                .stdin
                .take()
                .expect("piped hook input")
                .write_all(state.as_bytes());
            output(
                child.wait_with_output(),
                "cannot configure NVIDIA drivers in the container",
            )?;
            sent.map_err(hook_io)?;
        }
        Ok(())
    }
}

impl Node {
    fn device(&self) -> Result<Device> {
        let host = self
            .host_path
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or(&self.path);
        let host_node = host.strip_prefix("/run/nvidia/driver").unwrap_or(host);
        if !gpu_node(host_node)
            || !gpu_node(&self.path)
            || self.kind.as_deref().is_some_and(|kind| kind != "c")
        {
            return Err(invalid(format!(
                "unsupported NVIDIA device {:?}",
                self.path
            )));
        }
        let info = std::fs::metadata(host).map_err(|error| {
            Error::io(
                format!("cannot inspect NVIDIA device {host}"),
                &error,
                INSTALL,
            )
        })?;
        if !info.file_type().is_char_device()
            || self
                .major
                .is_some_and(|major| major != libc::major(info.rdev()))
            || self
                .minor
                .is_some_and(|minor| minor != libc::minor(info.rdev()))
        {
            return Err(invalid(format!(
                "incorrect device type or numbers for {host}"
            )));
        }
        Device::new(
            host,
            &self.path,
            self.permissions.as_deref().unwrap_or("rwm"),
        )
    }
}

impl Bind {
    fn mount(&self) -> Result<Option<Mount>> {
        if !absolute_path(&self.host_path)
            || !absolute_path(&self.container_path)
            || self.kind.as_deref().is_some_and(|kind| kind != "bind")
        {
            return Err(invalid(format!(
                "unsupported NVIDIA mount {:?}",
                self.container_path
            )));
        }
        let src = Path::new(&self.host_path);
        let info = std::fs::metadata(src).map_err(|error| {
            Error::io(
                format!("cannot inspect NVIDIA driver file {}", self.host_path),
                &error,
                INSTALL,
            )
        })?;
        // Compute and NVML use the device nodes. Do not expose host driver management sockets.
        if info.file_type().is_socket() {
            return Ok(None);
        }
        if !driver_target(&self.container_path)
            || !info.is_file()
            || info.mode() & 0o6000 != 0
            || !self.options.iter().any(|option| option == "ro")
            || self.options.iter().any(|option| {
                !matches!(
                    option.as_str(),
                    "ro" | "nosuid" | "nodev" | "bind" | "rbind" | "private" | "rprivate"
                )
            })
        {
            return Err(invalid(format!(
                "unsupported NVIDIA driver file {:?}",
                self.container_path
            )));
        }
        let source = trusted(src)?;
        Ok(Some(Mount {
            source: MountSource::Bind {
                path: source
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid("driver path is not UTF-8"))?,
                propagation: Default::default(),
                shallow_ro: true,
            },
            target: self.container_path.clone(),
            read_only: true,
            copy_up: false,
        }))
    }
}

impl Hook {
    fn arguments(&self, program: &Path) -> Result<Vec<String>> {
        let bad = || invalid("the toolkit returned an unsupported NVIDIA hook");
        if self.hook_name != "createContainer"
            || Path::new(&self.path) != program
            || self.env.iter().any(|env| env != "NVIDIA_CTK_DEBUG=false")
            || self.args.first().and_then(|arg| Path::new(arg).file_name()) != program.file_name()
        {
            return Err(bad());
        }
        let prefix = if program.ends_with("nvidia-ctk") {
            if self.args.get(1).is_none_or(|arg| arg != "hook") {
                return Err(bad());
            }
            2
        } else {
            1
        };
        let Some((hook, values)) = self.args.get(prefix..).and_then(<[_]>::split_first) else {
            return Err(bad());
        };
        if !HOOKS.contains(&hook.as_str()) {
            return Err(bad());
        }
        let mut args = values.iter();
        while let Some(arg) = args.next() {
            let (flag, value) = match arg.split_once('=') {
                Some((flag, value)) => (flag, value),
                None => (arg.as_str(), args.next().ok_or_else(bad)?.as_str()),
            };
            let valid = match (hook.as_str(), flag) {
                ("create-symlinks", "--link") => {
                    value.split_once("::").is_some_and(|(source, target)| {
                        !source.is_empty() && (driver_target(target) || gpu_node(target))
                    })
                }
                ("update-ldcache" | "create-soname-symlinks", "--folder") => driver_target(value),
                ("update-ldcache" | "create-soname-symlinks", "--ldconfig-path") => matches!(
                    value,
                    "/sbin/ldconfig"
                        | "/usr/sbin/ldconfig"
                        | "/sbin/ldconfig.real"
                        | "/usr/sbin/ldconfig.real"
                ),
                ("enable-cuda-compat", "--host-driver-version" | "--host-cuda-version") => {
                    !value.is_empty()
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || byte == b'.')
                }
                _ => false,
            };
            if !valid {
                return Err(bad());
            }
        }
        Ok(self.args[1..].to_vec())
    }
}

fn absolute_path(path: &str) -> bool {
    !path.contains('\0') && crate::volume::check_target(path).is_ok()
}

fn gpu_node(path: &str) -> bool {
    absolute_path(path) && (path.starts_with("/dev/nvidia") || path.starts_with("/dev/dri/"))
}

fn driver_target(path: &str) -> bool {
    absolute_path(path)
        && [
            "/lib",
            "/lib32",
            "/lib64",
            "/usr/lib",
            "/usr/lib32",
            "/usr/lib64",
            "/usr/local/lib",
            "/usr/bin",
            "/usr/share",
            "/etc",
            "/dev/dri/by-path",
        ]
        .iter()
        .any(|root| Path::new(path).starts_with(root))
}

fn tool(name: &str) -> Result<Option<PathBuf>> {
    for dir in ["/usr/bin", "/usr/local/bin"] {
        let path = Path::new(dir).join(name);
        if !path.exists() {
            continue;
        }
        let resolved = trusted(&path)?;
        if !std::fs::metadata(&resolved)
            .is_ok_and(|info| info.is_file() && info.mode() & 0o100 != 0)
        {
            return Err(invalid(format!(
                "{} is not an executable toolkit file",
                path.display()
            )));
        }
        return Ok(Some(path));
    }
    Ok(None)
}

fn trusted(path: &Path) -> Result<PathBuf> {
    let fail = || invalid(format!("{} is not controlled by root", path.display()));
    let resolved = path.canonicalize().map_err(|_| fail())?;
    // Check the original directories and the resolved file and directories. The stock tool
    // must not be redirected through a writable directory or a user-owned executable.
    for part in path.ancestors().skip(1).chain(resolved.ancestors()) {
        let info = std::fs::metadata(part).map_err(|_| fail())?;
        if info.uid() != 0 || info.mode() & 0o022 != 0 {
            return Err(fail());
        }
    }
    Ok(resolved)
}

fn command(path: &Path) -> Command {
    let mut cmd = Command::new(path);
    cmd.env_clear()
        .env("PATH", PATH)
        .env("LC_ALL", "C")
        .current_dir("/")
        .stdin(Stdio::null());
    cmd
}

fn check_version(program: &Path) -> Result<()> {
    let bytes = output(
        command(program).arg("--version").output(),
        "cannot identify the NVIDIA toolkit",
    )?;
    let version = String::from_utf8_lossy(&bytes);
    if version_number(&version).is_some_and(|version| version >= MIN_VERSION) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::MissingRequirement,
        "cannot use the installed NVIDIA toolkit",
        format!("{} must be version 1.19.1 or later", program.display()),
        INSTALL,
    ))
}

fn version_number(text: &str) -> Option<(u32, u32, u32)> {
    let (_, version) = text.lines().next()?.rsplit_once(" version ")?;
    let (major, rest) = version.split_once('.')?;
    let (minor, patch) = rest.split_once('.')?;
    Some((
        major.parse().ok()?,
        minor.parse().ok()?,
        patch.trim().parse().ok()?,
    ))
}

fn output(result: std::io::Result<Output>, what: &str) -> Result<Vec<u8>> {
    let out = result.map_err(|error| Error::io(what, &error, INSTALL))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.lines().last().unwrap_or("the NVIDIA toolkit failed");
        return Err(Error::new(ErrorKind::Runtime, what, detail, INSTALL));
    }
    Ok(out.stdout)
}

fn hook_io(error: std::io::Error) -> Error {
    Error::io("cannot run the NVIDIA container hook", &error, INSTALL)
}

fn invalid(why: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "cannot configure NVIDIA GPUs",
        why,
        INSTALL,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Spec {
        serde_json::from_value(serde_json::json!({
            "cdiVersion": "0.5.0",
            "kind": "nvidia.com/gpu",
            "containerEdits": {
                "deviceNodes": [{ "path": "/dev/nvidiactl" }]
            },
            "devices": [
                { "name": "10", "containerEdits": { "deviceNodes": [{ "path": "/dev/nvidia10" }] } },
                { "name": "2", "containerEdits": { "deviceNodes": [{ "path": "/dev/nvidia2" }] } },
                { "name": "GPU-abc", "containerEdits": { "deviceNodes": [{ "path": "/dev/nvidia10" }] } },
                { "name": "all", "containerEdits": { "deviceNodes": [{ "path": "/dev/nvidia99" }] } }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn includes_common_nodes_and_selects_only_requested_nvidia_devices() {
        let spec = fixture();
        let select = |selection: &str| {
            spec.select(&[&selection.parse().unwrap()])
                .unwrap()
                .iter()
                .flat_map(|edits| edits.device_nodes.iter().map(|node| node.path.as_str()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            select("driver=nvidia,1"),
            ["/dev/nvidiactl", "/dev/nvidia2"]
        );
        assert_eq!(
            select("driver=nvidia,all"),
            ["/dev/nvidiactl", "/dev/nvidia2", "/dev/nvidia10"]
        );
        assert_eq!(
            select("driver=nvidia,device=GPU-abc,GPU-abc"),
            ["/dev/nvidiactl", "/dev/nvidia10"]
        );
        assert!(spec.select(&[&"driver=nvidia,3".parse().unwrap()]).is_err());
        assert!(
            spec.select(&[&"driver=nvidia,device=all".parse().unwrap()])
                .is_err()
        );
    }

    #[test]
    fn admits_stock_library_hooks_and_rejects_unsafe_overrides() {
        let program = Path::new("/usr/bin/nvidia-cdi-hook");
        let mut hook = Hook {
            hook_name: "createContainer".into(),
            path: program.to_str().unwrap().into(),
            args: [
                "nvidia-cdi-hook",
                "update-ldcache",
                "--folder",
                "/usr/lib/aarch64-linux-gnu",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            env: vec!["NVIDIA_CTK_DEBUG=false".into()],
        };
        assert_eq!(
            hook.arguments(program).unwrap(),
            ["update-ldcache", "--folder", "/usr/lib/aarch64-linux-gnu"]
        );
        hook.env.push("LD_PRELOAD=/tmp/host-library".into());
        assert!(hook.arguments(program).is_err());
        hook.env.pop();
        hook.args
            .extend(["--ldconfig-path".into(), "/tmp/host-tool".into()]);
        assert!(hook.arguments(program).is_err());
        hook.args.truncate(4);
        hook.path = "/tmp/nvidia-cdi-hook".into();
        assert!(hook.arguments(program).is_err());
    }

    #[test]
    fn reads_the_vendor_binary_version() {
        assert_eq!(
            version_number("NVIDIA CDI Hook version 1.20.2\ncommit: example\n"),
            Some((1, 20, 2))
        );
        assert!(
            version_number("NVIDIA Container Toolkit CLI version 1.19.0")
                .is_some_and(|version| version < MIN_VERSION)
        );
        assert!(version_number("NVIDIA CDI Hook version 1.20.2-rc.1").is_none());
    }
}
