//! `POST /containers/create`: Docker's container config as a podbag [`Container`].

use crate::image::Image;
use crate::network::{NetworkMode, PortMapping, Proto};
use crate::spec::SecurityOpt;
use crate::{Access, Container, Error, ErrorKind, MountRequest, Result};
use serde_json::Value;

/// The container a create body describes, and warnings about settings podbag ignores.
/// The image must be pulled: NotFound otherwise, so that the docker CLI pulls it and
/// tries again.
pub fn container_from_create(body: &Value, name: Option<&str>) -> Result<(Container, Vec<String>)> {
    let image_name = body["Image"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "cannot create the container",
                "the config names no image",
                "give an image",
            )
        })?;
    let image = Image::get(image_name).map_err(|e| match e.kind {
        ErrorKind::NotFound => Error::new(
            ErrorKind::NotFound,
            format!("No such image: {image_name}"),
            "it is not pulled",
            "pull it first",
        ),
        _ => e,
    })?;
    map_create(body, name, &image)
}

/// [`container_from_create`] for an image already found.
pub fn map_create(
    body: &Value,
    name: Option<&str>,
    image: &Image,
) -> Result<(Container, Vec<String>)> {
    map_create_with(body, name, image, &|mode| {
        // SAFETY: geteuid has no preconditions.
        let rootless = unsafe { libc::geteuid() } != 0;
        crate::netfiles::host_gateway(mode, rootless, crate::netfiles::Place::detect())
    })
}

/// Where `host-gateway` leads for a network mode, or why it cannot be found here.
type GatewayRule<'a> = &'a dyn Fn(&NetworkMode) -> Result<crate::netfiles::HostGateway>;

/// [`map_create`] with the `host-gateway` rule given.
fn map_create_with(
    body: &Value,
    name: Option<&str>,
    image: &Image,
    gateway: GatewayRule<'_>,
) -> Result<(Container, Vec<String>)> {
    let host = &body["HostConfig"];
    refuse_unsupported(host)?;
    let mut warnings = Vec::new();
    let mut c = Container::from_image(image);
    if let Some(n) = name {
        c = c.name(n.trim_start_matches('/'));
    }
    if let Some(cmd) = str_list(&body["Cmd"]) {
        c = c.command(cmd);
    }
    if let Some(entry) = str_list(&body["Entrypoint"]) {
        // `--entrypoint ""` arrives as [""]: no entrypoint.
        let entry: Vec<String> = entry.into_iter().filter(|e| !e.is_empty()).collect();
        c = c.entrypoint(entry);
    }
    for kv in strings(&body["Env"]) {
        // A bare name is one the client did not have: unset.
        if let Some((k, v)) = kv.split_once('=') {
            c = c.env(k, v);
        }
    }
    if let Some(dir) = non_empty(&body["WorkingDir"]) {
        c = c.workdir(dir);
    }
    if let Some(user) = non_empty(&body["User"]) {
        c = c.user(user);
    }
    if let Some(hostname) = non_empty(&body["Hostname"]) {
        c = c.hostname(hostname);
    }
    if !body["StopTimeout"].is_null() {
        let seconds = body["StopTimeout"].as_i64().ok_or_else(|| {
            invalid(
                "invalid StopTimeout",
                "expected an integer",
                "give the grace period in seconds",
            )
        })?;
        c = c.stop_timeout(seconds);
    }
    c = c.tty(body["Tty"].as_bool().unwrap_or(false));
    c = c.interactive(body["OpenStdin"].as_bool().unwrap_or(false));
    if let Some(labels) = body["Labels"].as_object() {
        for (k, v) in labels {
            c = c.label(k, v.as_str().unwrap_or_default());
        }
    }
    let mut targets: Vec<String> = Vec::new();
    let driver = host["VolumeDriver"].as_str().unwrap_or_default();
    let external_driver = !matches!(driver, "" | "local");
    if external_driver {
        crate::volume::check_driver_name(driver)?;
    }
    let anonymous_driver = || {
        invalid(
            "cannot use an external driver with an anonymous volume",
            "the driver requires a volume name",
            "mount named volumes at all targets or use the local driver",
        )
    };
    let mount = |c: Container, targets: &mut Vec<String>, m: MountRequest| {
        targets.push(m.target().to_string());
        c.mount(m)
    };
    for bind in strings(&host["Binds"]) {
        let (mut m, warning) = parse_bind(&bind)?;
        if external_driver
            && let MountRequest::Named {
                name,
                target,
                access,
                ..
            } = m
        {
            m = MountRequest::NamedDriver {
                name,
                target,
                access,
                plugin: crate::volume::DriverVolume {
                    name: driver.to_string(),
                    options: Default::default(),
                },
            };
        }
        warnings.extend(warning);
        c = mount(c, &mut targets, m);
    }
    for m in host["Mounts"].as_array().into_iter().flatten() {
        let (m, warning) = parse_mount(m)?;
        warnings.extend(warning);
        c = mount(c, &mut targets, m);
    }
    if let Some(tmpfs) = host["Tmpfs"].as_object() {
        for (target, options) in tmpfs {
            c = mount(
                c,
                &mut targets,
                MountRequest::Tmpfs {
                    target: target.clone(),
                    options: options.as_str().unwrap_or_default().to_string(),
                },
            );
        }
    }
    if let Some(volumes) = body["Volumes"].as_object() {
        for target in volumes.keys() {
            // `-v host:/path` also lists /path here in some clients: the bind wins.
            if !targets.contains(target) {
                if external_driver {
                    return Err(anonymous_driver());
                }
                c = mount(
                    c,
                    &mut targets,
                    MountRequest::Anonymous {
                        target: target.clone(),
                        access: Access::ReadWrite,
                    },
                );
            }
        }
    }
    if external_driver
        && image
            .config
            .volumes
            .iter()
            .any(|target| !targets.contains(target))
    {
        return Err(anonymous_driver());
    }
    let network = network_mode(host["NetworkMode"].as_str().unwrap_or(""))?;
    let ports = port_bindings(&host["PortBindings"])?;
    if !ports.is_empty() && network == NetworkMode::Host {
        warnings.push(
            "published ports are discarded with the host network (the container uses the host's ports)"
                .to_string(),
        );
    } else {
        for p in ports {
            c = c.publish(p);
        }
    }
    let (aliases, extra) = endpoints(
        &body["NetworkingConfig"]["EndpointsConfig"],
        &network,
        &mut warnings,
    )?;
    crate::spec::check_networks(&network, &aliases, &extra)?;
    c = names(c, host, &|| gateway(&network), &mut warnings)?;
    c = c.network(network);
    for alias in aliases {
        c = c.network_alias(alias);
    }
    for a in extra {
        c = c.extra_network(a.network, a.aliases);
    }
    if let Some(memory) = host["Memory"].as_u64().filter(|m| *m > 0) {
        c = c.memory(memory);
    }
    let nano = host["NanoCpus"].as_u64().unwrap_or(0);
    let quota = host["CpuQuota"].as_i64().unwrap_or(0);
    let period = host["CpuPeriod"].as_i64().unwrap_or(0);
    if nano > 0 {
        c = c.cpus(nano as f64 / 1e9);
    } else if quota > 0 {
        let period = if period > 0 { period } else { 100_000 };
        c = c.cpus(quota as f64 / period as f64);
    }
    if let Some(pids) = host["PidsLimit"].as_i64().filter(|p| *p > 0) {
        c = c.pids_limit(pids);
    }
    for cap in strings(&host["CapAdd"]) {
        c = c.cap_add(cap.strip_prefix("CAP_").unwrap_or(&cap));
    }
    for cap in strings(&host["CapDrop"]) {
        c = c.cap_drop(cap.strip_prefix("CAP_").unwrap_or(&cap));
    }
    for opt in strings(&host["SecurityOpt"]) {
        let key = opt.split(['=', ':']).next().unwrap_or_default();
        if matches!(
            key,
            "apparmor" | "label" | "systempaths" | "writable-cgroups"
        ) {
            warnings.push(format!(
                "security option {opt} is ignored (podbag has no {key} support)"
            ));
            continue;
        }
        c = match crate::spec::parse_security_opt(&opt)? {
            SecurityOpt::NoNewPrivileges(yes) => c.no_new_privileges(yes),
            SecurityOpt::SeccompUnconfined => c.seccomp(false),
        };
    }
    c = c.privileged(host["Privileged"].as_bool().unwrap_or(false));
    let auto_remove = host["AutoRemove"].as_bool().unwrap_or(false);
    if auto_remove {
        c = c.remove_on_exit(true);
    }
    let policy = &host["RestartPolicy"];
    let restart = crate::restart::RestartPolicy::from_docker(
        policy["Name"].as_str().unwrap_or(""),
        policy["MaximumRetryCount"].as_i64().unwrap_or(0),
    )?;
    if auto_remove && restart != crate::restart::RestartPolicy::No {
        return Err(invalid(
            "cannot create the container",
            "AutoRemove and a restart policy do not go together",
            "drop one of them",
        ));
    }
    c = c.restart(restart);
    if let Some(settings) = crate::health::HealthSettings::from_docker(&body["Healthcheck"])? {
        c = c.health(settings);
    }
    // `[rows, columns]`: the terminal's first size (0: none given).
    let size = &host["ConsoleSize"];
    if let (Some(rows), Some(cols)) = (size[0].as_u64(), size[1].as_u64()) {
        c = c.console_size(
            u16::try_from(rows).unwrap_or(u16::MAX),
            u16::try_from(cols).unwrap_or(u16::MAX),
        );
    }
    warnings.extend(ignored(body));
    Ok((c, warnings))
}

