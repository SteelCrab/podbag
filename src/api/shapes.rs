//! Docker's JSON for `GET /containers/json` and `GET /containers/{id}/json`.

use super::docker::{rfc3339, state_label, status_text};
use crate::containers::MountInfo;
use crate::network::{NetworkMode, PortMapping, Proto};
use crate::spec::ContainerSpec;
use crate::state::{State, Status};
use serde_json::{Value, json};
use std::path::Path;

/// What both shapes are made from.
pub struct Facts<'a> {
    pub spec: &'a ContainerSpec,
    pub state: &'a State,
    /// The image name as `docker ps` shows it (or a `--rootfs` directory).
    pub image: &'a str,
    pub mounts: &'a [MountInfo],
    /// The container's networks, its primary one first (`host` or `none` alone).
    pub networks: &'a [Attached<'a>],
    /// The container's directory.
    pub dir: &'a Path,
    /// Where named volumes live (`<data>/volumes`).
    pub volumes: &'a Path,
    pub now: u64,
    /// How many times the restart policy started it again.
    pub restart_count: u32,
    /// Its health since its last start, when it has a check.
    pub health: Option<&'a crate::health::Health>,
}

/// One entry of `GET /containers/json`.
pub fn list_entry(f: &Facts) -> Value {
    let running = f.state.status == Status::Running;
    let ports: Vec<Value> = if running {
        f.state
            .ports
            .iter()
            .map(|p| {
                json!({
                    "IP": p.host_ip.to_string(),
                    "PrivatePort": p.container_port,
                    "PublicPort": p.host_port,
                    "Type": proto(p),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    json!({
        "Id": f.spec.id,
        "Names": [format!("/{}", f.spec.name)],
        "Image": f.image,
        "ImageID": f.spec.image.clone().unwrap_or_default(),
        "Command": f.spec.command.join(" "),
        "Created": f.state.created,
        "Ports": ports,
        "Labels": f.spec.labels,
        "State": state_label(f.state),
        "Status": format!(
            "{}{}",
            status_text(f.state, f.now),
            f.health
                .filter(|_| f.state.status == Status::Running && !f.state.paused)
                .map_or("", |h| h.status.ps_suffix())
        ),
        "HostConfig": {"NetworkMode": network_mode(f)},
        "NetworkSettings": {"Networks": networks(f)},
        "Mounts": mounts(f),
    })
}

/// One network of a container, for inspect and the list.
pub struct Attached<'a> {
    /// `host`, `none`, or a bridge network's name.
    pub name: &'a str,
    /// The bridge network: None for `host` and `none`, or when it is gone.
    pub network: Option<crate::network::Network>,
    /// The container's address there, while it runs.
    pub addresses: Option<crate::network::Addresses>,
    /// Its further names there.
    pub aliases: &'a [String],
}

/// `State.Health`.
fn health_json(h: &crate::health::Health) -> Value {
    let log: Vec<Value> = h
        .log
        .iter()
        .map(|p| {
            json!({
                "Start": p.start,
                "End": p.end,
                "ExitCode": p.exit_code,
                "Output": p.output,
            })
        })
        .collect();
    json!({
        "Status": h.status.name(),
        "FailingStreak": h.failing_streak,
        "Log": log,
    })
}

/// `Config.Healthcheck`: durations in nanoseconds.
fn healthcheck_json(c: &crate::health::HealthConfig) -> Value {
    json!({
        "Test": c.test,
        "Interval": c.interval_ms * 1_000_000,
        "Timeout": c.timeout_ms * 1_000_000,
        "Retries": c.retries,
        "StartPeriod": c.start_period_ms * 1_000_000,
        "StartInterval": c.start_interval_ms * 1_000_000,
    })
}

/// `GET /containers/{id}/json`.
pub fn inspect(f: &Facts) -> Value {
    let st = f.state;
    let running = st.status == Status::Running;
    let path = |name: &str| f.dir.join(name).display().to_string();
    let (gateway, prefix) = match f.networks.first().and_then(|a| a.network.as_ref()) {
        Some(n) => (n.gateway.clone(), prefix_len(&n.subnet)),
        None => (String::new(), 0),
    };
    json!({
        "Id": f.spec.id,
        "Created": rfc3339(Some(st.created)),
        "Path": f.spec.command.first().cloned().unwrap_or_default(),
        "Args": f.spec.command.iter().skip(1).collect::<Vec<_>>(),
        "State": {
            "Status": state_label(st),
            "Running": running,
            "Paused": running && st.paused,
            "Restarting": false,
            "OOMKilled": st.oom.is_some(),
            "Dead": false,
            "Pid": if running { st.pid.unwrap_or(0) } else { 0 },
            "ExitCode": st.exit_code.unwrap_or(0),
            "Error": "",
            "StartedAt": rfc3339(st.started),
            "FinishedAt": rfc3339(st.finished),
            "Health": f.health.map(health_json),
        },
        "Image": f.spec.image.clone().unwrap_or_default(),
        "ResolvConfPath": path("resolv.conf"),
        "HostnamePath": path("hostname"),
        "HostsPath": path("hosts"),
        "LogPath": path("log.json"),
        "Name": format!("/{}", f.spec.name),
        "RestartCount": f.restart_count,
        "Driver": "overlay",
        "Platform": "linux",
        "MountLabel": "",
        "ProcessLabel": "",
        "AppArmorProfile": "",
        "ExecIDs": null,
        "HostConfig": host_config(f),
        "Config": {
            "Hostname": f.spec.hostname,
            "Domainname": "",
            "User": f.spec.user.clone().unwrap_or_default(),
            "AttachStdin": false,
            "AttachStdout": false,
            "AttachStderr": false,
            "ExposedPorts": exposed(f),
            "Tty": f.spec.tty,
            "OpenStdin": f.spec.interactive,
            "StdinOnce": false,
            "StopTimeout": f.spec.stop_timeout,
            "Env": f.spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            "Cmd": f.spec.command,
            "Image": f.image,
            "Volumes": null,
            "WorkingDir": f.spec.workdir.clone().unwrap_or_default(),
            "Entrypoint": null,
            "OnBuild": null,
            "Labels": f.spec.labels,
            "Healthcheck": f.spec.health.as_ref().map(healthcheck_json),
        },
        "NetworkSettings": {
            "Bridge": "",
            "SandboxID": "",
            "HairpinMode": false,
            "Ports": if running { bound_ports(&f.state.ports, false) } else { json!({}) },
            "SandboxKey": "",
            "IPAddress": st.ip.clone().unwrap_or_default(),
            "IPPrefixLen": if st.ip.is_some() { prefix } else { 0 },
            "Gateway": if st.ip.is_some() { gateway } else { String::new() },
            "MacAddress": "",
            "Networks": networks(f),
        },
        "Mounts": mounts(f),
    })
}

fn proto(p: &PortMapping) -> &'static str {
    match p.proto {
        Proto::Tcp => "tcp",
        Proto::Udp => "udp",
    }
}

/// `{"80/tcp": [{"HostIp": …, "HostPort": …}]}`. As configured (`configured`: an
/// unspecified address or port 0 shows as ""), or as bound while running.
fn bound_ports(ports: &[PortMapping], configured: bool) -> Value {
    let mut out = serde_json::Map::new();
    for p in ports {
        let key = format!("{}/{}", p.container_port, proto(p));
        let (ip, port) = if configured {
            let ip = if p.host_ip.is_unspecified() {
                String::new()
            } else {
                p.host_ip.to_string()
            };
            let port = if p.host_port == 0 {
                String::new()
            } else {
                p.host_port.to_string()
            };
            (ip, port)
        } else {
            (p.host_ip.to_string(), p.host_port.to_string())
        };
        let entry = out.entry(key).or_insert_with(|| json!([]));
        if let Some(list) = entry.as_array_mut() {
            list.push(json!({"HostIp": ip, "HostPort": port}));
        }
    }
    Value::Object(out)
}

fn exposed(f: &Facts) -> Value {
    let ports: serde_json::Map<String, Value> = f
        .spec
        .publish
        .iter()
        .map(|p| (format!("{}/{}", p.container_port, proto(p)), json!({})))
        .collect();
    if ports.is_empty() {
        Value::Null
    } else {
        Value::Object(ports)
    }
}

/// Docker's name for the network mode: `bridge` for the default network.
fn network_mode(f: &Facts) -> String {
    match &f.spec.network {
        NetworkMode::Bridge { network } => network.clone(),
        NetworkMode::Host => "host".to_string(),
        NetworkMode::None => "none".to_string(),
    }
}

fn prefix_len(subnet: &str) -> u32 {
    subnet
        .rsplit_once('/')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(0)
}

/// `NetworkSettings.Networks`: an endpoint for each of the container's networks.
fn networks(f: &Facts) -> Value {
    let mut out = serde_json::Map::new();
    for a in f.networks {
        let fixed = f.spec.fixed_addresses.get(a.name);
        let mut endpoint = json!({
            "IPAMConfig": fixed.map(|fixed| json!({
                "IPv4Address": fixed.ipv4.map(|a| a.to_string()).unwrap_or_default(),
                "IPv6Address": fixed.ipv6.map(|a| a.to_string()).unwrap_or_default(),
            })),
            "Links": null,
            "Aliases": null,
            "MacAddress": "",
            "NetworkID": "",
            "EndpointID": "",
            "Gateway": "",
            "IPAddress": "",
            "IPPrefixLen": 0,
            "IPv6Gateway": "",
            "GlobalIPv6Address": "",
            "GlobalIPv6PrefixLen": 0,
            "DriverOpts": null,
            "DNSNames": null,
        });
        if let Some(n) = &a.network {
            // The full ID, as network inspect shows it: Compose compares the two.
            endpoint["NetworkID"] = json!(n.docker_id());
            if a.name != crate::network::DEFAULT {
                endpoint["Aliases"] = json!(a.aliases);
                let mut names: Vec<&str> = vec![&f.spec.name];
                names.extend(a.aliases.iter().map(String::as_str));
                names.push(crate::id::short_id(&f.spec.id));
                endpoint["DNSNames"] = json!(names);
            }
            if let Some(addresses) = a.addresses {
                endpoint["IPAddress"] = json!(addresses.ipv4.to_string());
                endpoint["Gateway"] = json!(n.gateway);
                endpoint["IPPrefixLen"] = json!(prefix_len(&n.subnet));
                if let Some((address, subnet)) = addresses.ipv6.zip(n.ipv6) {
                    endpoint["IPv6Gateway"] = json!(subnet.gateway().to_string());
                    endpoint["GlobalIPv6Address"] = json!(address.to_string());
                    endpoint["GlobalIPv6PrefixLen"] = json!(subnet.prefix);
                }
            }
        }
        out.insert(a.name.to_string(), endpoint);
    }
    Value::Object(out)
}

fn host_config(f: &Facts) -> Value {
    let limits = &f.spec.limits;
    // --cpus, or a quota and period of their own (`update --cpu-quota`).
    let (nano_cpus, cpu_quota) = match limits.cpu_period {
        None => (
            limits
                .cpu_quota
                .map(|q| q * 1_000_000_000 / crate::cgroup::CPU_PERIOD)
                .unwrap_or(0),
            0,
        ),
        Some(_) => (0, limits.cpu_quota.unwrap_or(0)),
    };
    // Docker's memory+swap: twice the memory unless it was given.
    let memory_swap = match (limits.memory, limits.memory_swap) {
        (_, Some(total)) => total,
        (Some(m), None) => (m as i64).saturating_mul(2),
        (None, None) => 0,
    };
    let binds: Vec<String> = f
        .mounts
        .iter()
        .filter(|m| m.kind != "tmpfs")
        .map(|m| {
            let mode = if m.read_only { ":ro" } else { "" };
            format!("{}:{}{mode}", m.source, m.target)
        })
        .collect();
    let mut security_opt = Vec::new();
    if f.spec.security.no_new_privileges {
        security_opt.push("no-new-privileges".to_string());
    }
    if !f.spec.security.seccomp {
        security_opt.push("seccomp=unconfined".to_string());
    }
    json!({
        "Binds": binds,
        "NetworkMode": network_mode(f),
        "PortBindings": bound_ports(&f.spec.publish, true),
        "RestartPolicy": {
            "Name": f.spec.restart.docker().0,
            "MaximumRetryCount": f.spec.restart.docker().1,
        },
        "AutoRemove": f.spec.auto_remove,
        "LogConfig": {"Type": "json-file", "Config": {}},
        "Memory": limits.memory.unwrap_or(0),
        "NanoCpus": nano_cpus,
        "CpuQuota": cpu_quota,
        "CpuPeriod": limits.cpu_period.unwrap_or(0),
        "CpuShares": limits.cpu_shares.unwrap_or(0),
        "CpusetCpus": limits.cpuset_cpus.clone().unwrap_or_default(),
        "CpusetMems": limits.cpuset_mems.clone().unwrap_or_default(),
        "MemorySwap": memory_swap,
        "MemoryReservation": limits.memory_reservation.unwrap_or(0),
        "BlkioWeight": limits.blkio_weight.unwrap_or(0),
        "PidsLimit": limits.pids,
        "CapAdd": f.spec.security.cap_add,
        "Privileged": f.spec.security.privileged,
        "CapDrop": f.spec.security.cap_drop,
        "SecurityOpt": security_opt,
        "ReadonlyRootfs": false,
        "PublishAllPorts": false,
        "Init": null,
        "ExtraHosts": f.spec.extra_hosts.iter().map(|e| e.docker()).collect::<Vec<_>>(),
        "Dns": f.spec.dns.servers,
        "DnsSearch": f.spec.dns.search,
        "DnsOptions": f.spec.dns.options,
    })
}

fn mounts(f: &Facts) -> Vec<Value> {
    f.mounts
        .iter()
        .map(|m| {
            let (name, source, driver) = match m.kind.as_str() {
                "volume" => (
                    m.source.clone(),
                    f.volumes.join(&m.source).join("data").display().to_string(),
                    "local",
                ),
                "tmpfs" => (String::new(), String::new(), ""),
                _ => (String::new(), m.source.clone(), ""),
            };
            json!({
                "Type": m.kind,
                "Name": name,
                "Source": source,
                "Destination": m.target,
                "Driver": driver,
                "Mode": if m.read_only { "ro" } else { "" },
                "RW": !m.read_only,
                "Propagation": "",
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::Network;

    fn port(host_port: u16) -> PortMapping {
        PortMapping {
            host_ip: "0.0.0.0".parse().unwrap(),
            host_port,
            container_port: 80,
            proto: Proto::Tcp,
        }
    }

    #[test]
    fn inspect_and_list_shapes() {
        let mut spec = crate::spec::spec_fixture();
        spec.name = "web".into();
        spec.command = vec!["sh".into(), "-c".into(), "echo hi".into()];
        spec.env = vec![("A".into(), "1".into())];
        spec.labels.insert("app".into(), "x".into());
        spec.network = NetworkMode::Bridge {
            network: "bridge".into(),
        };
        spec.publish = vec![port(0)];
        spec.extra_hosts = vec![
            crate::netfiles::ExtraHost::parse("db:10.1.2.3").unwrap(),
            crate::netfiles::ExtraHost::parse("gw=host-gateway").unwrap(),
            crate::netfiles::ExtraHost::parse("v6=[fd00::1]").unwrap(),
        ];
        spec.dns = crate::netfiles::DnsSettings {
            servers: vec!["9.9.9.9".parse().unwrap()],
            search: vec!["lan".into()],
            options: vec!["ndots:2".into()],
        };
        let state = State {
            status: Status::Running,
            pid: Some(42),
            shim_pid: Some(41),
            exit_code: None,
            created: 1000,
            started: Some(1001),
            finished: None,
            cgroup: None,
            oom: None,
            ip: Some("10.78.0.2".into()),
            network_inode: None,
            ports: vec![port(32768)],
            paused: false,
        };
        let net = Network {
            name: "bridge".into(),
            id: "abcdef123456".into(),
            subnet: "10.78.0.0/24".into(),
            gateway: "10.78.0.1".into(),
            ipv6: None,
            bridge: "op0".into(),
            created: 1,
            labels: Default::default(),
        };
        let attached = [Attached {
            name: "bridge",
            network: Some(net.clone()),
            addresses: Some(crate::network::Addresses {
                ipv4: "10.78.0.2".parse().unwrap(),
                ipv6: None,
            }),
            aliases: &[],
        }];
        let mounts = vec![
            MountInfo {
                kind: "volume".into(),
                source: "vol".into(),
                target: "/v".into(),
                read_only: false,
            },
            MountInfo {
                kind: "bind".into(),
                source: "/h".into(),
                target: "/h".into(),
                read_only: true,
            },
        ];
        let facts = Facts {
            spec: &spec,
            state: &state,
            image: "alpine",
            mounts: &mounts,
            networks: &attached,
            dir: Path::new("/d/c"),
            volumes: Path::new("/d/volumes"),
            now: 1061,
            restart_count: 0,
            health: None,
        };
        let i = inspect(&facts);
        for key in [
            "Id",
            "Created",
            "Path",
            "Args",
            "State",
            "Image",
            "Name",
            "RestartCount",
            "Driver",
            "Platform",
            "HostConfig",
            "Config",
            "NetworkSettings",
            "Mounts",
            "LogPath",
        ] {
            assert!(i.get(key).is_some(), "inspect has no {key}: {i}");
        }
        assert_eq!(i["Name"], "/web");
        assert_eq!(i["Path"], "sh");
        assert_eq!(i["Args"], json!(["-c", "echo hi"]));
        assert_eq!(i["State"]["Status"], "running");
        assert_eq!(i["State"]["Running"], true);
        assert_eq!(i["State"]["Pid"], 42);
        assert_eq!(i["Config"]["Env"], json!(["A=1"]));
        assert_eq!(i["Config"]["Labels"], json!({"app": "x"}));
        assert_eq!(i["Config"]["Image"], "alpine");
        assert_eq!(
            i["NetworkSettings"]["Ports"],
            json!({"80/tcp": [{"HostIp": "0.0.0.0", "HostPort": "32768"}]})
        );
        let bridge = &i["NetworkSettings"]["Networks"]["bridge"];
        assert_eq!(bridge["IPAddress"], "10.78.0.2");
        assert_eq!(bridge["IPPrefixLen"], 24);
        assert_eq!(bridge["Gateway"], "10.78.0.1");
        assert_eq!(bridge["NetworkID"], net.docker_id());
        // The default network gives no names.
        assert_eq!(bridge["Aliases"], serde_json::Value::Null);
        assert_eq!(i["HostConfig"]["NetworkMode"], "bridge");
        assert_eq!(
            i["HostConfig"]["ExtraHosts"],
            json!(["db:10.1.2.3", "gw:host-gateway", "v6:fd00::1"])
        );
        assert_eq!(i["HostConfig"]["Dns"], json!(["9.9.9.9"]));
        assert_eq!(i["HostConfig"]["DnsSearch"], json!(["lan"]));
        assert_eq!(i["HostConfig"]["DnsOptions"], json!(["ndots:2"]));
        assert_eq!(
            i["HostConfig"]["PortBindings"],
            json!({"80/tcp": [{"HostIp": "", "HostPort": ""}]})
        );
        assert_eq!(i["Mounts"][0]["Type"], "volume");
        assert_eq!(i["Mounts"][0]["Name"], "vol");
        assert_eq!(i["Mounts"][0]["Source"], "/d/volumes/vol/data");
        assert_eq!(i["Mounts"][1]["RW"], false);
        assert_eq!(i["LogPath"], "/d/c/log.json");

        let l = list_entry(&facts);
        assert_eq!(l["Id"], spec.id);
        assert_eq!(l["Names"], json!(["/web"]));
        assert_eq!(
            l["Ports"],
            json!([{"IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 32768, "Type": "tcp"}])
        );
        assert_eq!(l["State"], "running");
        assert_eq!(l["Status"], "Up About a minute");
        assert_eq!(l["Command"], "sh -c echo hi");
        assert_eq!(l["Labels"], json!({"app": "x"}));
        assert_eq!(l["HostConfig"]["NetworkMode"], "bridge");
    }

    #[test]
    fn a_stopped_container_shows_its_exit() {
        let spec = crate::spec::spec_fixture();
        let state = State {
            status: Status::Exited,
            pid: None,
            shim_pid: None,
            exit_code: Some(3),
            created: 1000,
            started: Some(1001),
            finished: Some(1002),
            cgroup: None,
            oom: None,
            ip: None,
            network_inode: None,
            ports: vec![],
            paused: false,
        };
        let facts = Facts {
            spec: &spec,
            state: &state,
            image: "/r",
            mounts: &[],
            networks: &[],
            dir: Path::new("/d/c"),
            volumes: Path::new("/d/volumes"),
            now: 1012,
            restart_count: 0,
            health: None,
        };
        let i = inspect(&facts);
        assert_eq!(i["State"]["Status"], "exited");
        assert_eq!(i["State"]["ExitCode"], 3);
        assert_eq!(i["State"]["Running"], false);
        assert_eq!(i["HostConfig"]["NetworkMode"], "host");
        let l = list_entry(&facts);
        assert_eq!(l["Status"], "Exited (3) 10 seconds ago");
        assert_eq!(l["Ports"], json!([]));
    }
}
