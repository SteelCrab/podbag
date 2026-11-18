//! `POST /containers/create`: Docker's container config as an openpod [`Container`].

use crate::image::Image;
use crate::network::{NetworkMode, PortMapping, Proto};
use crate::spec::SecurityOpt;
use crate::{Access, Container, Error, ErrorKind, MountRequest, Result};
use serde_json::Value;

/// The container a create body describes, and warnings about settings openpod ignores.
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
    c = c.tty(body["Tty"].as_bool().unwrap_or(false));
    c = c.interactive(body["OpenStdin"].as_bool().unwrap_or(false));
    if let Some(labels) = body["Labels"].as_object() {
        for (k, v) in labels {
            c = c.label(k, v.as_str().unwrap_or_default());
        }
    }
    let mut targets: Vec<String> = Vec::new();
    let mount = |c: Container, targets: &mut Vec<String>, m: MountRequest| {
        targets.push(m.target().to_string());
        c.mount(m)
    };
    for bind in strings(&host["Binds"]) {
        let (m, warning) = parse_bind(&bind)?;
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
    c = c.network(network);
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
                "security option {opt} is ignored (openpod has no {key} support)"
            ));
            continue;
        }
        c = match crate::spec::parse_security_opt(&opt)? {
            SecurityOpt::NoNewPrivileges(yes) => c.no_new_privileges(yes),
            SecurityOpt::SeccompUnconfined => c.seccomp(false),
        };
    }
    if host["AutoRemove"].as_bool().unwrap_or(false) {
        c = c.remove_on_exit(true);
    }
    warnings.extend(ignored(body));
    Ok((c, warnings))
}

/// Settings openpod cannot honor: an error now beats a container that is not what was
/// asked for.
fn refuse_unsupported(host: &Value) -> Result<()> {
    if host["Privileged"].as_bool().unwrap_or(false) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "cannot create a privileged container",
            "openpod does not run privileged containers yet",
            "drop --privileged, or add only the capabilities it needs with --cap-add",
        ));
    }
    let refuse = |what: &str, next: &str| {
        Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot create the container",
            format!("openpod does not support {what}"),
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

/// Accepted settings that change nothing in openpod: warnings, not errors.
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
    let restart = host["RestartPolicy"]["Name"].as_str().unwrap_or("");
    if !matches!(restart, "" | "no") {
        note(format!(
            "restart policy {restart} is ignored: openpod does not restart containers yet"
        ));
    }
    for (key, what) in [
        ("Dns", "dns servers"),
        ("DnsSearch", "dns search domains"),
        ("DnsOptions", "dns options"),
        ("ExtraHosts", "extra hosts"),
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
            note(format!("{what} setting is ignored by openpod"));
        }
    }
    let driver = host["LogConfig"]["Type"].as_str().unwrap_or("");
    if !matches!(driver, "" | "json-file") {
        note(format!(
            "log driver {driver} is ignored: openpod writes logs as json-file"
        ));
    }
    let test = body["Healthcheck"]["Test"].as_array();
    if test.is_some_and(|t| !t.is_empty() && t[0] != "NONE") {
        note("healthcheck is ignored: openpod does not run health checks yet".to_string());
    }
    for (key, what) in [
        ("StopSignal", "stop signal"),
        ("StopTimeout", "stop timeout"),
        ("Domainname", "domain name"),
        ("MacAddress", "mac address"),
    ] {
        if set(&body[key]) {
            note(format!("{what} setting is ignored by openpod"));
        }
    }
    let endpoints = body["NetworkingConfig"]["EndpointsConfig"].as_object();
    for (net, cfg) in endpoints.into_iter().flatten() {
        if set(&cfg["Aliases"]) {
            note(format!(
                "network aliases on {net} are ignored: containers are found by name"
            ));
        }
        if set(&cfg["IPAMConfig"]["IPv4Address"]) {
            note(format!("a fixed address on {net} is ignored"));
        }
    }
    if endpoints.is_some_and(|e| e.len() > 1) {
        note("only the network in HostConfig.NetworkMode is joined".to_string());
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
        "volume" if source.is_empty() => MountRequest::Anonymous { target, access },
        "volume" => {
            let driver = m["VolumeOptions"]["DriverConfig"]["Name"]
                .as_str()
                .unwrap_or("");
            if !matches!(driver, "" | "local") {
                return Err(invalid(
                    format!("invalid mount {spec}"),
                    format!("volume driver {driver} is not supported"),
                    "use local volumes",
                ));
            }
            crate::volume::check_name(source)?;
            MountRequest::Named {
                name: source.to_string(),
                target,
                access,
                nocopy: m["VolumeOptions"]["NoCopy"].as_bool().unwrap_or(false),
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

    fn create(host_config: Value) -> Result<(Container, Vec<String>)> {
        map_create(
            &json!({"Image": "alpine", "HostConfig": host_config}),
            None,
            &image(),
        )
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
    fn create_refuses_what_openpod_cannot_do() {
        let cases = [
            (json!({"Privileged": true}), ErrorKind::Unsupported),
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

    #[test]
    fn create_warns_about_ignored_fields() {
        let body = json!({
            "Image": "alpine",
            "Healthcheck": {"Test": ["CMD", "true"]},
            "StopSignal": "SIGINT",
            "HostConfig": {
                "RestartPolicy": {"Name": "always"},
                "Dns": ["1.1.1.1"],
                "ExtraHosts": ["a:1.2.3.4"],
                "LogConfig": {"Type": "syslog"},
                "Init": true,
                "ShmSize": 1000,
                "Ulimits": [{"Name": "nofile", "Soft": 1, "Hard": 2}],
                "SecurityOpt": ["apparmor=unconfined"],
                "NetworkMode": "host",
                "PortBindings": {"80/tcp": [{"HostPort": "80"}]}
            },
            "NetworkingConfig": {"EndpointsConfig": {"host": {"Aliases": ["a"]}}}
        });
        let (c, warnings) = map_create(&body, None, &image()).unwrap();
        let text = warnings.join("\n").to_lowercase();
        for word in [
            "restart policy",
            "dns",
            "extra hosts",
            "log driver",
            "init",
            "shm size",
            "ulimits",
            "healthcheck",
            "stop signal",
            "apparmor",
            "published ports",
            "aliases",
        ] {
            assert!(text.contains(word), "no warning about {word}: {warnings:?}");
        }
        assert_eq!(
            fields(&c)["publish"],
            json!([]),
            "dropped with the host network"
        );
    }
}