/// `ExtraHosts` and the DNS settings (`Dns`, `DnsSearch`, `DnsOptions`). An entry with
/// `host-gateway` that cannot be found here (`gateway`) is left out with a warning:
/// Compose files often have one, and the container works without it.
fn names(
    mut c: Container,
    host: &Value,
    gateway: &dyn Fn() -> Result<crate::netfiles::HostGateway>,
    warnings: &mut Vec<String>,
) -> Result<Container> {
    use crate::netfiles::{ExtraHost, HOST_GATEWAY, HostAddress};
    let entries = strings(&host["ExtraHosts"])
        .iter()
        .map(|e| ExtraHost::parse(e))
        .collect::<Result<Vec<_>>>()?;
    // Asked only when an entry needs it.
    let refused = entries
        .iter()
        .any(|e| e.address == HostAddress::HostGateway)
        .then(gateway)
        .and_then(Result::err);
    for entry in entries {
        if let (HostAddress::HostGateway, Some(e)) = (entry.address, &refused) {
            warnings.push(format!(
                "{HOST_GATEWAY} is not available here ({}): extra host {} was not added; \
                 give the host's address as NAME:IP",
                e.why, entry.name
            ));
            continue;
        }
        c = c.add_host(entry);
    }
    for server in strings(&host["Dns"]) {
        c = c.dns_server(crate::netfiles::parse_dns_server(&server)?);
    }
    for domain in strings(&host["DnsSearch"]) {
        crate::netfiles::check_dns_search(&domain)?;
        c = c.dns_search(domain);
    }
    for option in strings(&host["DnsOptions"]) {
        crate::netfiles::check_dns_option(&option)?;
        c = c.dns_option(option);
    }
    Ok(c)
}

