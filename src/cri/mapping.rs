//! From the kubelet's words to podbag's: a CRI container config becomes a podbag container
//! spec. Pure functions; the caller reads the files and the image.

use std::collections::BTreeMap;

use super::pb;
use crate::cgroup::Limits;
use crate::image::Image;
use crate::spec::{ContainerSpec, Device, Namespace, Namespaces, Rootfs, Security};
use crate::volume::{Mount, MountSource, Propagation};
use crate::{Error, ErrorKind, Result};

/// Where cgroup v2 is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// How the kubelet names cgroups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupDriver {
    Cgroupfs,
    Systemd,
}

/// The cgroup directory of a container of `id` under the kubelet's `parent`: a cgroupfs
/// path (`/kubepods/burstable/pod<uid>`), or a systemd slice
/// (`kubepods-burstable-pod<uid>.slice`, whose path holds every slice above it). Without a
/// parent (a pod made by hand), podbag's own group.
pub fn cgroup_path(driver: CgroupDriver, parent: &str, id: &str) -> Result<String> {
    let root = CGROUP_ROOT;
    let bad = |why: &str| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid cgroup parent {parent:?}"),
            why,
            "check the kubelet's cgroup driver: it must match `podbag cri serve --cgroup-driver`",
        )
    };
    if parent.is_empty() {
        return Ok(format!("{root}/podbag/{id}"));
    }
    if parent.split('/').any(|p| p == "..") {
        return Err(bad("it leaves the cgroup tree"));
    }
    match driver {
        CgroupDriver::Cgroupfs => {
            if parent.ends_with(".slice") && !parent.contains('/') {
                return Err(bad("this is a systemd slice, and the driver is cgroupfs"));
            }
            Ok(format!("{root}/{}/{id}", parent.trim_matches('/')))
        }
        CgroupDriver::Systemd => {
            let Some(name) = parent.strip_suffix(".slice").filter(|n| !n.contains('/')) else {
                return Err(bad(
                    "the systemd driver takes a slice name, like kubepods.slice",
                ));
            };
            Ok(format!("{root}/{}/podbag-{id}.scope", slice_path(name)))
        }
    }
}

/// `a-b-c` (a slice name without `.slice`) → `a.slice/a-b.slice/a-b-c.slice`, as systemd
/// nests slices; `-` is the root slice.
fn slice_path(name: &str) -> String {
    if name == "-" || name.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = name.split('-').collect();
    (1..=parts.len())
        .map(|n| format!("{}.slice", parts[..n].join("-")))
        .collect::<Vec<_>>()
        .join("/")
}

/// The cgroup files for the kubelet's resources (cgroup v2, as runc converts them).
pub fn resource_files(r: &pb::LinuxContainerResources) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let mut set = |k: &str, v: String| {
        files.insert(k.to_string(), v);
    };
    if r.cpu_shares > 0 {
        set("cpu.weight", cpu_weight(r.cpu_shares as u64).to_string());
    }
    if r.cpu_quota != 0 || r.cpu_period != 0 {
        let period = if r.cpu_period > 0 {
            r.cpu_period
        } else {
            crate::cgroup::CPU_PERIOD as i64
        };
        let quota = if r.cpu_quota > 0 {
            r.cpu_quota.to_string()
        } else {
            "max".to_string()
        };
        set("cpu.max", format!("{quota} {period}"));
    }
    if r.memory_limit_in_bytes > 0 {
        set("memory.max", r.memory_limit_in_bytes.to_string());
        // The kubelet gives memory plus swap; cgroup v2 counts swap alone.
        match r.memory_swap_limit_in_bytes {
            -1 => set("memory.swap.max", "max".into()),
            s if s > 0 => set(
                "memory.swap.max",
                s.saturating_sub(r.memory_limit_in_bytes).max(0).to_string(),
            ),
            _ => {}
        }
    }
    if !r.cpuset_cpus.is_empty() {
        set("cpuset.cpus", r.cpuset_cpus.clone());
    }
    if !r.cpuset_mems.is_empty() {
        set("cpuset.mems", r.cpuset_mems.clone());
    }
    for h in &r.hugepage_limits {
        if !h.page_size.is_empty() && !h.page_size.contains('/') {
            set(&format!("hugetlb.{}.max", h.page_size), h.limit.to_string());
        }
    }
    for (k, v) in &r.unified {
        set(k, v.clone());
    }
    files
}

