//! `/version`, `/info` and `/system/df`.

use super::docker::{docker_arch, docker_image_name, state_name};
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
            "Platform": {"Name": "openpod"},
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
            "ID": "openpod",
            "Containers": containers.len(),
            "ContainersRunning": count(Status::Running),
            "ContainersPaused": 0,
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
            "Runtimes": {"openpod": {"path": "openpod"}},
            "DefaultRuntime": "openpod",
            "Swarm": {"LocalNodeState": "inactive", "NodeID": "", "NodeAddr": "", "ControlAvailable": false},
            "Plugins": {"Volume": ["local"], "Network": ["bridge", "host", "null"], "Log": ["json-file"]},
            "Warnings": [],
        }),
    )
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
    let containers: Vec<_> = containers
        .iter()
        .map(|c| json!({"Id": c.id, "Names": [format!("/{}", c.name)], "State": state_name(c.state.status), "SizeRw": 0, "SizeRootFs": 0}))
        .collect();
    let volumes: Vec<_> = crate::Volume::list()
        .unwrap_or_default()
        .iter()
        .map(|v| json!({"Name": v.name, "Driver": "local", "Mountpoint": v.mountpoint, "UsageData": {"Size": -1, "RefCount": -1}}))
        .collect();
    Reply::Json(
        200,
        json!({
            "LayersSize": usage.object_bytes,
            "Images": images,
            "Containers": containers,
            "Volumes": volumes,
            "BuildCache": [],
        }),
    )
}
