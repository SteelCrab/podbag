//! A task's container as a Docker create body: the service's `ContainerSpec`,
//! resources, restart policy and networks in the shape `POST /containers/create` takes.
//! The same mapping as for `docker run` then checks it and builds the container.

use super::plan::{self, TemplateFacts};
use serde_json::{Map, Value, json};

/// A network a task joins: its podbag name, and the task's names there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attach {
    pub network: String,
    pub aliases: Vec<String>,
}

/// The labels Docker puts on a task's container.
pub fn task_labels(facts: &TemplateFacts) -> Vec<(&'static str, String)> {
    vec![
        ("com.docker.swarm.node.id", facts.node_id.to_string()),
        ("com.docker.swarm.service.id", facts.service_id.to_string()),
        (
            "com.docker.swarm.service.name",
            facts.service_name.to_string(),
        ),
        ("com.docker.swarm.task", String::new()),
        ("com.docker.swarm.task.id", facts.task_id.to_string()),
        ("com.docker.swarm.task.name", facts.task_name.to_string()),
    ]
}

/// Copy `from[key]` to `to[as_key]` when it is set.
fn copy(to: &mut Map<String, Value>, as_key: &str, from: &Value, key: &str) {
    if let Some(v) = from.get(key).filter(|v| !v.is_null()) {
        to.insert(as_key.to_string(), v.clone());
    }
}