/// cgroup v1 CPU shares (2–262144) as a cgroup v2 weight (1–10000), runc's formula.
pub fn cpu_weight(shares: u64) -> u64 {
    let shares = shares.clamp(2, 262_144);
    1 + ((shares - 2) * 9999) / 262_142
}

/// The kubelet's resources as podbag limits.
pub fn limits(r: Option<&pb::LinuxContainerResources>) -> Limits {
    Limits {
        files: r.map(resource_files).unwrap_or_default(),
        ..Default::default()
    }
}

/// Where the container's log goes: the pod's log folder and the container's own path.
pub fn log_file(log_directory: &str, log_path: &str) -> Option<String> {
    if log_path.is_empty() {
        return None;
    }
    if log_path.starts_with('/') || log_directory.is_empty() {
        return Some(log_path.to_string());
    }
    Some(format!(
        "{}/{log_path}",
        log_directory.trim_end_matches('/')
    ))
}

/// The pod a container goes into, as the mapping needs it.
#[derive(Debug, Clone, Default)]
pub struct PodView {
    pub hostname: String,
    pub log_directory: String,
    pub cgroup_parent: String,
    /// The pod's network namespace; None: the host's network.
    pub net: Option<String>,
    /// The pod's IPC namespace; None: the host's.
    pub ipc: Option<String>,
    /// The pod's UTS namespace; None: the host's.
    pub uts: Option<String>,
    /// The pod's PID namespace, when its containers share one.
    pub pid: Option<String>,
    /// The pod's own files: `hostname` and `resolv.conf`.
    pub files: String,
    /// The pod's /dev/shm, shared by its containers (none with the host's IPC).
    pub shm: Option<String>,
    pub labels: BTreeMap<String, String>,
}

/// What the caller found out for the container: the PID namespace of a target container.
#[derive(Debug, Clone, Default)]
pub struct Found {
    /// `/proc/<pid>/ns/pid` of the container a `TARGET` PID namespace names.
    pub target_pid_ns: Option<String>,
}