/// Settings podbag cannot honor: an error now beats a container that is not what was
/// asked for.
fn refuse_unsupported(host: &Value) -> Result<()> {
    let refuse = |what: &str, next: &str| {
        Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot create the container",
            format!("podbag does not support {what}"),
            next.to_string(),
        ))
    };
    let listed = |key: &str| host[key].as_array().is_some_and(|a| !a.is_empty());
    if listed("Devices") || listed("DeviceRequests") || listed("DeviceCgroupRules") {
        return refuse("devices", "drop --device and --gpus");
    }
    if host["ReadonlyRootfs"].as_bool().unwrap_or(false) {
        return refuse("a read-only root filesystem", "drop --read-only");
    }
    for key in ["PidMode", "IpcMode", "UTSMode", "UsernsMode"] {
        let mode = host[key].as_str().unwrap_or("");
        if mode == "host" || mode.starts_with("container:") {
            return refuse(
                &format!("{key} {mode}"),
                "drop the --pid/--ipc/--uts/--userns option",
            );
        }
    }
    if listed("VolumesFrom") {
        return refuse("--volumes-from", "mount the same named volumes with -v");
    }
    if listed("Links") {
        return refuse(
            "--link",
            "put the containers on one network: they find each other by name",
        );
    }
    Ok(())
}

/// Accepted settings that change nothing in podbag: warnings, not errors.
fn ignored(body: &Value) -> Vec<String> {
    let host = &body["HostConfig"];
    let mut out = Vec::new();
    let mut note = |text: String| out.push(text);
    let set = |v: &Value| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    };
    for (key, what) in [
        ("Ulimits", "ulimits"),
        ("ShmSize", "shm size"),
        ("Init", "init"),
        ("CpuShares", "cpu shares"),
        ("CpusetCpus", "cpuset"),
        ("MemoryReservation", "memory reservation"),
        ("GroupAdd", "added groups"),
        ("Sysctls", "sysctls"),
        ("OomKillDisable", "oom-kill-disable"),
        ("CgroupParent", "cgroup parent"),
        ("PublishAllPorts", "publish all ports (-P)"),
    ] {
        if set(&host[key]) {
            note(format!("{what} setting is ignored by podbag"));
        }
    }
    let driver = host["LogConfig"]["Type"].as_str().unwrap_or("");
    if !matches!(driver, "" | "json-file") {
        note(format!(
            "log driver {driver} is ignored: podbag writes logs as json-file"
        ));
    }
    for (key, what) in [
        ("StopSignal", "stop signal"),
        ("Domainname", "domain name"),
        ("MacAddress", "mac address"),
    ] {
        if set(&body[key]) {
            note(format!("{what} setting is ignored by podbag"));
        }
    }
    out
}

/// `null` → None, `"x"` → ["x"] (older clients), a list → its strings.
fn str_list(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(_) => Some(strings(v)),
        _ => None,
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(String::from))
        .collect()
}

fn non_empty(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

fn invalid(what: impl Into<String>, why: impl Into<String>, next: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, what, why, next.to_string())
}

/// A host path must be absolute: the API does not know the client's directory.
fn check_host_path(path: &str, spec: &str) -> Result<()> {
    if path.starts_with('/') {
        Ok(())
    } else {
        Err(invalid(
            format!("invalid mount {spec}"),
            format!("{path} is neither an absolute host path nor a volume name"),
            "give the host path from /",
        ))
    }
}

/// The host paths a create body binds (`HostConfig.Binds` and `Mounts` of type bind).
pub fn bind_sources(body: &Value) -> Result<Vec<String>> {
    let host = &body["HostConfig"];
    let mut sources = Vec::new();
    for bind in strings(&host["Binds"]) {
        if let (MountRequest::Bind { source, .. }, _) = parse_bind(&bind)? {
            sources.push(source);
        }
    }
    for m in host["Mounts"].as_array().into_iter().flatten() {
        if let (MountRequest::Bind { source, .. }, _) = parse_mount(m)? {
            sources.push(source);
        }
    }
    Ok(sources)
}

/// Whether `path` is `root` or below it, with links followed as far as the path exists
/// and `..` taken out (a link under `root` may lead anywhere).
pub fn lies_under(path: &std::path::Path, root: &std::path::Path) -> bool {
    use std::path::{Component, PathBuf};
    fn resolve(path: &std::path::Path) -> PathBuf {
        if let Ok(real) = path.canonicalize() {
            return real;
        }
        match (path.parent(), path.file_name()) {
            (Some(dir), Some(name)) => resolve(dir).join(name),
            _ => {
                let mut clean = PathBuf::from("/");
                for c in path.components() {
                    match c {
                        Component::ParentDir => {
                            clean.pop();
                        }
                        Component::Normal(n) => clean.push(n),
                        _ => {}
                    }
                }
                clean
            }
        }
    }
    resolve(path).starts_with(root)
}

