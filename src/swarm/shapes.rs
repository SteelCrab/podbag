//! The swarm's objects as Docker Engine API 1.47 shows them: the swarm, its node,
//! services, tasks, secrets and configs, and the Swarm part of `/info`.

use super::store::{Cluster, FileObject, Kind, ServiceRecord};
use serde_json::{Value, json};

/// Facts about this host for the node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Host {
    pub hostname: String,
    /// Docker's name for the CPU: `x86_64`, `aarch64`.
    pub architecture: String,
    pub ncpu: u64,
    pub memory: u64,
    pub engine_version: String,
}

/// The address the node shows: the advertised address without its port.
fn node_addr(c: &Cluster) -> String {
    let addr = &c.advertise_addr;
    match addr.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host.to_string(),
        _ => addr.clone(),
    }
}

/// The swarm without its join tokens (`/info` shows it so).
fn cluster_info(c: &Cluster) -> Value {
    json!({
        "ID": c.id,
        "Version": {"Index": c.version},
        "CreatedAt": c.created,
        "UpdatedAt": c.updated,
        "Spec": c.spec,
        "TLSInfo": {"TrustRoot": "", "CertIssuerSubject": "", "CertIssuerPublicKey": ""},
        "RootRotationInProgress": false,
        "DefaultAddrPool": c.default_addr_pool,
        "SubnetSize": c.subnet_size,
        "DataPathPort": c.data_path_port,
    })
}

/// `GET /swarm`.
pub fn swarm_json(c: &Cluster) -> Value {
    let mut v = cluster_info(c);
    v["JoinTokens"] = json!({"Worker": c.tokens.worker, "Manager": c.tokens.manager});
    v
}

/// The Swarm part of `/info`.
pub fn info_swarm(c: Option<&Cluster>) -> Value {
    let Some(c) = c else {
        return json!({
            "NodeID": "",
            "NodeAddr": "",
            "LocalNodeState": "inactive",
            "ControlAvailable": false,
            "Error": "",
            "RemoteManagers": null,
        });
    };
    let addr = node_addr(c);
    json!({
        "NodeID": c.node_id,
        "NodeAddr": addr,
        "LocalNodeState": "active",
        "ControlAvailable": true,
        "Error": "",
        "RemoteManagers": [{"NodeID": c.node_id, "Addr": c.advertise_addr}],
        "Nodes": 1,
        "Managers": 1,
        "Cluster": cluster_info(c),
    })
}

/// `GET /nodes/{id}`: the one node, a ready manager and leader.
pub fn node_json(c: &Cluster, host: &Host) -> Value {
    json!({
        "ID": c.node_id,
        "Version": {"Index": c.node.version},
        "CreatedAt": c.node.created,
        "UpdatedAt": c.node.updated,
        "Spec": c.node.spec,
        "Description": {
            "Hostname": host.hostname,
            "Platform": {"Architecture": host.architecture, "OS": "linux"},
            "Resources": {"NanoCPUs": host.ncpu * 1_000_000_000, "MemoryBytes": host.memory},
            "Engine": {
                "EngineVersion": host.engine_version,
                "Plugins": [
                    {"Type": "Network", "Name": "bridge"},
                    {"Type": "Network", "Name": "overlay"},
                    {"Type": "Volume", "Name": "local"},
                ],
            },
            "TLSInfo": {"TrustRoot": "", "CertIssuerSubject": "", "CertIssuerPublicKey": ""},
        },
        "Status": {"State": "ready", "Addr": node_addr(c)},
        "ManagerStatus": {"Leader": true, "Reachability": "reachable", "Addr": c.advertise_addr},
    })
}

pub fn worker_json(worker: &super::remote::Worker, ready: bool) -> Value {
    let host = worker.host.as_ref();
    json!({
        "ID": worker.id,
        "Version": {"Index": worker.version},
        "CreatedAt": worker.created,
        "UpdatedAt": worker.updated.as_ref().unwrap_or(&worker.created),
        "Spec": {
            "Name": worker.name,
            "Role": "worker",
            "Availability": if worker.drained { "drain" } else { "active" },
            "Labels": {}
        },
        "Description": {
            "Hostname": worker.name,
            "Platform": {"Architecture": host.map(|h| h.architecture.as_str()).unwrap_or_default(), "OS": "linux"},
            "Resources": {"NanoCPUs": host.map_or(0, |h| h.ncpu * 1_000_000_000), "MemoryBytes": host.map_or(0, |h| h.memory)},
            "Engine": {"EngineVersion": host.map(|h| h.engine_version.as_str()).unwrap_or_default()}
        },
        "Status": {"State": if ready { "ready" } else { "down" }, "Addr": worker.destination}
    })
}