/// The create body of a task's container. `template` is the service's TaskTemplate.
pub fn create_body(template: &Value, facts: &TemplateFacts, networks: &[Attach]) -> Value {
    let spec = &template["ContainerSpec"];
    let expand = |v: &Value| v.as_str().map(|s| plan::expand(s, facts));
    let mut body = Map::new();
    copy(&mut body, "Image", spec, "Image");
    // A service's Command replaces the image's entrypoint, its Args the image's command.
    copy(&mut body, "Entrypoint", spec, "Command");
    copy(&mut body, "Cmd", spec, "Args");
    if let Some(env) = spec["Env"].as_array() {
        let env: Vec<Value> = env
            .iter()
            .filter_map(|e| expand(e).map(Value::from))
            .collect();
        body.insert("Env".into(), Value::from(env));
    }
    copy(&mut body, "WorkingDir", spec, "Dir");
    copy(&mut body, "User", spec, "User");
    if let Some(hostname) = expand(&spec["Hostname"]).filter(|h| !h.is_empty()) {
        body.insert("Hostname".into(), Value::from(hostname));
    }
    copy(&mut body, "Tty", spec, "TTY");
    copy(&mut body, "OpenStdin", spec, "OpenStdin");
    copy(&mut body, "Healthcheck", spec, "Healthcheck");
    copy(&mut body, "StopSignal", spec, "StopSignal");
    if let Some(grace) = spec["StopGracePeriod"].as_u64() {
        body.insert("StopTimeout".into(), Value::from(grace / 1_000_000_000));
    }
    let mut labels = spec["Labels"].as_object().cloned().unwrap_or_default();
    for (k, v) in task_labels(facts) {
        labels.insert(k.to_string(), Value::from(v));
    }
    body.insert("Labels".into(), Value::Object(labels));

    let mut host = Map::new();
    if let Some(mounts) = spec["Mounts"].as_array() {
        let mounts: Vec<Value> = mounts
            .iter()
            .map(|m| {
                let mut m = m.clone();
                if let Some(source) = expand(&m["Source"]) {
                    m["Source"] = Value::from(source);
                }
                m
            })
            .collect();
        host.insert("Mounts".into(), Value::from(mounts));
    }
    let (name, max) = plan::restart_policy(&template["RestartPolicy"]).docker();
    host.insert(
        "RestartPolicy".into(),
        json!({"Name": name, "MaximumRetryCount": max}),
    );
    let limits = &template["Resources"]["Limits"];
    copy(&mut host, "NanoCpus", limits, "NanoCPUs");
    copy(&mut host, "Memory", limits, "MemoryBytes");
    copy(&mut host, "PidsLimit", limits, "Pids");
    copy(&mut host, "CapAdd", spec, "CapabilityAdd");
    copy(&mut host, "CapDrop", spec, "CapabilityDrop");
    copy(&mut host, "ReadonlyRootfs", spec, "ReadOnly");
    copy(&mut host, "Init", spec, "Init");
    copy(&mut host, "Sysctls", spec, "Sysctls");
    copy(&mut host, "Ulimits", spec, "Ulimits");
    if let Some(hosts) = spec["Hosts"].as_array() {
        // A service writes `IP name…`, a container `name:IP`.
        let extra: Vec<Value> = hosts
            .iter()
            .filter_map(Value::as_str)
            .flat_map(|line| {
                let mut words = line.split_whitespace();
                let ip = words.next().unwrap_or_default().to_string();
                words
                    .map(move |name| Value::from(format!("{name}:{ip}")))
                    .collect::<Vec<_>>()
            })
            .collect();
        host.insert("ExtraHosts".into(), Value::from(extra));
    }
    let dns = &spec["DNSConfig"];
    copy(&mut host, "Dns", dns, "Nameservers");
    copy(&mut host, "DnsSearch", dns, "Search");
    copy(&mut host, "DnsOptions", dns, "Options");
    if let Some(driver) = template["LogDriver"]["Name"].as_str() {
        host.insert("LogConfig".into(), json!({"Type": driver}));
    }
    let primary = networks.first().map_or("default", |a| a.network.as_str());
    host.insert("NetworkMode".into(), Value::from(primary));
    body.insert("HostConfig".into(), Value::Object(host));

    let endpoints: Map<String, Value> = networks
        .iter()
        .map(|a| (a.network.clone(), json!({"Aliases": a.aliases})))
        .collect();
    body.insert(
        "NetworkingConfig".into(),
        json!({"EndpointsConfig": endpoints}),
    );
    Value::Object(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> TemplateFacts<'static> {
        TemplateFacts {
            service_id: "sid",
            service_name: "app_web",
            node_id: "nid",
            node_hostname: "box",
            task_id: "tid",
            task_name: "app_web.2.tid",
            slot: 2,
        }
    }

    #[test]
    fn the_container_spec_becomes_a_create_body() {
        let template = json!({
            "ContainerSpec": {
                "Image": "nginx:1.27",
                "Command": ["/entry"],
                "Args": ["-g", "daemon off;"],
                "Env": ["SLOT={{.Task.Slot}}", "PLAIN=1"],
                "Dir": "/srv",
                "User": "1000:1000",
                "Hostname": "{{.Service.Name}}-{{.Task.Slot}}",
                "TTY": true,
                "OpenStdin": true,
                "Labels": {"com.docker.stack.namespace": "app", "own": "x"},
                "Mounts": [{"Type": "volume", "Source": "data-{{.Task.Slot}}", "Target": "/data"},
                           {"Type": "tmpfs", "Target": "/tmp"}],
                "StopGracePeriod": 3_000_000_000u64,
                "StopSignal": "SIGQUIT",
                "Healthcheck": {"Test": ["CMD", "true"], "Interval": 1_000_000_000u64},
                "CapabilityAdd": ["CAP_NET_ADMIN"],
                "CapabilityDrop": ["CAP_MKNOD"],
                "ReadOnly": false,
                "Init": true,
                "Hosts": ["10.0.0.1 db"],
                "DNSConfig": {"Nameservers": ["1.1.1.1"]}
            },
            "Resources": {"Limits": {"NanoCPUs": 500_000_000u64, "MemoryBytes": 67_108_864u64, "Pids": 50}},
            "RestartPolicy": {"Condition": "on-failure", "MaxAttempts": 4},
            "LogDriver": {"Name": "json-file"}
        });
        let networks = [
            Attach {
                network: "app_default".into(),
                aliases: vec!["web".into(), "tasks.app_web".into()],
            },
            Attach {
                network: "back".into(),
                aliases: vec![],
            },
        ];
        let b = create_body(&template, &facts(), &networks);
        assert_eq!(b["Image"], "nginx:1.27");
        assert_eq!(b["Entrypoint"], json!(["/entry"]));
        assert_eq!(b["Cmd"], json!(["-g", "daemon off;"]));
        assert_eq!(b["Env"], json!(["SLOT=2", "PLAIN=1"]));
        assert_eq!(b["WorkingDir"], "/srv");
        assert_eq!(b["User"], "1000:1000");
        assert_eq!(b["Hostname"], "app_web-2");
        assert_eq!(b["Tty"], true);
        assert_eq!(b["OpenStdin"], true);
        assert_eq!(b["StopSignal"], "SIGQUIT");
        assert_eq!(b["StopTimeout"], 3);
        assert_eq!(b["Healthcheck"]["Test"], json!(["CMD", "true"]));
        let labels = &b["Labels"];
        assert_eq!(labels["own"], "x");
        assert_eq!(labels["com.docker.stack.namespace"], "app");
        assert_eq!(labels["com.docker.swarm.service.id"], "sid");
        assert_eq!(labels["com.docker.swarm.service.name"], "app_web");
        assert_eq!(labels["com.docker.swarm.task.id"], "tid");
        assert_eq!(labels["com.docker.swarm.task.name"], "app_web.2.tid");
        assert_eq!(labels["com.docker.swarm.node.id"], "nid");
        assert_eq!(labels["com.docker.swarm.task"], "");
        let host = &b["HostConfig"];
        assert_eq!(host["Mounts"][0]["Source"], "data-2");
        assert_eq!(host["Mounts"][1]["Type"], "tmpfs");
        assert_eq!(
            host["RestartPolicy"],
            json!({"Name": "on-failure", "MaximumRetryCount": 4})
        );
        assert_eq!(host["NanoCpus"], 500_000_000u64);
        assert_eq!(host["Memory"], 67_108_864u64);
        assert_eq!(host["PidsLimit"], 50);
        assert_eq!(host["CapAdd"], json!(["CAP_NET_ADMIN"]));
        assert_eq!(host["CapDrop"], json!(["CAP_MKNOD"]));
        assert_eq!(host["ReadonlyRootfs"], false);
        assert_eq!(host["Init"], true);
        assert_eq!(host["ExtraHosts"], json!(["db:10.0.0.1"]));
        assert_eq!(host["Dns"], json!(["1.1.1.1"]));
        assert_eq!(host["LogConfig"]["Type"], "json-file");
        assert_eq!(host["NetworkMode"], "app_default");
        assert_eq!(
            b["NetworkingConfig"]["EndpointsConfig"],
            json!({
                "app_default": {"Aliases": ["web", "tasks.app_web"]},
                "back": {"Aliases": []}
            })
        );
    }

    #[test]
    fn a_bare_spec_maps_to_a_bare_body() {
        let b = create_body(
            &json!({"ContainerSpec": {"Image": "alpine"}}),
            &facts(),
            &[],
        );
        assert_eq!(b["Image"], "alpine");
        assert_eq!(b["Cmd"], Value::Null, "the image's command");
        assert_eq!(b["Entrypoint"], Value::Null, "the image's entrypoint");
        assert_eq!(
            b["HostConfig"]["RestartPolicy"],
            json!({"Name": "always", "MaximumRetryCount": 0}),
            "swarm restarts tasks unless told otherwise"
        );
        assert_eq!(b["HostConfig"]["NetworkMode"], "default");
        assert!(b.get("StopTimeout").is_none());
        let none = create_body(
            &json!({"ContainerSpec": {"Image": "alpine"}, "RestartPolicy": {"Condition": "none"}}),
            &facts(),
            &[],
        );
        assert_eq!(none["HostConfig"]["RestartPolicy"]["Name"], "no");
    }

    #[test]
    fn labels_say_whose_task_it_is() {
        let labels = task_labels(&facts());
        assert!(labels.contains(&("com.docker.swarm.task.name", "app_web.2.tid".into())));
        assert_eq!(labels.len(), 6);
    }
}