/// The settings of a podbag container for the kubelet's container `config` in `pod`, on
/// `image`. `rootfs` and `digest` come from the store.
#[allow(clippy::too_many_arguments)]
pub fn container_spec(
    id: &str,
    name: &str,
    config: &pb::ContainerConfig,
    pod: &PodView,
    image: &Image,
    rootfs: Rootfs,
    digest: String,
    driver: CgroupDriver,
    found: &Found,
) -> Result<ContainerSpec> {
    let linux = config.linux.clone().unwrap_or_default();
    let sc = linux.security_context.clone().unwrap_or_default();
    let nonempty = |v: &Vec<String>| (!v.is_empty()).then(|| v.clone());
    let command = crate::container::command_line(
        &image.config,
        nonempty(&config.command).as_deref(),
        nonempty(&config.args).as_deref(),
    )?;
    let mut env = image.config.env.clone();
    env.extend(config.envs.iter().map(|kv| {
        (
            kv.key.clone(),
            String::from_utf8_lossy(&kv.value).into_owned(),
        )
    }));
    let workdir = Some(config.working_dir.clone())
        .filter(|w| !w.is_empty())
        .or_else(|| image.config.workdir.clone());

    let security = security(&sc, linux.resources.as_ref())?;
    let user = user(&sc, image.config.user.as_deref())?;

    let ns_opts = sc.namespace_options.clone().unwrap_or_default();
    let pid = match pb::NamespaceMode::try_from(ns_opts.pid).unwrap_or(pb::NamespaceMode::Container)
    {
        pb::NamespaceMode::Node => Namespace::Host,
        pb::NamespaceMode::Pod => match &pod.pid {
            Some(path) => Namespace::Join { path: path.clone() },
            None => Namespace::Own,
        },
        pb::NamespaceMode::Target => match &found.target_pid_ns {
            Some(path) => Namespace::Join { path: path.clone() },
            None => {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    "cannot share the target's PID namespace",
                    format!("container {} does not run", ns_opts.target_id),
                    "start the target container first",
                ));
            }
        },
        pb::NamespaceMode::Container => Namespace::Own,
    };
    let join = |path: &Option<String>| match path {
        Some(p) => Namespace::Join { path: p.clone() },
        None => Namespace::Host,
    };
    let (network, net) = match &pod.net {
        Some(p) => (
            crate::network::NetworkMode::None,
            Namespace::Join { path: p.clone() },
        ),
        None => (crate::network::NetworkMode::Host, Namespace::Own),
    };
    let namespaces = Namespaces {
        net,
        ipc: join(&pod.ipc),
        uts: join(&pod.uts),
        pid,
    };

    let mut mounts: Vec<Mount> = Vec::new();
    let given = |target: &str| config.mounts.iter().any(|m| m.container_path == target);
    let bind = |path: String, target: &str, read_only: bool| Mount {
        source: MountSource::Bind {
            path,
            propagation: Propagation::Private,
            shallow_ro: false,
        },
        target: target.to_string(),
        read_only,
        copy_up: false,
    };
    for (file, target) in [
        ("hostname", "/etc/hostname"),
        ("resolv.conf", "/etc/resolv.conf"),
    ] {
        if !given(target) {
            mounts.push(bind(
                format!("{}/{file}", pod.files),
                target,
                security.read_only_rootfs,
            ));
        }
    }
    if let (Some(shm), false) = (&pod.shm, given("/dev/shm")) {
        mounts.push(bind(shm.clone(), "/dev/shm", false));
    }
    for m in &config.mounts {
        if m.container_path.is_empty() || m.host_path.is_empty() {
            continue;
        }
        if m.image.is_some() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "cannot mount an image",
                "podbag does not mount images as volumes",
                "use another kind of volume",
            ));
        }
        let propagation = match pb::MountPropagation::try_from(m.propagation) {
            Ok(pb::MountPropagation::PropagationHostToContainer) => Propagation::HostToContainer,
            Ok(pb::MountPropagation::PropagationBidirectional) => Propagation::Bidirectional,
            _ => Propagation::Private,
        };
        mounts.push(Mount {
            source: MountSource::Bind {
                path: m.host_path.clone(),
                propagation,
                shallow_ro: m.readonly && !m.recursive_read_only,
            },
            target: m.container_path.clone(),
            read_only: m.readonly,
            copy_up: false,
        });
    }
    let devices = config
        .devices
        .iter()
        .map(|d| Device {
            host: d.host_path.clone(),
            container: if d.container_path.is_empty() {
                d.host_path.clone()
            } else {
                d.container_path.clone()
            },
            access: if d.permissions.is_empty() {
                "rwm".into()
            } else {
                d.permissions.clone()
            },
        })
        .collect();

    let mut labels = pod.labels.clone();
    labels.extend(config.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
    Ok(ContainerSpec {
        id: id.to_string(),
        rootfs,
        command,
        env,
        workdir,
        hostname: if pod.hostname.is_empty() {
            crate::id::short_id(id).to_string()
        } else {
            pod.hostname.clone()
        },
        user,
        image: Some(digest),
        image_name: Some(image.name.clone()),
        name: name.to_string(),
        auto_remove: false,
        tty: config.tty,
        interactive: config.stdin,
        detach: true,
        labels,
        limits: limits(linux.resources.as_ref()),
        cgroup: crate::cgroup::Placement::Exact {
            path: cgroup_path(driver, &pod.cgroup_parent, id)?,
        },
        security,
        mounts,
        network,
        publish: Vec::new(),
        aliases: Vec::new(),
        restart: Default::default(),
        health: None,
        console_size: None,
        extra_networks: Vec::new(),
        namespaces,
        devices,
        cri_log: log_file(&pod.log_directory, &config.log_path),
        stdin_fifo: None,
    })
}

/// The user to run as: the kubelet's user (a name or a uid), else the image's; with the
/// kubelet's group, else the one the user part gives.
pub fn user(
    sc: &pb::LinuxContainerSecurityContext,
    image_user: Option<&str>,
) -> Result<Option<String>> {
    let given = if !sc.run_as_username.is_empty() {
        Some(sc.run_as_username.clone())
    } else {
        sc.run_as_user.as_ref().map(|uid| uid.value.to_string())
    };
    match (given, &sc.run_as_group) {
        (Some(u), Some(g)) => Ok(Some(format!("{u}:{}", g.value))),
        (None, Some(_)) => Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot set the container's group",
            "a group is given without a user",
            "give runAsUser too",
        )),
        (Some(u), None) => Ok(Some(u)),
        (None, None) => Ok(image_user.filter(|u| !u.is_empty()).map(str::to_string)),
    }
}