/// Task counts for `GET /services?status=true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub running: u64,
    pub desired: u64,
    pub completed: u64,
}

/// `GET /services/{id}`. `vips` are (network ID, `address/prefix`).
pub fn service_json(s: &ServiceRecord, vips: &[(String, String)], counts: Option<Counts>) -> Value {
    let ports: Vec<Value> = s
        .ports
        .iter()
        .map(|p| {
            json!({
                "Protocol": p.protocol,
                "TargetPort": p.target,
                "PublishedPort": p.published,
                "PublishMode": p.mode,
            })
        })
        .collect();
    let vips: Vec<Value> = vips
        .iter()
        .map(|(network, addr)| json!({"NetworkID": network, "Addr": addr}))
        .collect();
    let mut v = json!({
        "ID": s.id,
        "Version": {"Index": s.version},
        "CreatedAt": s.created,
        "UpdatedAt": s.updated,
        "Spec": s.spec,
        "Endpoint": {
            "Spec": s.spec["EndpointSpec"],
            "Ports": ports,
            "VirtualIPs": vips,
        },
    });
    if let Some(previous) = &s.previous_spec {
        v["PreviousSpec"] = previous.clone();
    }
    if let Some(status) = &s.update_status {
        v["UpdateStatus"] = status.clone();
    }
    if let Some(c) = counts {
        v["ServiceStatus"] = json!({
            "RunningTasks": c.running,
            "DesiredTasks": c.desired,
            "CompletedTasks": c.completed,
        });
    }
    v
}

/// What a task is, for its JSON.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TaskFacts {
    pub id: String,
    pub version: u64,
    pub created: String,
    pub updated: String,
    pub name: String,
    pub service_id: String,
    pub slot: u64,
    pub node_id: String,
    pub spec: Value,
    pub desired_state: String,
    pub state: String,
    pub message: String,
    pub err: String,
    pub container_id: String,
    pub pid: u32,
    pub exit_code: i32,
    /// (network name, network ID, `address/prefix`).
    pub networks: Vec<(String, String, String)>,
}

/// `GET /tasks/{id}`.
pub fn task_json(t: &TaskFacts) -> Value {
    let attachments: Vec<Value> = t
        .networks
        .iter()
        .map(|(name, id, addr)| {
            json!({
                "Network": {
                    "ID": id,
                    "Spec": {"Name": name, "DriverConfiguration": {"Name": "overlay"}, "Scope": "swarm"},
                },
                "Addresses": [addr],
            })
        })
        .collect();
    let mut status = json!({
        "Timestamp": t.updated,
        "State": t.state,
        "Message": t.message,
        "ContainerStatus": {"ContainerID": t.container_id, "PID": t.pid, "ExitCode": t.exit_code},
        "PortStatus": {},
    });
    if !t.err.is_empty() {
        status["Err"] = Value::from(t.err.clone());
    }
    json!({
        "ID": t.id,
        "Version": {"Index": t.version},
        "CreatedAt": t.created,
        "UpdatedAt": t.updated,
        "Name": t.name,
        "Labels": {},
        "Spec": t.spec,
        "ServiceID": t.service_id,
        "Slot": t.slot,
        "NodeID": t.node_id,
        "Status": status,
        "DesiredState": t.desired_state,
        "NetworksAttachments": attachments,
    })
}

/// `GET /secrets/{id}` or `GET /configs/{id}`. Only a config shows its data.
pub fn object_json(kind: Kind, o: &FileObject, data: Option<&[u8]>) -> Value {
    let mut spec = o.spec.clone();
    if let Some(map) = spec.as_object_mut() {
        map.remove("Data");
        if let (Kind::Config, Some(data)) = (kind, data) {
            map.insert("Data".to_string(), Value::from(base64_encode(data)));
        }
    }
    json!({
        "ID": o.id,
        "Version": {"Index": o.version},
        "CreatedAt": o.created,
        "UpdatedAt": o.updated,
        "Spec": spec,
    })
}

