//! `/version`, `/info` and `/system/df`.

use super::docker::{docker_arch, docker_image_name, state_label};
use super::router::{Ctx, Reply, reply_error};
use crate::state::Status;
use serde_json::json;

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn machine() -> String {
    std::env::consts::ARCH.to_string()
}

fn os_name() -> String {
    read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".to_string())
}

fn mem_total() -> u64 {
    read("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn rootless() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() != 0 }
}

pub fn version() -> Reply {
    let v = env!("CARGO_PKG_VERSION");
    let kernel = read("/proc/sys/kernel/osrelease");
    Reply::Json(
        200,
        json!({
            "Platform": {"Name": "podbag"},
            "Components": [{
                "Name": "Engine",
                "Version": v,
                "Details": {"ApiVersion": super::router::API_VERSION, "MinAPIVersion": super::router::MIN_API_VERSION, "Os": "linux", "Arch": docker_arch(&machine()), "KernelVersion": kernel},
            }],
            "Version": v,
            "ApiVersion": super::router::API_VERSION,
            "MinAPIVersion": super::router::MIN_API_VERSION,
            "GitCommit": "",
            "GoVersion": "",
            "Os": "linux",
            "Arch": docker_arch(&machine()),
            "KernelVersion": kernel,
            "BuildTime": "",
        }),
    )
}

pub fn info(ctx: &Ctx) -> Reply {
    let containers = match crate::ContainerHandle::list(true) {
        Ok(c) => c,
        Err(e) => return reply_error(&e),
    };
    let count = |s: Status| containers.iter().filter(|c| c.state.status == s).count();
    let paused = containers.iter().filter(|c| c.state.paused).count();
    let images = crate::Image::list().map(|i| i.len()).unwrap_or(0);
    let mut security = vec![
        "name=seccomp,profile=builtin".to_string(),
        "name=cgroupns".to_string(),
    ];
    if rootless() {
        security.push("name=rootless".to_string());
    }
    let cgroup2 = std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    Reply::Json(
        200,
        json!({
            "ID": "podbag",
            "Containers": containers.len(),
            "ContainersRunning": count(Status::Running) - paused,
            "ContainersPaused": paused,
            "ContainersStopped": count(Status::Exited) + count(Status::Created),
            "Images": images,
            "Driver": "overlayfs",
            "DriverStatus": [],
            "DockerRootDir": ctx.dirs.data,
            "KernelVersion": read("/proc/sys/kernel/osrelease"),
            "OperatingSystem": os_name(),
            "OSType": "linux",
            "Architecture": machine(),
            "NCPU": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            "MemTotal": mem_total(),
            "Name": read("/proc/sys/kernel/hostname"),
            "ServerVersion": env!("CARGO_PKG_VERSION"),
            "CgroupDriver": if rootless() { "systemd" } else { "cgroupfs" },
            "CgroupVersion": if cgroup2 { "2" } else { "1" },
            "LoggingDriver": "json-file",
            "SecurityOptions": security,
            "Runtimes": {"podbag": {"path": "podbag"}},
            "DefaultRuntime": "podbag",
            "Swarm": crate::swarm::http::info(ctx),
            // Docker Hub; local registries are reached over plain HTTP.
            "IndexServerAddress": crate::image::auth::DOCKER_HUB_KEY,
            "RegistryConfig": {
                "InsecureRegistryCIDRs": ["127.0.0.0/8", "::1/128"],
                "IndexConfigs": {"docker.io": {"Name": "docker.io", "Mirrors": [], "Secure": true, "Official": true}},
                "Mirrors": [],
            },
            "Plugins": {"Volume": ["local"], "Network": ["bridge", "host", "null", "overlay"], "Log": ["json-file", "syslog", "journald"]},
            "Warnings": [],
        }),
    )
}

/// Bytes in the files below `dir` (as `du` counts them), without following links or
/// leaving its file system. Folders it may not read count as empty.
fn dir_size(dir: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(top) = std::fs::symlink_metadata(dir) else {
        return 0;
    };
    let mut bytes = 0;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.dev() != top.dev() {
                continue;
            }
            bytes += meta.blocks() * 512;
            if meta.is_dir() {
                todo.push(entry.path());
            }
        }
    }
    bytes
}

pub fn df() -> Reply {
    let usage = match crate::disk_usage() {
        Ok(u) => u,
        Err(e) => return reply_error(&e),
    };
    let containers = crate::ContainerHandle::list(true).unwrap_or_default();
    let images: Vec<_> = crate::Image::list()
        .unwrap_or_default()
        .iter()
        .map(|i| {
            let users = containers.iter().filter(|c| c.image == i.name).count();
            json!({
                "Id": format!("sha256:{}", i.digest.trim_start_matches("sha256:")),
                "RepoTags": [docker_image_name(&i.name)],
                "Size": i.size,
                "SharedSize": -1,
                "Containers": users,
                "Created": 0,
            })
        })
        .collect();
    // As `docker ps` shows them (`system df -v` prints image, command, created, status).
    let ctx = crate::paths::dirs().ok().map(Ctx::new);
    let entries: Vec<_> = containers
        .iter()
        .map(|c| {
            let spec = ctx
                .as_ref()
                .and_then(|ctx| Some((ctx, crate::state::read_config(&ctx.dirs, &c.id).ok()?)));
            let mut entry = match spec {
                Some((ctx, spec)) => {
                    super::containers::shape(ctx, c, &spec, super::shapes::list_entry)
                }
                None => json!({"Id": c.id, "Names": [format!("/{}", c.name)],
                               "State": state_label(&c.state)}),
            };
            entry["SizeRw"] = json!(0);
            entry["SizeRootFs"] = json!(0);
            entry
        })
        .collect();
    let volumes: Vec<_> = crate::Volume::list()
        .unwrap_or_default()
        .iter()
        .map(|v| {
            let users = containers
                .iter()
                .filter(|c| {
                    c.mounts
                        .iter()
                        .any(|m| m.kind == "volume" && m.source == v.name)
                })
                .count();
            let size = if v.plugin.is_some() {
                0
            } else {
                dir_size(std::path::Path::new(&v.mountpoint))
            };
            json!({"Name": v.name, "Driver": v.driver(), "Mountpoint": v.mountpoint,
                   "Scope": "local", "UsageData": {"Size": size, "RefCount": users}})
        })
        .collect();
    Reply::Json(
        200,
        json!({
            "LayersSize": usage.object_bytes,
            "Images": images,
            "Containers": entries,
            "Volumes": volumes,
            "BuildCache": [],
        }),
    )
}