/// The security settings for the kubelet's security context.
pub fn security(
    sc: &pb::LinuxContainerSecurityContext,
    resources: Option<&pb::LinuxContainerResources>,
) -> Result<Security> {
    let caps = sc.capabilities.clone().unwrap_or_default();
    let seccomp = match &sc.seccomp {
        None => false,
        Some(p) => match pb::security_profile::ProfileType::try_from(p.profile_type) {
            Ok(pb::security_profile::ProfileType::RuntimeDefault) => true,
            Ok(pb::security_profile::ProfileType::Unconfined) => false,
            _ => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "cannot apply the seccomp profile",
                    format!(
                        "podbag has only its default profile and none; {:?} profiles are not supported",
                        p.localhost_ref
                    ),
                    "use the RuntimeDefault or Unconfined seccomp profile",
                ));
            }
        },
    };
    let list = |v: &Vec<String>| v.clone();
    Ok(Security {
        cap_add: list(&caps.add_capabilities),
        cap_drop: list(&caps.drop_capabilities),
        no_new_privileges: sc.no_new_privs,
        seccomp,
        privileged: sc.privileged,
        groups: sc
            .supplemental_groups
            .iter()
            .filter_map(|&g| u32::try_from(g).ok())
            .collect(),
        own_group_too: true,
        oom_score_adj: resources
            .map(|r| r.oom_score_adj)
            .filter(|&v| v != 0)
            .map(|v| v.clamp(-1000, 1000) as i32),
        read_only_rootfs: sc.readonly_rootfs,
        // The kubelet sends the default lists itself, and none for an unmasked /proc.
        masked_paths: Some(sc.masked_paths.clone()).filter(|_| !sc.privileged),
        readonly_paths: Some(sc.readonly_paths.clone()).filter(|_| !sc.privileged),
    })
}

/// A podbag container name for a kubelet container: unique per pod, name and attempt, as
/// the CRI wants. The parts are DNS labels, which podbag names allow.
pub fn container_name(pod: &pb::PodSandboxMetadata, c: &pb::ContainerMetadata) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-') {
                    ch
                } else {
                    '-'
                }
            })
            .collect()
    };
    format!(
        "k8s_{}_{}_{}_{}_{}",
        clean(&c.name),
        clean(&pod.name),
        clean(&pod.namespace),
        clean(&pod.uid),
        c.attempt
    )
}

/// The kernel settings of a pod, sorted: the kubelet's, and in a network of the pod's own
/// what containerd sets by default: any user may bind low ports and ping (non-root DNS
/// servers such as CoreDNS bind port 53).
pub fn pod_sysctls(
    given: &std::collections::HashMap<String, String>,
    own_net: bool,
) -> Vec<(String, String)> {
    let mut out: BTreeMap<String, String> =
        given.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    if own_net {
        for (k, v) in [
            ("net.ipv4.ip_unprivileged_port_start", "0"),
            ("net.ipv4.ping_group_range", "0 2147483647"),
        ] {
            out.entry(k.to_string()).or_insert_with(|| v.to_string());
        }
    }
    out.into_iter().collect()
}

/// The text of a pod's /etc/resolv.conf: the kubelet's DNS settings, or the host's file.
pub fn resolv_conf(dns: Option<&pb::DnsConfig>, host: &str) -> String {
    let Some(dns) =
        dns.filter(|d| !d.servers.is_empty() || !d.searches.is_empty() || !d.options.is_empty())
    else {
        return host.to_string();
    };
    let mut out = String::new();
    if !dns.searches.is_empty() {
        out.push_str(&format!("search {}\n", dns.searches.join(" ")));
    }
    for s in &dns.servers {
        out.push_str(&format!("nameserver {s}\n"));
    }
    if !dns.options.is_empty() {
        out.push_str(&format!("options {}\n", dns.options.join(" ")));
    }
    out
}