/// Standard base64 with padding.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let b = [
            group[0],
            *group.get(1).unwrap_or(&0),
            *group.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= group.len() {
                out.push(ALPHABET[(n >> shift & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64; padding and line breaks are optional. None for anything else.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut sextets = Vec::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'\n' | b'\r' => {}
            b'=' => break,
            _ => sextets.push(ALPHABET.iter().position(|a| *a == b)? as u32),
        }
    }
    if sextets.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(sextets.len() * 3 / 4);
    for group in sextets.chunks(4) {
        let mut n = 0u32;
        for (i, v) in group.iter().enumerate() {
            n |= v << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..group.len()]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::store::{NodeRecord, PortRecord, Tokens};
    use std::collections::BTreeMap;

    fn cluster() -> Cluster {
        Cluster {
            id: "c".repeat(25),
            node_id: "n".repeat(25),
            created: "2026-11-20T10:00:00.000000000Z".into(),
            updated: "2026-11-20T10:00:01.000000000Z".into(),
            version: 7,
            index: 12,
            spec: json!({"Name": "default", "Labels": {}}),
            node: NodeRecord {
                version: 9,
                created: "2026-11-20T10:00:00.000000000Z".into(),
                updated: "2026-11-20T10:00:00.000000000Z".into(),
                spec: json!({"Labels": {"zone": "a"}, "Role": "manager", "Availability": "active"}),
            },
            tokens: Tokens {
                worker: "w".into(),
                manager: "m".into(),
            },
            listen_addr: "0.0.0.0:2377".into(),
            advertise_addr: "192.0.2.5:2377".into(),
            default_addr_pool: vec!["10.0.0.0/8".into()],
            subnet_size: 24,
            data_path_port: 4789,
            own_ingress: true,
        }
    }

    fn host() -> Host {
        Host {
            hostname: "box".into(),
            architecture: "aarch64".into(),
            ncpu: 4,
            memory: 1 << 32,
            engine_version: "0.1.0".into(),
        }
    }

    #[test]
    fn swarm_and_info_shapes() {
        let c = cluster();
        let s = swarm_json(&c);
        assert_eq!(s["ID"], c.id);
        assert_eq!(s["Version"]["Index"], 7);
        assert_eq!(s["Spec"]["Name"], "default");
        assert_eq!(s["JoinTokens"]["Worker"], "w");
        assert_eq!(s["JoinTokens"]["Manager"], "m");
        assert_eq!(s["DefaultAddrPool"], json!(["10.0.0.0/8"]));
        assert_eq!(s["SubnetSize"], 24);
        assert_eq!(s["DataPathPort"], 4789);
        let off = info_swarm(None);
        assert_eq!(off["LocalNodeState"], "inactive");
        assert_eq!(off["ControlAvailable"], false);
        assert_eq!(off["NodeID"], "");
        let on = info_swarm(Some(&c));
        assert_eq!(on["LocalNodeState"], "active");
        assert_eq!(on["ControlAvailable"], true);
        assert_eq!(on["NodeID"], c.node_id);
        assert_eq!(on["NodeAddr"], "192.0.2.5");
        assert_eq!(on["Nodes"], 1);
        assert_eq!(on["Managers"], 1);
        assert_eq!(on["Cluster"]["ID"], c.id);
        assert_eq!(on["RemoteManagers"][0]["NodeID"], c.node_id);
        assert!(
            on["Cluster"].get("JoinTokens").is_none(),
            "only GET /swarm has them"
        );
    }

    #[test]
    fn the_node_is_a_ready_leader() {
        let c = cluster();
        let n = node_json(&c, &host());
        assert_eq!(n["ID"], c.node_id);
        assert_eq!(n["Version"]["Index"], 9);
        assert_eq!(n["Spec"]["Role"], "manager");
        assert_eq!(n["Spec"]["Labels"]["zone"], "a");
        assert_eq!(n["Description"]["Hostname"], "box");
        assert_eq!(n["Description"]["Platform"]["OS"], "linux");
        assert_eq!(n["Description"]["Platform"]["Architecture"], "aarch64");
        assert_eq!(n["Description"]["Resources"]["NanoCPUs"], 4_000_000_000u64);
        assert_eq!(n["Description"]["Engine"]["EngineVersion"], "0.1.0");
        assert_eq!(n["Status"]["State"], "ready");
        assert_eq!(n["Status"]["Addr"], "192.0.2.5");
        assert_eq!(n["ManagerStatus"]["Leader"], true);
        assert_eq!(n["ManagerStatus"]["Reachability"], "reachable");
        assert_eq!(n["ManagerStatus"]["Addr"], "192.0.2.5:2377");
    }

    fn service() -> ServiceRecord {
        ServiceRecord {
            id: "s".repeat(25),
            version: 14,
            created: "2026-11-20T10:00:00.000000000Z".into(),
            updated: "2026-11-20T10:00:02.000000000Z".into(),
            spec: crate::swarm::plan::with_defaults(&json!({
                "Name": "web",
                "Labels": {"com.docker.stack.namespace": "app"},
                "TaskTemplate": {
                    "ContainerSpec": {"Image": "nginx:1.27", "Args": ["-g", "daemon off;"],
                                      "Env": ["A=1"], "Secrets": [{"SecretName": "pw", "SecretID": "x",
                                      "File": {"Name": "pw", "UID": "0", "GID": "0", "Mode": 292}}]},
                    "RestartPolicy": {"Condition": "on-failure", "MaxAttempts": 2},
                    "Networks": [{"Target": "app_default", "Aliases": ["web"]}],
                    "ForceUpdate": 1
                },
                "Mode": {"Replicated": {"Replicas": 3}},
                "UpdateConfig": {"Parallelism": 1, "Delay": 1_000_000_000u64, "Order": "start-first"},
                "EndpointSpec": {"Ports": [{"Protocol": "tcp", "TargetPort": 80, "PublishedPort": 8080}]}
            })),
            previous_spec: Some(json!({"Name": "web", "Mode": {"Replicated": {"Replicas": 1}}})),
            update_status: Some(json!({"State": "completed", "Message": "update completed"})),
            ports: vec![PortRecord {
                protocol: "tcp".into(),
                target: 80,
                published: 8080,
                mode: "ingress".into(),
            }],
            vips: BTreeMap::new(),
            worker_slots: Default::default(),
            worker_releases: Default::default(),
            removing: false,
        }
    }

    #[test]
    fn a_service_spec_round_trips() {
        let s = service();
        let vips = [("i".repeat(64), "10.78.1.9/24".to_string())];
        let j = service_json(&s, &vips, None);
        assert_eq!(j["ID"], s.id);
        assert_eq!(j["Version"]["Index"], 14);
        assert_eq!(j["CreatedAt"], s.created);
        assert_eq!(j["UpdatedAt"], s.updated);
        assert_eq!(j["Spec"], s.spec, "the spec comes back as it was stored");
        assert_eq!(j["PreviousSpec"], s.previous_spec.clone().unwrap());
        assert_eq!(j["UpdateStatus"]["State"], "completed");
        assert_eq!(j["Endpoint"]["Spec"], s.spec["EndpointSpec"]);
        assert_eq!(
            j["Endpoint"]["Ports"],
            json!([{"Protocol": "tcp", "TargetPort": 80, "PublishedPort": 8080, "PublishMode": "ingress"}])
        );
        assert_eq!(
            j["Endpoint"]["VirtualIPs"],
            json!([{"NetworkID": "i".repeat(64), "Addr": "10.78.1.9/24"}])
        );
        assert!(j.get("ServiceStatus").is_none());
        // And through JSON text: what a client sends back on update is what it got.
        let text = serde_json::to_string(&j["Spec"]).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(crate::swarm::plan::with_defaults(&back), s.spec);
        let counted = service_json(
            &s,
            &[],
            Some(Counts {
                running: 2,
                desired: 3,
                completed: 0,
            }),
        );
        assert_eq!(
            counted["ServiceStatus"],
            json!({"RunningTasks": 2, "DesiredTasks": 3, "CompletedTasks": 0})
        );
        let mut fresh = service();
        fresh.previous_spec = None;
        fresh.update_status = None;
        let j = service_json(&fresh, &[], None);
        assert!(j.get("PreviousSpec").is_none());
        assert!(j.get("UpdateStatus").is_none());
        assert_eq!(j["Endpoint"]["VirtualIPs"], json!([]));
    }

    #[test]
    fn task_shape() {
        let t = TaskFacts {
            id: "t".repeat(25),
            version: 20,
            created: "2026-11-20T10:00:00.000000000Z".into(),
            updated: "2026-11-20T10:00:03.000000000Z".into(),
            name: format!("web.2.{}", "t".repeat(25)),
            service_id: "s".repeat(25),
            slot: 2,
            node_id: "n".repeat(25),
            spec: json!({"ContainerSpec": {"Image": "nginx:1.27"}}),
            desired_state: "running".into(),
            state: "failed".into(),
            message: "started".into(),
            err: "task: non-zero exit (1)".into(),
            container_id: "c".repeat(64),
            pid: 0,
            exit_code: 1,
            networks: vec![("app_default".into(), "i".repeat(64), "10.78.1.3/24".into())],
        };
        let j = task_json(&t);
        assert_eq!(j["ID"], t.id);
        assert_eq!(j["Version"]["Index"], 20);
        assert_eq!(j["Name"], t.name);
        assert_eq!(j["ServiceID"], t.service_id);
        assert_eq!(j["Slot"], 2);
        assert_eq!(j["NodeID"], t.node_id);
        assert_eq!(j["Spec"]["ContainerSpec"]["Image"], "nginx:1.27");
        assert_eq!(j["DesiredState"], "running");
        assert_eq!(j["Status"]["State"], "failed");
        assert_eq!(j["Status"]["Message"], "started");
        assert_eq!(j["Status"]["Err"], "task: non-zero exit (1)");
        assert_eq!(j["Status"]["Timestamp"], t.updated);
        assert_eq!(
            j["Status"]["ContainerStatus"]["ContainerID"],
            t.container_id
        );
        assert_eq!(j["Status"]["ContainerStatus"]["ExitCode"], 1);
        assert_eq!(j["Status"]["PortStatus"], json!({}));
        assert_eq!(j["NetworksAttachments"][0]["Network"]["ID"], "i".repeat(64));
        assert_eq!(
            j["NetworksAttachments"][0]["Network"]["Spec"]["Name"],
            "app_default"
        );
        assert_eq!(
            j["NetworksAttachments"][0]["Addresses"],
            json!(["10.78.1.3/24"])
        );
        let mut ok = t.clone();
        ok.err.clear();
        assert!(task_json(&ok)["Status"].get("Err").is_none());
    }

    #[test]
    fn secrets_hide_their_data_and_configs_show_it() {
        let o = FileObject {
            id: "o".repeat(25),
            version: 4,
            created: "2026-11-20T10:00:00.000000000Z".into(),
            updated: "2026-11-20T10:00:00.000000000Z".into(),
            spec: json!({"Name": "site", "Labels": {"a": "b"}}),
        };
        let mut data = [0u8; 16];
        crate::swarm::store::random(&mut data).unwrap();
        let secret = object_json(Kind::Secret, &o, Some(&data));
        assert_eq!(secret["ID"], o.id);
        assert_eq!(secret["Version"]["Index"], 4);
        assert_eq!(secret["Spec"]["Name"], "site");
        assert_eq!(secret["Spec"]["Labels"]["a"], "b");
        assert!(secret["Spec"].get("Data").is_none(), "{secret}");
        let config = object_json(Kind::Config, &o, Some(&data));
        assert_eq!(config["Spec"]["Data"], base64_encode(&data));
    }

    #[test]
    fn base64_both_ways() {
        for (raw, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (&[0xff, 0xfe, 0x00][..], "//4A"),
        ] {
            assert_eq!(base64_encode(raw), text);
            assert_eq!(base64_decode(text).unwrap(), raw);
        }
        assert_eq!(base64_decode("Zm9vYg").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9v\nYg==\n").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9v!"), None);
        assert_eq!(base64_decode("Z"), None);
        let mut data = [0u8; 300];
        crate::swarm::store::random(&mut data).unwrap();
        assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
    }
}