/// A `HostConfig.Binds` entry: `src:dst[:opts]`, src a host path or a volume name.
fn parse_bind(spec: &str) -> Result<(MountRequest, Option<String>)> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (source, target, opts) = match parts.as_slice() {
        [s, t] => (*s, *t, ""),
        [s, t, o] => (*s, *t, *o),
        _ => {
            return Err(invalid(
                format!("invalid bind {spec}"),
                "a bind is source:target[:options]",
                "give both a source and a target",
            ));
        }
    };
    let mut access = Access::ReadWrite;
    let mut nocopy = false;
    let mut warning = None;
    for opt in opts.split(',').filter(|o| !o.is_empty()) {
        match opt {
            "ro" => access = Access::ReadOnly,
            "rw" => access = Access::ReadWrite,
            "nocopy" => nocopy = true,
            "z" | "Z" | "consistent" | "cached" | "delegated" => {}
            "private" | "rprivate" | "shared" | "rshared" | "slave" | "rslave" => {
                warning = Some(format!("mount propagation {opt} on {target} is ignored"));
            }
            _ => {
                return Err(invalid(
                    format!("invalid bind {spec}"),
                    format!("unknown option {opt}"),
                    "use ro, rw or nocopy",
                ));
            }
        }
    }
    let target = target.to_string();
    if source.starts_with('/') {
        return Ok((
            MountRequest::Bind {
                source: source.to_string(),
                target,
                access,
            },
            warning,
        ));
    }
    if crate::volume::check_name(source).is_err() {
        check_host_path(source, spec)?;
    }
    Ok((
        MountRequest::Named {
            name: source.to_string(),
            target,
            access,
            nocopy,
        },
        warning,
    ))
}