/// Whether `labels` has every pair of `selector`.
pub fn matches_labels<'a>(
    labels: impl IntoIterator<Item = (&'a String, &'a String)> + Clone,
    selector: &std::collections::HashMap<String, String>,
) -> bool {
    selector.iter().all(|(k, v)| {
        labels
            .clone()
            .into_iter()
            .any(|(lk, lv)| lk == k && lv == v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ImageConfig;

    #[test]
    fn cgroup_paths_for_both_drivers() {
        let id = "c".repeat(64);
        assert_eq!(
            cgroup_path(CgroupDriver::Cgroupfs, "/kubepods/burstable/pod12", &id).unwrap(),
            format!("/sys/fs/cgroup/kubepods/burstable/pod12/{id}")
        );
        assert_eq!(
            cgroup_path(CgroupDriver::Cgroupfs, "kubepods/besteffort/podab/", &id).unwrap(),
            format!("/sys/fs/cgroup/kubepods/besteffort/podab/{id}")
        );
        assert_eq!(
            cgroup_path(CgroupDriver::Systemd, "kubepods-burstable-pod12.slice", &id).unwrap(),
            format!(
                "/sys/fs/cgroup/kubepods.slice/kubepods-burstable.slice/\
                 kubepods-burstable-pod12.slice/podbag-{id}.scope"
            )
        );
        assert_eq!(
            cgroup_path(CgroupDriver::Systemd, "kubepods.slice", &id).unwrap(),
            format!("/sys/fs/cgroup/kubepods.slice/podbag-{id}.scope")
        );
        assert_eq!(
            cgroup_path(CgroupDriver::Cgroupfs, "", &id).unwrap(),
            format!("/sys/fs/cgroup/podbag/{id}")
        );
        for (driver, bad) in [
            (CgroupDriver::Cgroupfs, "kubepods.slice"),
            (CgroupDriver::Cgroupfs, "/kubepods/../.."),
            (CgroupDriver::Systemd, "/kubepods/burstable"),
        ] {
            assert_eq!(
                cgroup_path(driver, bad, &id).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad}"
            );
        }
    }

    #[test]
    fn resources_become_cgroup_v2_files() {
        let r = pb::LinuxContainerResources {
            cpu_period: 100_000,
            cpu_quota: 50_000,
            cpu_shares: 512,
            memory_limit_in_bytes: 128 << 20,
            memory_swap_limit_in_bytes: 128 << 20,
            cpuset_cpus: "0-1".into(),
            hugepage_limits: vec![pb::HugepageLimit {
                page_size: "2MB".into(),
                limit: 0,
            }],
            unified: [("memory.high".to_string(), "100000000".to_string())].into(),
            ..Default::default()
        };
        let f = resource_files(&r);
        assert_eq!(f["cpu.weight"], "20");
        assert_eq!(f["cpu.max"], "50000 100000");
        assert_eq!(f["memory.max"], "134217728");
        assert_eq!(f["memory.swap.max"], "0");
        assert_eq!(f["cpuset.cpus"], "0-1");
        assert!(!f.contains_key("cpuset.mems"));
        assert_eq!(f["hugetlb.2MB.max"], "0");
        assert_eq!(f["memory.high"], "100000000");
        let l = limits(Some(&r));
        assert_eq!(
            l.controllers(),
            ["cpu", "cpuset", "hugetlb", "memory"].map(String::from)
        );
        // Shares alone: no cpu.max. No quota with a period: no limit.
        let only = resource_files(&pb::LinuxContainerResources {
            cpu_shares: 2,
            cpu_period: 100_000,
            ..Default::default()
        });
        assert_eq!(only["cpu.weight"], "1");
        assert_eq!(only["cpu.max"], "max 100000");
        assert!(resource_files(&Default::default()).is_empty());
        assert_eq!(cpu_weight(1024), 39);
        assert_eq!(cpu_weight(262_144), 10_000);
    }

    #[test]
    fn log_files_join_the_pod_folder() {
        assert_eq!(
            log_file("/var/log/pods/ns_web_1", "nginx/0.log").as_deref(),
            Some("/var/log/pods/ns_web_1/nginx/0.log")
        );
        assert_eq!(log_file("/x/", "/abs.log").as_deref(), Some("/abs.log"));
        assert_eq!(log_file("", "rel.log").as_deref(), Some("rel.log"));
        assert_eq!(log_file("/x", ""), None);
    }

    #[test]
    fn users_come_from_the_kubelet_or_the_image() {
        let sc =
            |user: Option<i64>, name: &str, group: Option<i64>| pb::LinuxContainerSecurityContext {
                run_as_user: user.map(|value| pb::Int64Value { value }),
                run_as_username: name.into(),
                run_as_group: group.map(|value| pb::Int64Value { value }),
                ..Default::default()
            };
        let u = |s, image| user(&s, image).unwrap();
        assert_eq!(u(sc(None, "", None), Some("nginx")), Some("nginx".into()));
        assert_eq!(u(sc(None, "", None), None), None);
        assert_eq!(
            u(sc(Some(1000), "", None), Some("nginx")),
            Some("1000".into())
        );
        assert_eq!(u(sc(None, "app", Some(5)), None), Some("app:5".into()));
        assert_eq!(u(sc(Some(7), "", Some(8)), None), Some("7:8".into()));
        // A group needs the kubelet's user, as with containerd.
        for image in [Some("web:web"), None] {
            assert_eq!(
                user(&sc(None, "", Some(8)), image).unwrap_err().kind,
                ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn security_contexts_map_and_localhost_seccomp_is_refused() {
        let profile = |t: pb::security_profile::ProfileType| pb::SecurityProfile {
            profile_type: t as i32,
            ..Default::default()
        };
        let sc = pb::LinuxContainerSecurityContext {
            capabilities: Some(pb::Capability {
                add_capabilities: vec!["NET_ADMIN".into()],
                drop_capabilities: vec!["ALL".into()],
                ..Default::default()
            }),
            supplemental_groups: vec![10, 20],
            no_new_privs: true,
            readonly_rootfs: true,
            masked_paths: vec!["/proc/kcore".into()],
            seccomp: Some(profile(pb::security_profile::ProfileType::RuntimeDefault)),
            ..Default::default()
        };
        let r = pb::LinuxContainerResources {
            oom_score_adj: 1000,
            ..Default::default()
        };
        let s = security(&sc, Some(&r)).unwrap();
        assert_eq!(s.cap_add, ["NET_ADMIN"]);
        assert_eq!(s.cap_drop, ["ALL"]);
        assert_eq!(s.groups, [10, 20]);
        assert!(s.no_new_privileges && s.seccomp && s.read_only_rootfs);
        assert_eq!(s.oom_score_adj, Some(1000));
        assert_eq!(s.masked_paths, Some(vec!["/proc/kcore".to_string()]));
        assert_eq!(s.readonly_paths, Some(vec![]));
        let none = security(&Default::default(), None).unwrap();
        assert!(
            !none.seccomp,
            "no profile is unconfined, as with containerd"
        );
        let local = pb::LinuxContainerSecurityContext {
            seccomp: Some(profile(pb::security_profile::ProfileType::Localhost)),
            ..Default::default()
        };
        assert_eq!(
            security(&local, None).unwrap_err().kind,
            ErrorKind::Unsupported
        );
        let privileged = pb::LinuxContainerSecurityContext {
            privileged: true,
            masked_paths: vec!["/proc/kcore".into()],
            ..Default::default()
        };
        assert_eq!(security(&privileged, None).unwrap().masked_paths, None);
    }

    #[test]
    fn pods_with_their_own_network_may_use_low_ports_and_ping() {
        let none = std::collections::HashMap::new();
        assert_eq!(
            pod_sysctls(&none, true),
            [
                (
                    "net.ipv4.ip_unprivileged_port_start".to_string(),
                    "0".to_string()
                ),
                ("net.ipv4.ping_group_range".into(), "0 2147483647".into()),
            ]
        );
        assert!(pod_sysctls(&none, false).is_empty());
        let given: std::collections::HashMap<String, String> = [
            (
                "net.ipv4.ip_unprivileged_port_start".to_string(),
                "1024".to_string(),
            ),
            ("kernel.shm_rmid_forced".into(), "1".into()),
        ]
        .into();
        assert_eq!(
            pod_sysctls(&given, true),
            [
                ("kernel.shm_rmid_forced".to_string(), "1".to_string()),
                ("net.ipv4.ip_unprivileged_port_start".into(), "1024".into()),
                ("net.ipv4.ping_group_range".into(), "0 2147483647".into()),
            ]
        );
    }

    #[test]
    fn resolv_conf_from_dns_settings_or_the_host() {
        let dns = pb::DnsConfig {
            servers: vec!["10.43.0.10".into()],
            searches: vec![
                "default.svc.cluster.local".into(),
                "svc.cluster.local".into(),
            ],
            options: vec!["ndots:5".into()],
        };
        assert_eq!(
            resolv_conf(Some(&dns), "nameserver 1.1.1.1\n"),
            "search default.svc.cluster.local svc.cluster.local\nnameserver 10.43.0.10\noptions ndots:5\n"
        );
        assert_eq!(resolv_conf(None, "host\n"), "host\n");
        assert_eq!(resolv_conf(Some(&Default::default()), "host\n"), "host\n");
    }

    #[test]
    fn names_are_unique_per_pod_name_and_attempt() {
        let pod = pb::PodSandboxMetadata {
            name: "web-7d4".into(),
            uid: "a1b2".into(),
            namespace: "default".into(),
            attempt: 0,
        };
        let c = pb::ContainerMetadata {
            name: "nginx".into(),
            attempt: 2,
        };
        let name = container_name(&pod, &c);
        assert_eq!(name, "k8s_nginx_web-7d4_default_a1b2_2");
        crate::container::check_name(&name).unwrap();
    }

    fn pod() -> PodView {
        PodView {
            hostname: "web".into(),
            log_directory: "/var/log/pods/x".into(),
            cgroup_parent: "/kubepods/pod1".into(),
            net: Some("/run/podbag/cri/ns/p/net".into()),
            ipc: Some("/run/podbag/cri/ns/p/ipc".into()),
            uts: Some("/run/podbag/cri/ns/p/uts".into()),
            pid: None,
            files: "/var/lib/podbag/cri/sandboxes/p".into(),
            shm: Some("/run/podbag/cri/ns/p/shm".into()),
            labels: BTreeMap::new(),
        }
    }

    fn image() -> Image {
        Image {
            name: "docker.io/library/nginx:1".into(),
            digest: "sha256:ab".into(),
            size: 1,
            config: ImageConfig {
                entrypoint: vec!["/docker-entrypoint.sh".into()],
                cmd: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
                env: vec![("PATH".into(), "/bin".into()), ("A".into(), "1".into())],
                workdir: Some("/srv".into()),
                user: Some("101".into()),
                ..Default::default()
            },
        }
    }

    fn rootfs() -> Rootfs {
        Rootfs::Dir { path: "/r".into() }
    }

    #[test]
    fn a_container_joins_its_pod_and_gets_its_files() {
        let id = "d".repeat(64);
        let config = pb::ContainerConfig {
            metadata: Some(pb::ContainerMetadata {
                name: "nginx".into(),
                attempt: 0,
            }),
            args: vec!["nginx-debug".into()],
            envs: vec![pb::KeyValue {
                key: "A".into(),
                value: b"2".to_vec(),
            }],
            mounts: vec![
                pb::Mount {
                    container_path: "/etc/hosts".into(),
                    host_path: "/var/lib/kubelet/pods/x/etc-hosts".into(),
                    ..Default::default()
                },
                pb::Mount {
                    container_path: "/data".into(),
                    host_path: "/srv/data".into(),
                    readonly: true,
                    propagation: pb::MountPropagation::PropagationHostToContainer as i32,
                    ..Default::default()
                },
            ],
            devices: vec![pb::Device {
                host_path: "/dev/fuse".into(),
                ..Default::default()
            }],
            log_path: "nginx/0.log".into(),
            labels: [(
                "io.kubernetes.container.name".to_string(),
                "nginx".to_string(),
            )]
            .into(),
            ..Default::default()
        };
        let spec = container_spec(
            &id,
            "k8s_nginx",
            &config,
            &pod(),
            &image(),
            rootfs(),
            "sha256:ab".into(),
            CgroupDriver::Cgroupfs,
            &Found::default(),
        )
        .unwrap();
        spec.validate().unwrap();
        assert_eq!(spec.command, ["/docker-entrypoint.sh", "nginx-debug"]);
        assert!(spec.env.contains(&("A".into(), "2".into())));
        assert_eq!(spec.workdir.as_deref(), Some("/srv"));
        assert_eq!(spec.user.as_deref(), Some("101"));
        assert_eq!(spec.hostname, "web");
        assert!(spec.detach && !spec.auto_remove);
        assert_eq!(spec.network, crate::network::NetworkMode::None);
        assert_eq!(
            spec.namespaces.net,
            Namespace::Join {
                path: "/run/podbag/cri/ns/p/net".into()
            }
        );
        assert_eq!(spec.namespaces.pid, Namespace::Own);
        let targets: Vec<&str> = spec.mounts.iter().map(|m| m.target.as_str()).collect();
        assert_eq!(
            targets,
            [
                "/etc/hostname",
                "/etc/resolv.conf",
                "/dev/shm",
                "/etc/hosts",
                "/data"
            ]
        );
        assert!(matches!(
            &spec.mounts[4].source,
            MountSource::Bind {
                propagation: Propagation::HostToContainer,
                ..
            }
        ));
        assert!(spec.mounts[4].read_only);
        assert_eq!(spec.devices[0].container, "/dev/fuse");
        assert_eq!(spec.devices[0].access, "rwm");
        assert_eq!(spec.cri_log.as_deref(), Some("/var/log/pods/x/nginx/0.log"));
        assert_eq!(
            spec.cgroup,
            crate::cgroup::Placement::Exact {
                path: format!("/sys/fs/cgroup/kubepods/pod1/{id}")
            }
        );
        assert_eq!(spec.labels["io.kubernetes.container.name"], "nginx");
    }

    #[test]
    fn host_namespaces_and_shared_pids() {
        let id = "e".repeat(64);
        let mut view = pod();
        view.net = None;
        view.ipc = None;
        view.uts = None;
        view.shm = None;
        view.pid = Some("/run/podbag/cri/ns/p/pid".into());
        let config = |mode: pb::NamespaceMode| pb::ContainerConfig {
            command: vec!["sleep".into()],
            linux: Some(pb::LinuxContainerConfig {
                security_context: Some(pb::LinuxContainerSecurityContext {
                    namespace_options: Some(pb::NamespaceOption {
                        pid: mode as i32,
                        target_id: "t".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec_for = |mode, found: &Found| {
            container_spec(
                &id,
                "n",
                &config(mode),
                &view,
                &image(),
                rootfs(),
                "sha256:ab".into(),
                CgroupDriver::Cgroupfs,
                found,
            )
        };
        let spec = spec_for(pb::NamespaceMode::Pod, &Found::default()).unwrap();
        spec.validate().unwrap();
        assert_eq!(spec.network, crate::network::NetworkMode::Host);
        assert_eq!(spec.namespaces.ipc, Namespace::Host);
        assert_eq!(spec.namespaces.uts, Namespace::Host);
        assert_eq!(
            spec.namespaces.pid,
            Namespace::Join {
                path: "/run/podbag/cri/ns/p/pid".into()
            }
        );
        assert_eq!(spec.command, ["sleep"], "a command drops the image's Cmd");
        assert!(!spec.mounts.iter().any(|m| m.target == "/dev/shm"));
        let host = spec_for(pb::NamespaceMode::Node, &Found::default()).unwrap();
        assert_eq!(host.namespaces.pid, Namespace::Host);
        assert_eq!(
            spec_for(pb::NamespaceMode::Target, &Found::default())
                .unwrap_err()
                .kind,
            ErrorKind::NotFound
        );
        let target = spec_for(
            pb::NamespaceMode::Target,
            &Found {
                target_pid_ns: Some("/proc/9/ns/pid".into()),
            },
        )
        .unwrap();
        assert_eq!(
            target.namespaces.pid,
            Namespace::Join {
                path: "/proc/9/ns/pid".into()
            }
        );
    }

    #[test]
    fn label_selectors_need_every_pair() {
        let labels: BTreeMap<String, String> =
            [("a".to_string(), "1".to_string()), ("b".into(), "2".into())].into();
        let sel = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<std::collections::HashMap<_, _>>()
        };
        assert!(matches_labels(&labels, &sel(&[])));
        assert!(matches_labels(&labels, &sel(&[("a", "1"), ("b", "2")])));
        assert!(!matches_labels(&labels, &sel(&[("a", "2")])));
        assert!(!matches_labels(&labels, &sel(&[("c", "1")])));
    }
}