/// A `HostConfig.Mounts` entry.
fn parse_mount(m: &Value) -> Result<(MountRequest, Option<String>)> {
    let kind = m["Type"].as_str().unwrap_or("");
    let source = m["Source"].as_str().unwrap_or("");
    let target = m["Target"].as_str().unwrap_or("").to_string();
    let spec = format!("type={kind},target={target}");
    if target.is_empty() {
        return Err(invalid(
            format!("invalid mount {spec}"),
            "it has no target",
            "give a target",
        ));
    }
    let access = if m["ReadOnly"].as_bool().unwrap_or(false) {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    let propagation = m["BindOptions"]["Propagation"]
        .as_str()
        .filter(|p| !p.is_empty());
    let warning = propagation.map(|p| format!("mount propagation {p} on {target} is ignored"));
    let request = match kind {
        "bind" => {
            check_host_path(source, &spec)?;
            MountRequest::Bind {
                source: source.to_string(),
                target,
                access,
            }
        }
        "volume" => {
            let driver = m["VolumeOptions"]["DriverConfig"]["Name"]
                .as_str()
                .unwrap_or("");
            use crate::volume::Backend;
            let opts =
                super::objects::driver_options(&m["VolumeOptions"]["DriverConfig"]["Options"])?;
            let backend = crate::volume::backend(driver, opts)?;
            if source.is_empty() {
                if !matches!(backend, Backend::Directory) {
                    return Err(invalid(
                        format!("invalid mount {spec}"),
                        "a mount with a configured driver needs a volume name",
                        "set a source name for the volume",
                    ));
                }
                MountRequest::Anonymous { target, access }
            } else {
                crate::volume::check_name(source)?;
                match backend {
                    Backend::Nfs(nfs) => MountRequest::NamedNfs {
                        name: source.to_string(),
                        target,
                        access,
                        nfs,
                    },
                    Backend::Plugin(plugin) => MountRequest::NamedDriver {
                        name: source.to_string(),
                        target,
                        access,
                        plugin,
                    },
                    Backend::Directory | Backend::ExistingOrDirectory => MountRequest::Named {
                        name: source.to_string(),
                        target,
                        access,
                        nocopy: m["VolumeOptions"]["NoCopy"].as_bool().unwrap_or(false),
                    },
                }
            }
        }
        "tmpfs" => {
            let mut options = Vec::new();
            if let Some(size) = m["TmpfsOptions"]["SizeBytes"].as_u64().filter(|s| *s > 0) {
                options.push(format!("size={size}"));
            }
            if let Some(mode) = m["TmpfsOptions"]["Mode"].as_u64().filter(|s| *s > 0) {
                options.push(format!("mode={mode:o}"));
            }
            if access == Access::ReadOnly {
                options.push("ro".to_string());
            }
            MountRequest::Tmpfs {
                target,
                options: options.join(","),
            }
        }
        _ => {
            return Err(invalid(
                format!("invalid mount {spec}"),
                format!("mount type {kind:?} is not supported"),
                "use bind, volume or tmpfs",
            ));
        }
    };
    Ok((request, warning))
}

/// The aliases on the primary network, and the further networks, of an
/// `EndpointsConfig` (Compose names every network of a service there).
fn endpoints(
    v: &Value,
    primary: &NetworkMode,
    warnings: &mut Vec<String>,
) -> Result<(Vec<String>, Vec<crate::spec::Attachment>)> {
    // The keys the docker CLI and Compose use for the primary network.
    let primary_keys: Vec<&str> = match primary {
        NetworkMode::Bridge { network } if network == crate::network::DEFAULT => {
            vec![crate::network::DEFAULT, "default"]
        }
        NetworkMode::Bridge { network } => vec![network.as_str()],
        NetworkMode::Host => vec!["host"],
        NetworkMode::None => vec!["none"],
    };
    let mut aliases = Vec::new();
    let mut extra = Vec::new();
    for (key, cfg) in v.as_object().into_iter().flatten() {
        let names = match &cfg["Aliases"] {
            Value::Null => Vec::new(),
            Value::Array(list) => list
                .iter()
                .map(|a| {
                    a.as_str().map(str::to_string).ok_or_else(|| {
                        invalid(
                            "cannot create the container",
                            format!("the aliases on network {key} are not all strings"),
                            "give the aliases as strings",
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            _ => {
                return Err(invalid(
                    "cannot create the container",
                    format!("the aliases on network {key} are not a list"),
                    "give the aliases as a list of strings",
                ));
            }
        };
        if cfg["IPAMConfig"]["IPv4Address"]
            .as_str()
            .is_some_and(|a| !a.is_empty())
        {
            warnings.push(format!("a fixed address on {key} is ignored"));
        }
        if primary_keys.contains(&key.as_str()) {
            aliases = names;
        } else {
            extra.push(crate::spec::Attachment {
                network: match key.as_str() {
                    "default" => crate::network::DEFAULT.to_string(),
                    other => other.to_string(),
                },
                aliases: names,
            });
        }
    }
    Ok((aliases, extra))
}

fn network_mode(mode: &str) -> Result<NetworkMode> {
    Ok(match mode {
        "" | "default" | "bridge" => NetworkMode::default(),
        "host" => NetworkMode::Host,
        "none" => NetworkMode::None,
        m if m.starts_with("container:") => {
            return Err(invalid(
                "cannot create the container",
                format!("network mode {m} is not supported"),
                "put both containers on one network instead",
            ));
        }
        name => NetworkMode::Bridge {
            network: name.to_string(),
        },
    })
}

/// `{"80/tcp": [{"HostIp": "", "HostPort": "8080"}]}`.
fn port_bindings(v: &Value) -> Result<Vec<PortMapping>> {
    let mut out = Vec::new();
    for (key, bindings) in v.as_object().into_iter().flatten() {
        let bad = |why: String| {
            invalid(
                format!("cannot publish {key}"),
                why,
                "publish one port, like 8080:80",
            )
        };
        let (port, proto) = key.split_once('/').unwrap_or((key.as_str(), "tcp"));
        let proto = match proto {
            "tcp" => Proto::Tcp,
            "udp" => Proto::Udp,
            p => return Err(bad(format!("protocol {p} is not supported"))),
        };
        let container_port: u16 = port
            .parse()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| bad(format!("{port} is not one port number")))?;
        for b in bindings.as_array().into_iter().flatten() {
            let host_port = match b["HostPort"].as_str().unwrap_or("") {
                "" => 0,
                p => p
                    .parse::<u16>()
                    .map_err(|_| bad(format!("host port {p} is not one port number")))?,
            };
            let host_ip = match b["HostIp"].as_str().unwrap_or("") {
                "" => std::net::Ipv4Addr::UNSPECIFIED,
                ip => ip
                    .parse()
                    .map_err(|_| bad(format!("host address {ip} is not an IPv4 address")))?,
            };
            out.push(PortMapping {
                host_ip,
                host_port,
                container_port,
                proto,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn image() -> Image {
        Image {
            name: "docker.io/library/alpine:latest".into(),
            digest: format!("sha256:{}", "a".repeat(64)),
            size: 1,
            config: Default::default(),
        }
    }

    fn fields(c: &Container) -> Value {
        serde_json::to_value(c).unwrap()
    }

    /// A create with `host_config`, where `host-gateway` can be found.
    fn create(host_config: Value) -> Result<(Container, Vec<String>)> {
        create_where(host_config, &|_| Ok(crate::netfiles::HostGateway::Bridge))
    }

    fn create_where(
        host_config: Value,
        gateway: GatewayRule<'_>,
    ) -> Result<(Container, Vec<String>)> {
        map_create_with(
            &json!({"Image": "alpine", "HostConfig": host_config}),
            None,
            &image(),
            gateway,
        )
    }

    #[test]
    fn bind_sources_lie_under_a_root_after_links_and_dots() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("in")).unwrap();
        std::os::unix::fs::symlink("/", root.join("out")).unwrap();
        assert!(lies_under(&root.join("in"), &root));
        assert!(lies_under(&root.join("in/not/made/yet"), &root));
        assert!(!lies_under(&root.join("in/../../x"), &root));
        assert!(!lies_under(&root.join("out/tmp"), &root));
        let sibling = format!("{}-other", root.display());
        assert!(!lies_under(std::path::Path::new(&sibling), &root));
        let body = json!({"HostConfig": {
            "Binds": ["/a:/a", "vol:/v"],
            "Mounts": [{"Type": "bind", "Source": "/b", "Target": "/b"},
                       {"Type": "volume", "Source": "v2", "Target": "/v2"}]
        }});
        assert_eq!(bind_sources(&body).unwrap(), ["/a", "/b"]);
    }

    #[test]
    fn create_maps_docker_fields() {
        let body = json!({
            "Image": "alpine",
            "Cmd": ["sh", "-c", "x"],
            "Entrypoint": null,
            "Env": ["A=1", "B=two=2", "NOVALUE"],
            "WorkingDir": "/w",
            "User": "1000:1000",
            "Hostname": "h1",
            "Tty": true,
            "OpenStdin": true,
            "Labels": {"app": "x"},
            "Volumes": {"/anon": {}},
            "HostConfig": {
                "Binds": ["/host/a:/a:ro", "vol:/v", "vol2:/v2:nocopy,rw"],
                "Mounts": [
                    {"Type": "tmpfs", "Target": "/t", "TmpfsOptions": {"SizeBytes": 1048576, "Mode": 1023}},
                    {"Type": "bind", "Source": "/host/b", "Target": "/b", "ReadOnly": true},
                    {"Type": "volume", "Source": "v3", "Target": "/v3", "VolumeOptions": {"NoCopy": true}},
                    {"Type": "volume", "Target": "/v4"}
                ],
                "Tmpfs": {"/run": "size=64m"},
                "PortBindings": {
                    "80/tcp": [{"HostIp": "", "HostPort": "8080"}],
                    "53/udp": [{"HostIp": "127.0.0.1", "HostPort": ""}]
                },
                "NetworkMode": "mynet",
                "Memory": 1048576,
                "NanoCpus": 1500000000u64,
                "PidsLimit": 100,
                "CapAdd": ["CAP_NET_ADMIN"],
                "CapDrop": ["MKNOD"],
                "SecurityOpt": ["no-new-privileges:true", "seccomp=unconfined"],
                "AutoRemove": true,
                "RestartPolicy": {"Name": "no", "MaximumRetryCount": 0},
                "LogConfig": {"Type": "json-file", "Config": {}},
                "Dns": [], "ExtraHosts": null, "ShmSize": 0, "Ulimits": []
            }
        });
        let (c, warnings) = map_create(&body, Some("web"), &image()).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let f = fields(&c);
        assert_eq!(f["command"], json!(["sh", "-c", "x"]));
        assert_eq!(f["entrypoint"], Value::Null);
        assert_eq!(f["env"], json!([["A", "1"], ["B", "two=2"]]));
        assert_eq!(f["workdir"], "/w");
        assert_eq!(f["user"], "1000:1000");
        assert_eq!(f["hostname"], "h1");
        assert_eq!(f["tty"], true);
        assert_eq!(f["interactive"], true);
        assert_eq!(f["name"], "web");
        assert_eq!(f["auto_remove"], true);
        assert_eq!(f["labels"], json!({"app": "x"}));
        assert_eq!(f["memory"], 1048576);
        assert_eq!(f["cpus"], 1.5);
        assert_eq!(f["pids_limit"], 100);
        assert_eq!(f["security"]["cap_add"], json!(["NET_ADMIN"]));
        assert_eq!(f["security"]["cap_drop"], json!(["MKNOD"]));
        assert_eq!(f["security"]["no_new_privileges"], true);
        assert_eq!(f["security"]["seccomp"], false);
        assert_eq!(
            f["network"],
            serde_json::to_value(NetworkMode::Bridge {
                network: "mynet".into()
            })
            .unwrap()
        );
        let mut publish: Vec<Value> = f["publish"].as_array().unwrap().clone();
        publish.sort_by_key(|p| p["container_port"].as_u64());
        assert_eq!(
            publish,
            vec![
                serde_json::to_value(PortMapping {
                    host_ip: "127.0.0.1".parse().unwrap(),
                    host_port: 0,
                    container_port: 53,
                    proto: Proto::Udp,
                })
                .unwrap(),
                serde_json::to_value(PortMapping {
                    host_ip: "0.0.0.0".parse().unwrap(),
                    host_port: 8080,
                    container_port: 80,
                    proto: Proto::Tcp,
                })
                .unwrap(),
            ]
        );
        let want = [
            MountRequest::Bind {
                source: "/host/a".into(),
                target: "/a".into(),
                access: Access::ReadOnly,
            },
            MountRequest::Named {
                name: "vol".into(),
                target: "/v".into(),
                access: Access::ReadWrite,
                nocopy: false,
            },
            MountRequest::Named {
                name: "vol2".into(),
                target: "/v2".into(),
                access: Access::ReadWrite,
                nocopy: true,
            },
            MountRequest::Tmpfs {
                target: "/t".into(),
                options: "size=1048576,mode=1777".into(),
            },
            MountRequest::Bind {
                source: "/host/b".into(),
                target: "/b".into(),
                access: Access::ReadOnly,
            },
            MountRequest::Named {
                name: "v3".into(),
                target: "/v3".into(),
                access: Access::ReadWrite,
                nocopy: true,
            },
            MountRequest::Anonymous {
                target: "/v4".into(),
                access: Access::ReadWrite,
            },
            MountRequest::Tmpfs {
                target: "/run".into(),
                options: "size=64m".into(),
            },
            MountRequest::Anonymous {
                target: "/anon".into(),
                access: Access::ReadWrite,
            },
        ];
        let mounts = f["mounts"].as_array().unwrap();
        for m in &want {
            let m = serde_json::to_value(m).unwrap();
            assert!(mounts.contains(&m), "{m} missing from {mounts:?}");
        }
        assert_eq!(mounts.len(), want.len(), "{mounts:?}");
    }

    #[test]
    fn docker_external_driver_mounts_keep_names_and_options() {
        let (c, _) = create(json!({
            "VolumeDriver": "fixture",
            "Binds": ["bare:/a:ro"],
            "Mounts": [{
                "Type": "volume", "Source": "structured", "Target": "/b",
                "VolumeOptions": {"DriverConfig": {"Name":"fixture", "Options":{"pool":"dev"}}}
            }],
        }))
        .unwrap();
        let f = fields(&c);
        assert_eq!(f["mounts"][0]["type"], "named_driver");
        assert_eq!(f["mounts"][0]["access"], "ReadOnly");
        assert_eq!(f["mounts"][0]["plugin"]["name"], "fixture");
        assert_eq!(f["mounts"][1]["name"], "structured");
        assert_eq!(f["mounts"][1]["plugin"]["options"]["pool"], "dev");
        assert!(create(json!({"Mounts":[{"Type":"volume","Target":"/data", "VolumeOptions":{"DriverConfig":{"Name":"fixture"}}}]})).is_err());
    }

    #[test]
    fn entrypoint_null_keeps_and_empty_clears() {
        let entry = |v: Value| {
            let (c, _) =
                map_create(&json!({"Image": "alpine", "Entrypoint": v}), None, &image()).unwrap();
            fields(&c)["entrypoint"].clone()
        };
        assert_eq!(entry(Value::Null), Value::Null);
        assert_eq!(entry(json!([])), json!([]));
        assert_eq!(entry(json!([""])), json!([]));
        assert_eq!(entry(json!(["/init", "-v"])), json!(["/init", "-v"]));
        assert_eq!(entry(json!("/init")), json!(["/init"]));
    }

    #[test]
    fn network_modes_map() {
        let net = |mode: &str| {
            let (c, _) = create(json!({"NetworkMode": mode})).unwrap();
            fields(&c)["network"].clone()
        };
        let default = serde_json::to_value(NetworkMode::default()).unwrap();
        assert_eq!(net("default"), default);
        assert_eq!(net("bridge"), default);
        assert_eq!(net(""), default);
        assert_eq!(
            net("host"),
            serde_json::to_value(NetworkMode::Host).unwrap()
        );
        assert_eq!(
            net("none"),
            serde_json::to_value(NetworkMode::None).unwrap()
        );
    }

    #[test]
    fn create_refuses_what_podbag_cannot_do() {
        let cases = [
            (
                json!({"Devices": [{"PathOnHost": "/dev/fuse", "PathInContainer": "/dev/fuse", "CgroupPermissions": "rwm"}]}),
                ErrorKind::InvalidInput,
            ),
            (json!({"Binds": ["rel/path:/x"]}), ErrorKind::InvalidInput),
            (json!({"Binds": ["./x:/x"]}), ErrorKind::InvalidInput),
            (
                json!({"Binds": ["/only-one-part"]}),
                ErrorKind::InvalidInput,
            ),
            (
                json!({"NetworkMode": "container:abc"}),
                ErrorKind::InvalidInput,
            ),
            (json!({"ReadonlyRootfs": true}), ErrorKind::InvalidInput),
            (json!({"PidMode": "host"}), ErrorKind::InvalidInput),
            (json!({"IpcMode": "host"}), ErrorKind::InvalidInput),
            (json!({"VolumesFrom": ["other"]}), ErrorKind::InvalidInput),
            (json!({"Links": ["other:o"]}), ErrorKind::InvalidInput),
            (
                json!({"PortBindings": {"80/tcp": [{"HostPort": "8000-8010"}]}}),
                ErrorKind::InvalidInput,
            ),
            (
                json!({"PortBindings": {"80/sctp": [{"HostPort": "80"}]}}),
                ErrorKind::InvalidInput,
            ),
            (
                json!({"Mounts": [{"Type": "bind", "Source": "rel", "Target": "/r"}]}),
                ErrorKind::InvalidInput,
            ),
            (
                json!({"Mounts": [{"Type": "npipe", "Source": "x", "Target": "/r"}]}),
                ErrorKind::InvalidInput,
            ),
            (
                json!({"SecurityOpt": ["seccomp={\"defaultAction\":\"SCMP_ACT_ALLOW\"}"]}),
                ErrorKind::InvalidInput,
            ),
        ];
        for (host_config, kind) in cases {
            match create(host_config.clone()) {
                Err(e) => assert_eq!(e.kind, kind, "{host_config}: {e}"),
                Ok(_) => panic!("{host_config} was accepted"),
            }
        }
    }

    fn with_endpoints(mode: &str, endpoints: Value) -> Result<(Container, Vec<String>)> {
        map_create(
            &json!({
                "Image": "alpine",
                "HostConfig": {"NetworkMode": mode},
                "NetworkingConfig": {"EndpointsConfig": endpoints}
            }),
            None,
            &image(),
        )
    }

    #[test]
    fn endpoints_give_aliases_and_further_networks() {
        // As Compose sends them: every network of the service, each with its aliases.
        let (c, warnings) = with_endpoints(
            "app",
            json!({
                "app": {"Aliases": ["web", "p-web-1"]},
                "back": {"Aliases": ["w"], "IPAMConfig": {"IPv4Address": "10.9.9.9"}},
                "zed": {}
            }),
        )
        .unwrap();
        let f = fields(&c);
        assert_eq!(f["aliases"], json!(["web", "p-web-1"]));
        assert_eq!(
            f["extra_networks"],
            json!([{"network": "back", "aliases": ["w"]}, {"network": "zed"}])
        );
        let text = warnings.join("\n");
        assert!(text.contains("fixed address"), "{warnings:?}");
        assert!(
            !text.contains("alias") && !text.contains("only the network"),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_key_of_the_primary_network_may_be_its_mode() {
        // The docker CLI names the default network "default" or "bridge".
        for (mode, key) in [("default", "default"), ("bridge", "bridge"), ("", "bridge")] {
            let (c, _) = with_endpoints(mode, json!({ key: {} })).unwrap();
            let f = fields(&c);
            assert_eq!(
                f["network"],
                serde_json::to_value(NetworkMode::default()).unwrap()
            );
            assert_eq!(f["extra_networks"], json!([]), "{mode} {key}");
        }
        let (c, _) = with_endpoints("host", json!({"host": {}})).unwrap();
        assert_eq!(
            fields(&c)["network"],
            serde_json::to_value(NetworkMode::Host).unwrap()
        );
    }

    #[test]
    fn aliases_need_a_user_network() {
        for (mode, endpoints) in [
            ("default", json!({"default": {"Aliases": ["a"]}})),
            ("bridge", json!({"bridge": {"Aliases": ["a"]}})),
            ("host", json!({"host": {"Aliases": ["a"]}})),
            // The host's network or none with a user network.
            ("host", json!({"app": {}})),
            ("none", json!({"none": {}, "app": {}})),
            // An alias that would break the hosts file.
            ("app", json!({"app": {"Aliases": ["a b"]}})),
            ("app", json!({"app": {}, "back": {"Aliases": ["x\ny"]}})),
            ("app", json!({"app": {"Aliases": "web"}})),
        ] {
            match with_endpoints(mode, endpoints.clone()) {
                Err(e) => assert_eq!(e.kind, ErrorKind::InvalidInput, "{mode} {endpoints}: {e}"),
                Ok(_) => panic!("{mode} {endpoints} was accepted"),
            }
        }
    }

    #[test]
    fn create_keeps_integer_stop_timeouts() {
        for value in [json!(null), json!(0), json!(2), json!(-1)] {
            let (container, warnings) =
                map_create(&json!({"StopTimeout": value}), None, &image()).unwrap();
            assert!(warnings.is_empty());
            assert_eq!(fields(&container)["stop_timeout"], value);
        }
        assert_eq!(
            map_create(&json!({"StopTimeout": "2"}), None, &image())
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn create_warns_about_ignored_fields() {
        let body = json!({
            "Image": "alpine",
            "Healthcheck": {"Test": ["CMD", "true"]},
            "StopSignal": "SIGINT",
            "HostConfig": {
                "RestartPolicy": {"Name": "always"},
                "Dns": ["1.1.1.1"],
                "DnsSearch": ["lan"],
                "DnsOptions": ["ndots:2"],
                "ExtraHosts": ["a:1.2.3.4"],
                "LogConfig": {"Type": "syslog"},
                "Init": true,
                "ShmSize": 1000,
                "Ulimits": [{"Name": "nofile", "Soft": 1, "Hard": 2}],
                "SecurityOpt": ["apparmor=unconfined"],
                "NetworkMode": "host",
                "PortBindings": {"80/tcp": [{"HostPort": "80"}]}
            }
        });
        let (c, warnings) = map_create(&body, None, &image()).unwrap();
        let text = warnings.join("\n").to_lowercase();
        for word in [
            "log driver",
            "init",
            "shm size",
            "ulimits",
            "stop signal",
            "apparmor",
            "published ports",
        ] {
            assert!(text.contains(word), "no warning about {word}: {warnings:?}");
        }
        assert_eq!(
            fields(&c)["publish"],
            json!([]),
            "dropped with the host network"
        );
        // Taken, not ignored.
        assert_eq!(fields(&c)["restart"], json!("always"));
        assert_eq!(fields(&c)["health"]["test"], json!(["CMD", "true"]));
        assert!(!text.contains("health"), "{warnings:?}");
        assert!(!text.contains("restart"), "{warnings:?}");
        assert!(!text.contains("dns"), "{warnings:?}");
        assert!(!text.contains("extra hosts"), "{warnings:?}");
    }

    #[test]
    fn extra_hosts_and_dns_are_applied() {
        let (c, warnings) = create(json!({
            "ExtraHosts": ["db:10.1.2.3", "v6=[fd00::1]", "host.docker.internal:host-gateway"],
            "Dns": ["9.9.9.9", "2620:fe::fe"],
            "DnsSearch": ["corp.example", "."],
            "DnsOptions": ["ndots:2", "edns0"]
        }))
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let f = fields(&c);
        assert_eq!(
            f["extra_hosts"],
            json!([
                {"name": "db", "address": {"ip": "10.1.2.3"}},
                {"name": "v6", "address": {"ip": "fd00::1"}},
                {"name": "host.docker.internal", "address": "host_gateway"}
            ])
        );
        assert_eq!(
            f["dns"],
            json!({
                "servers": ["9.9.9.9", "2620:fe::fe"],
                "search": ["corp.example", "."],
                "options": ["ndots:2", "edns0"]
            })
        );
    }

    #[test]
    fn an_unknown_host_gateway_is_left_out_with_a_warning() {
        use crate::netfiles::{Place, host_gateway};
        // A rootless bridge has no address for the host: only that entry goes.
        let rootless = |mode: &NetworkMode| host_gateway(mode, true, Place::Linux);
        let (c, warnings) = create_where(
            json!({
                "ExtraHosts": ["host.docker.internal:host-gateway", "db:10.1.2.3"],
                "Dns": ["9.9.9.9"]
            }),
            &rootless,
        )
        .unwrap();
        let f = fields(&c);
        assert_eq!(
            f["extra_hosts"],
            json!([{"name": "db", "address": {"ip": "10.1.2.3"}}])
        );
        assert_eq!(f["dns"]["servers"], json!(["9.9.9.9"]));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let text = &warnings[0];
        for part in [
            "host-gateway",
            "rootless",
            "host.docker.internal",
            "NAME:IP",
        ] {
            assert!(text.contains(part), "no {part} in {text}");
        }
        // The same with no network, and on WSL.
        let (c, warnings) = create_where(
            json!({"NetworkMode": "none", "ExtraHosts": ["gw:host-gateway"]}),
            &|mode| host_gateway(mode, false, Place::Linux),
        )
        .unwrap();
        assert_eq!(fields(&c)["extra_hosts"], json!([]));
        assert!(warnings[0].contains("no network"), "{warnings:?}");
        let (_, warnings) = create_where(json!({"ExtraHosts": ["gw:host-gateway"]}), &|mode| {
            host_gateway(mode, false, Place::Wsl)
        })
        .unwrap();
        assert!(warnings[0].contains("WSL"), "{warnings:?}");
        // Where it can be found, it stays, and the rule is not asked without one.
        let (c, warnings) = create_where(json!({"ExtraHosts": ["gw:host-gateway"]}), &|mode| {
            host_gateway(mode, false, Place::Linux)
        })
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(fields(&c)["extra_hosts"][0]["address"], "host_gateway");
        create_where(json!({"ExtraHosts": ["db:10.0.0.1"]}), &|_| {
            panic!("not needed")
        })
        .unwrap();
    }

    #[test]
    fn bad_extra_hosts_and_dns_are_refused() {
        for host_config in [
            json!({"ExtraHosts": ["db"]}),
            json!({"ExtraHosts": [":10.0.0.1"]}),
            json!({"ExtraHosts": ["db:999.0.0.1"]}),
            json!({"Dns": ["dns.example"]}),
            json!({"DnsSearch": ["bad domain"]}),
            json!({"DnsOptions": ["ndots:2 edns0"]}),
        ] {
            match create(host_config.clone()) {
                Err(e) => assert_eq!(e.kind, ErrorKind::InvalidInput, "{host_config}: {e}"),
                Ok(_) => panic!("{host_config} was accepted"),
            }
        }
    }
}
