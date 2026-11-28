//! `/volumes/…` and `/networks/…`.

use super::docker::rfc3339;
use super::events::publish;
use super::http::Request;
use super::router::{Ctx, Reply, check_filters, labels_match, message, not_supported, reply_error};
use crate::ErrorKind;
use crate::network::{Labels, Network, docker_id_of};
use crate::volume::Volume;
use serde_json::{Value, json};
use std::collections::BTreeMap;

type Filters = BTreeMap<String, Vec<String>>;

fn filters(req: &Request) -> Result<Filters, Reply> {
    req.query
        .filters()
        .map_err(|_| message(400, "invalid filters"))
}

fn check_volume_filters(filters: &Filters) -> Result<(), Reply> {
    check_filters(filters, &["name", "dangling", "driver", "label"], &[])
}

fn check_network_filters(filters: &Filters) -> Result<(), Reply> {
    check_filters(
        filters,
        &["name", "id", "driver", "type", "label"],
        &["dangling", "scope"],
    )
}

fn check_network_prune_filters(filters: &Filters) -> Result<(), Reply> {
    check_filters(filters, &["label", "label!"], &["until"])
}

/// The `Labels` of a create request: an object of strings, or nothing.
fn labels_of(body: &Value) -> Result<Labels, Reply> {
    let bad = || message(400, "Labels must be an object of strings");
    match &body["Labels"] {
        Value::Null => Ok(Labels::new()),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| match v.as_str() {
                Some(v) if !k.is_empty() => Ok((k.clone(), v.to_string())),
                _ => Err(bad()),
            })
            .collect(),
        _ => Err(bad()),
    }
}

/// A volume as Docker shows it.
pub fn volume_json(v: &Volume) -> Value {
    json!({
        "Name": v.name,
        "Driver": v.driver(),
        "Mountpoint": v.mountpoint,
        "CreatedAt": rfc3339(Some(v.created)),
        "Labels": v.labels,
        "Scope": "local",
        "Options": match &v.plugin {
            Some(plugin) => json!(plugin.options),
            None => v.nfs.as_ref().map_or_else(|| json!({}), |opts| json!(opts)),
        },
        "UsageData": null,
    })
}

fn prefix_len(subnet: &str) -> u32 {
    subnet
        .rsplit_once('/')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(0)
}

/// A bridge network as Docker shows it, with its containers: (ID, name, address).
pub fn network_json(
    n: &Network,
    endpoints: &[(String, String, crate::network::Addresses)],
) -> Value {
    let prefix = prefix_len(&n.subnet);
    let mut ipam = vec![json!({"Subnet": n.subnet, "Gateway": n.gateway})];
    if let Some(subnet) = n.ipv6 {
        ipam.push(json!({"Subnet": subnet.to_string(), "Gateway": subnet.gateway().to_string()}));
    }
    let containers: serde_json::Map<String, Value> = endpoints
        .iter()
        .map(|(id, name, addresses)| {
            let ipv6 = addresses
                .ipv6
                .zip(n.ipv6)
                .map_or_else(String::new, |(address, subnet)| {
                    format!("{address}/{}", subnet.prefix)
                });
            (
                id.clone(),
                json!({
                    "Name": name,
                    "EndpointID": "",
                    "MacAddress": "",
                    "IPv4Address": format!("{}/{prefix}", addresses.ipv4),
                    "IPv6Address": ipv6,
                }),
            )
        })
        .collect();
    json!({
        "Name": n.name,
        "Id": n.docker_id(),
        "Created": rfc3339(Some(n.created)),
        "Scope": "local",
        "Driver": "bridge",
        "EnableIPv4": true,
        "EnableIPv6": n.ipv6.is_some(),
        "IPAM": {
            "Driver": "default",
            "Options": null,
            "Config": ipam,
        },
        "Internal": false,
        "Attachable": false,
        "Ingress": false,
        "ConfigFrom": {"Network": ""},
        "ConfigOnly": false,
        "Containers": containers,
        "Options": {"com.docker.network.bridge.name": n.bridge},
        "Labels": n.labels,
    })
}

/// `host` or `none`, which podbag keeps no record of.
pub fn special_network_json(name: &str) -> Value {
    json!({
        "Name": name,
        "Id": docker_id_of(name),
        "Created": rfc3339(None),
        "Scope": "local",
        "Driver": if name == "host" { "host" } else { "null" },
        "EnableIPv4": name == "host",
        "EnableIPv6": false,
        "IPAM": {"Driver": "default", "Options": null, "Config": []},
        "Internal": false,
        "Attachable": false,
        "Ingress": false,
        "ConfigFrom": {"Network": ""},
        "ConfigOnly": false,
        "Containers": {},
        "Options": {},
        "Labels": {},
    })
}

/// Routes under `/volumes`.
pub fn volumes(req: &Request, ctx: &Ctx, rest: &[&str]) -> Option<Reply> {
    Some(match (req.method.as_str(), rest) {
        ("GET", []) => list_volumes(req, ctx),
        ("POST", ["create"]) => create_volume(req, ctx),
        ("POST", ["prune"]) => prune_volumes(req),
        ("GET", [name]) => match Volume::get(name) {
            Ok(v) => Reply::Json(200, volume_json(&v)),
            Err(e) => no_such_volume(name, &e),
        },
        ("DELETE", [name]) => remove_volume(name, req.query.bool_param("force")),
        ("PUT", [_]) => not_supported("cluster volumes", "use local volumes"),
        _ => return None,
    })
}

fn remove_volume(name: &str, force: bool) -> Reply {
    let volume = Volume::get(name).ok();
    match Volume::remove(name, force) {
        Ok(()) => {
            if let Some(volume) = volume {
                publish(
                    "volume",
                    "destroy",
                    name,
                    &[("driver", volume.driver().to_string())],
                );
            }
            Reply::Empty(204)
        }
        Err(e) => no_such_volume(name, &e),
    }
}

fn no_such_volume(name: &str, e: &crate::Error) -> Reply {
    match e.kind {
        ErrorKind::NotFound => message(404, format!("get {name}: no such volume")),
        _ => reply_error(e),
    }
}

/// `GET /volumes?filters`: `name`, `dangling`, `driver` and `label`.
fn list_volumes(req: &Request, ctx: &Ctx) -> Reply {
    let filters = match filters(req) {
        Ok(f) => f,
        Err(reply) => return reply,
    };
    if let Err(reply) = check_volume_filters(&filters) {
        return reply;
    }
    let volumes = match Volume::list() {
        Ok(v) => v,
        Err(e) => return reply_error(&e),
    };
    let any = |key: &str, ok: &dyn Fn(&str) -> bool| {
        filters
            .get(key)
            .is_none_or(|values| values.iter().any(|v| ok(v)))
    };
    let out: Vec<Value> = volumes
        .iter()
        .filter(|v| labels_match(&v.labels, &filters))
        .filter(|v| any("name", &|want| v.name.contains(want)))
        .filter(|v| any("driver", &|want| want == v.driver()))
        .filter(|v| {
            any("dangling", &|want| {
                let unused = crate::volume::users_of(&ctx.dirs, &v.name)
                    .map(|u| u.is_empty())
                    .unwrap_or(false);
                matches!(want, "true" | "1") == unused
            })
        })
        .map(volume_json)
        .collect();
    Reply::Json(200, json!({"Volumes": out, "Warnings": []}))
}

/// `POST /volumes/create`.
fn create_volume(req: &Request, ctx: &Ctx) -> Reply {
    let body = match req.json() {
        Ok(b) => b,
        Err(e) => return message(400, format!("{e:?}")),
    };
    let driver = body["Driver"].as_str().unwrap_or("");
    let opts = match driver_options(&body["DriverOpts"]) {
        Ok(opts) => opts,
        Err(e) => return reply_error(&e),
    };
    let name = body["Name"].as_str().filter(|n| !n.is_empty());
    let labels = match labels_of(&body) {
        Ok(l) => l,
        Err(reply) => return reply,
    };
    match Volume::create_with_driver(&ctx.dirs, name, labels, driver, opts) {
        Ok(v) => {
            publish(
                "volume",
                "create",
                &v.name,
                &[("driver", v.driver().to_string())],
            );
            Reply::Json(201, volume_json(&v))
        }
        Err(e) => reply_error(&e),
    }
}

pub(super) fn driver_options(options: &Value) -> crate::Result<Labels> {
    if options.is_null() {
        return Ok(Labels::new());
    }
    serde_json::from_value(options.clone()).map_err(|_| {
        crate::Error::new(
            ErrorKind::InvalidInput,
            "invalid volume driver options",
            "options must be an object of strings",
            "set each driver option as a string",
        )
    })
}

/// `POST /volumes/prune?filters`: unused anonymous volumes, all unused ones with
/// `all=true`.
fn prune_volumes(req: &Request) -> Reply {
    let filters = match filters(req) {
        Ok(f) => f,
        Err(reply) => return reply,
    };
    if let Err(reply) = check_filters(&filters, &["all", "label", "label!"], &[]) {
        return reply;
    }
    let all = filters
        .get("all")
        .is_some_and(|v| v.iter().any(|x| matches!(x.as_str(), "true" | "1")));
    match Volume::prune_matching(all, &|v| labels_match(&v.labels, &filters)) {
        Ok((names, bytes)) => {
            for n in &names {
                publish("volume", "destroy", n, &[("driver", "local".to_string())]);
            }
            let deleted = if names.is_empty() {
                Value::Null
            } else {
                json!(names)
            };
            Reply::Json(
                200,
                json!({"VolumesDeleted": deleted, "SpaceReclaimed": bytes}),
            )
        }
        Err(e) => reply_error(&e),
    }
}

/// Routes under `/networks`.
pub fn networks(req: &Request, ctx: &Ctx, rest: &[&str]) -> Option<Reply> {
    Some(match (req.method.as_str(), rest) {
        ("GET", []) => list_networks(req, ctx),
        ("POST", ["create"]) => req
            .json()
            .ok()
            .and_then(|body| crate::swarm::http::create_network(ctx, &body))
            .unwrap_or_else(|| create_network(req)),
        ("POST", ["prune"]) => prune_networks(req),
        ("GET", [id]) => inspect_network(ctx, id),
        ("DELETE", [id]) => remove_network(ctx, id),
        ("POST", [id, "connect"]) => connect_network(req, id, true),
        ("POST", [id, "disconnect"]) => connect_network(req, id, false),
        _ => return None,
    })
}

const SPECIAL: [&str; 2] = ["host", "none"];

/// A network by name, full or short ID, or ID prefix: Some(network), or None for `host`
/// and `none`.
fn find_network(id: &str) -> Result<(String, Option<Network>), Reply> {
    let missing = || message(404, format!("network {id} not found"));
    if let Some(special) = SPECIAL.iter().find(|s| **s == id) {
        return Ok((special.to_string(), None));
    }
    let all = Network::list().map_err(|e| reply_error(&e))?;
    if let Some(n) = all.iter().find(|n| n.name == id) {
        return Ok((n.name.clone(), Some(n.clone())));
    }
    for special in SPECIAL {
        if docker_id_of(special).starts_with(id) {
            return Ok((special.to_string(), None));
        }
    }
    let matching: Vec<&Network> = all
        .iter()
        .filter(|n| !id.is_empty() && n.docker_id().starts_with(id))
        .collect();
    match matching.as_slice() {
        [n] => Ok((n.name.clone(), Some((*n).clone()))),
        [] => Err(missing()),
        _ => Err(message(400, format!("network {id} is ambiguous"))),
    }
}

/// `GET /networks?filters`: `name`, `id`, `driver`, `type` and `label`.
fn list_networks(req: &Request, ctx: &Ctx) -> Reply {
    let filters = match filters(req) {
        Ok(f) => f,
        Err(reply) => return reply,
    };
    if let Err(reply) = check_network_filters(&filters) {
        return reply;
    }
    let bridges = match Network::list() {
        Ok(n) => n,
        Err(e) => return reply_error(&e),
    };
    // (name, docker id, driver, value, labels)
    let mut all: Vec<(String, String, &str, Value, Labels)> = bridges
        .iter()
        .map(|n| {
            (
                n.name.clone(),
                n.docker_id(),
                crate::swarm::net::driver_of(&ctx.dirs, &n.name),
                {
                    let mut v = network_json(n, &[]);
                    crate::swarm::net::dress(&ctx.dirs, &mut v);
                    v
                },
                n.labels.clone(),
            )
        })
        .collect();
    for special in SPECIAL {
        let driver = if special == "host" { "host" } else { "null" };
        all.push((
            special.to_string(),
            docker_id_of(special),
            driver,
            special_network_json(special),
            Labels::new(),
        ));
    }
    let any = |key: &str, ok: &dyn Fn(&str) -> bool| {
        filters
            .get(key)
            .is_none_or(|values| values.iter().any(|v| ok(v)))
    };
    let out: Vec<Value> = all
        .into_iter()
        .filter(|(.., labels)| labels_match(labels, &filters))
        .filter(|(name, ..)| any("name", &|want| name.contains(want)))
        .filter(|(_, id, ..)| any("id", &|want| id.starts_with(want)))
        .filter(|(_, _, driver, ..)| any("driver", &|want| want == *driver))
        .filter(|(name, ..)| {
            let builtin = matches!(name.as_str(), "bridge" | "host" | "none");
            any("type", &|want| (want == "builtin") == builtin)
        })
        .map(|(_, _, _, v, _)| v)
        .collect();
    Reply::Json(200, Value::Array(out))
}

/// `GET /networks/{id}`.
fn inspect_network(ctx: &Ctx, id: &str) -> Reply {
    match find_network(id) {
        Ok((_, Some(n))) => {
            let endpoints = crate::network::endpoints(&ctx.dirs, &n.name).unwrap_or_default();
            let mut v = network_json(&n, &endpoints);
            crate::swarm::net::dress(&ctx.dirs, &mut v);
            Reply::Json(200, v)
        }
        Ok((name, None)) => Reply::Json(200, special_network_json(&name)),
        Err(reply) => reply,
    }
}

/// `POST /networks/{id}/connect` and `disconnect`: join or leave a secondary bridge.
fn connect_network(req: &Request, id: &str, join: bool) -> Reply {
    let body = match req.json() {
        Ok(b) => b,
        Err(e) => return message(400, format!("{e:?}")),
    };
    let (name, network) = match find_network(id) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let Some(network) = network else {
        return message(
            400,
            format!("cannot connect a container to network {name}: it is a network mode"),
        );
    };
    let Some(container) = body["Container"].as_str().filter(|c| !c.is_empty()) else {
        return message(400, "the request needs a Container");
    };
    let handle = match crate::ContainerHandle::get(container) {
        Ok(h) => h,
        Err(e) => return reply_error(&e),
    };
    let done = if join {
        let endpoint = &body["EndpointConfig"];
        let addresses = match super::create::fixed_addresses(endpoint) {
            Ok(addresses) => addresses,
            Err(e) => return reply_error(&e),
        };
        let aliases: Vec<String> = match &endpoint["Aliases"] {
            Value::Null => Vec::new(),
            Value::Array(list) => match list
                .iter()
                .map(|a| a.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
            {
                Some(list) => list,
                None => return message(400, "Aliases must be a list of strings"),
            },
            _ => return message(400, "Aliases must be a list of strings"),
        };
        handle.connect_with_addresses(&name, &aliases, addresses)
    } else {
        handle.disconnect(&name)
    };
    match done {
        Ok(()) => {
            publish(
                "network",
                if join { "connect" } else { "disconnect" },
                &network.docker_id(),
                &[
                    ("container", handle.id().to_string()),
                    ("name", name),
                    ("type", "bridge".to_string()),
                ],
            );
            Reply::Empty(200)
        }
        Err(e) => reply_error(&e),
    }
}

/// `POST /networks/create`: bridge networks with an optional subnet.
fn create_network(req: &Request) -> Reply {
    let body = match req.json() {
        Ok(b) => b,
        Err(e) => return message(400, format!("{e:?}")),
    };
    let Some(name) = body["Name"].as_str().filter(|n| !n.is_empty()) else {
        return message(400, "the network needs a Name");
    };
    let driver = body["Driver"].as_str().unwrap_or("");
    if !matches!(driver, "" | "bridge") {
        return message(
            400,
            format!("network driver {driver} is not supported (podbag makes bridge networks)"),
        );
    }
    if body["Ingress"].as_bool().unwrap_or(false) {
        return message(400, "ingress networks are not supported");
    }
    if body["EnableIPv4"].as_bool() == Some(false) {
        return message(
            400,
            "only IPv4 and dual-stack bridge networks are supported",
        );
    }
    let ipam_driver = body["IPAM"]["Driver"].as_str().unwrap_or("");
    if !matches!(ipam_driver, "" | "default") {
        return message(400, format!("IPAM driver {ipam_driver} is not supported"));
    }
    let configs = body["IPAM"]["Config"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut warnings = Vec::new();
    let mut ipv4 = None;
    let mut ipv6 = None;
    for config in &configs {
        let Some(subnet) = config["Subnet"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if subnet.contains(':') {
            if ipv6.is_some() {
                return message(400, "only one IPv6 subnet is supported");
            }
            let parsed = match subnet.parse() {
                Ok(subnet) => subnet,
                Err(e) => return reply_error(&e),
            };
            ipv6 = Some((parsed, config));
        } else if ipv4.is_some() {
            warnings.push("only the first IPv4 IPAM config is used".to_string());
        } else {
            ipv4 = Some((subnet, config));
        }
    }
    if body["Internal"].as_bool().unwrap_or(false) {
        warnings.push("internal networks are not supported: this one reaches outside".to_string());
    }
    if body["Options"].as_object().is_some_and(|o| !o.is_empty()) {
        warnings.push("network options are ignored".to_string());
    }
    let labels = match labels_of(&body) {
        Ok(l) => l,
        Err(reply) => return reply,
    };
    let subnet = ipv4.map(|(subnet, _)| subnet);
    let dual_stack = ipv6.is_some() || body["EnableIPv6"].as_bool().unwrap_or(false);
    let created = if dual_stack {
        Network::create_dual_stack(name, subnet, ipv6.map(|(subnet, _)| subnet), labels)
    } else {
        Network::create_with(name, subnet, labels)
    };
    let network = match created {
        Ok(n) => n,
        Err(e) => return reply_error(&e),
    };
    let configured = ipv4
        .map(|(_, config)| (config, network.gateway.clone()))
        .into_iter()
        .chain(
            ipv6.zip(network.ipv6)
                .map(|((_, config), subnet)| (config, subnet.gateway().to_string())),
        );
    for (config, gateway) in configured {
        if let Some(gw) = config["Gateway"]
            .as_str()
            .filter(|gw| !gw.is_empty() && *gw != gateway)
        {
            warnings.push(format!("gateway {gw} is ignored: the gateway is {gateway}",));
        }
        if config["IPRange"].as_str().is_some_and(|r| !r.is_empty()) {
            warnings
                .push("IP ranges are ignored: addresses come from the whole subnet".to_string());
        }
    }
    let id = network.docker_id();
    publish(
        "network",
        "create",
        &id,
        &[
            ("name", network.name.clone()),
            ("type", "bridge".to_string()),
        ],
    );
    Reply::Json(201, json!({"Id": id, "Warning": warnings.join("; ")}))
}

/// `DELETE /networks/{id}`.
fn remove_network(ctx: &Ctx, id: &str) -> Reply {
    let (name, network) = match find_network(id) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let Some(network) = network.filter(|n| n.name != "bridge") else {
        return message(
            403,
            format!("{name} is a pre-defined network and cannot be removed"),
        );
    };
    match Network::remove(&network.name) {
        Ok(()) => {
            crate::swarm::net::forget(&ctx.dirs, &network.name);
            publish(
                "network",
                "destroy",
                &network.docker_id(),
                &[
                    ("name", network.name.clone()),
                    ("type", "bridge".to_string()),
                ],
            );
            Reply::Empty(204)
        }
        Err(e) => reply_error(&e),
    }
}

/// `POST /networks/prune`: networks no container uses.
fn prune_networks(req: &Request) -> Reply {
    let filters = match filters(req) {
        Ok(f) => f,
        Err(reply) => return reply,
    };
    if let Err(reply) = check_network_prune_filters(&filters) {
        return reply;
    }
    match Network::prune_matching(&|n| labels_match(&n.labels, &filters)) {
        Ok(removed) => {
            let names: Vec<String> = removed
                .iter()
                .map(|n| {
                    publish(
                        "network",
                        "destroy",
                        &n.docker_id(),
                        &[("name", n.name.clone()), ("type", "bridge".to_string())],
                    );
                    n.name.clone()
                })
                .collect();
            let deleted = if names.is_empty() {
                Value::Null
            } else {
                json!(names)
            };
            Reply::Json(200, json!({ "NetworksDeleted": deleted }))
        }
        Err(e) => reply_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_filters_podbag_lacks_are_refused() {
        let keys = |k: &str| BTreeMap::from([(k.to_string(), vec!["x".to_string()])]);
        assert!(check_volume_filters(&keys("name")).is_ok());
        assert!(check_volume_filters(&keys("bogus")).is_err());
        assert!(check_network_filters(&keys("type")).is_ok());
        assert!(check_network_filters(&keys("dangling")).is_err());
        assert!(check_network_prune_filters(&keys("until")).is_err());
        assert!(check_network_prune_filters(&keys("label")).is_ok());
    }

    #[test]
    fn volume_shape() {
        let v = Volume {
            name: "v1".into(),
            mountpoint: "/d/volumes/v1/data".into(),
            created: 1_700_000_000,
            anonymous: false,
            labels: Labels::new(),
            nfs: None,
            plugin: None,
        };
        let j = volume_json(&v);
        assert_eq!(j["Name"], "v1");
        assert_eq!(j["Driver"], "local");
        assert_eq!(j["Mountpoint"], "/d/volumes/v1/data");
        assert_eq!(j["CreatedAt"], "2023-11-14T22:13:20.000000000Z");
        assert_eq!(j["Labels"], json!({}));
        assert_eq!(j["Scope"], "local");
    }

    #[test]
    fn nfs_volume_inspect_returns_docker_options() {
        let options = json!({"type":"nfs", "device":":/data", "o":"addr=192.0.2.1,vers=4"});
        let v = Volume {
            name: "share".into(),
            mountpoint: "/d/volumes/share/data".into(),
            created: 1_700_000_000,
            anonymous: false,
            labels: Labels::new(),
            nfs: crate::volume::parse_options(driver_options(&options).unwrap()).unwrap(),
            plugin: None,
        };
        assert_eq!(volume_json(&v)["Options"], options);
        assert!(driver_options(&json!({"type":false})).is_err());
    }

    #[test]
    fn network_shapes() {
        let n = Network {
            name: "app".into(),
            id: "abcdef123456".into(),
            subnet: "10.78.1.0/24".into(),
            gateway: "10.78.1.1".into(),
            ipv6: Some("fd7f:1::/64".parse().unwrap()),
            bridge: "pbg-1".into(),
            created: 1_700_000_000,
            labels: [("a".to_string(), "b".to_string())].into_iter().collect(),
        };
        let c = "c".repeat(64);
        let j = network_json(
            &n,
            &[(
                c.clone(),
                "web".into(),
                crate::network::Addresses {
                    ipv4: "10.78.1.2".parse().unwrap(),
                    ipv6: Some("fd7f:1::2".parse().unwrap()),
                },
            )],
        );
        assert_eq!(j["Name"], "app");
        assert_eq!(j["Id"], n.docker_id());
        assert_eq!(j["Driver"], "bridge");
        assert_eq!(j["Scope"], "local");
        assert_eq!(
            j["IPAM"]["Config"],
            json!([{"Subnet": "10.78.1.0/24", "Gateway": "10.78.1.1"}, {"Subnet": "fd7f:1::/64", "Gateway": "fd7f:1::1"}])
        );
        assert_eq!(j["Containers"][&c]["Name"], "web");
        assert_eq!(j["Containers"][&c]["IPv4Address"], "10.78.1.2/24");
        assert_eq!(j["EnableIPv6"], true);
        assert_eq!(j["Containers"][&c]["IPv6Address"], "fd7f:1::2/64");
        assert_eq!(j["Labels"], json!({"a": "b"}));
        let host = special_network_json("host");
        assert_eq!(host["Driver"], "host");
        assert_eq!(host["Id"].as_str().unwrap().len(), 64);
        assert_eq!(host["IPAM"]["Config"], json!([]));
        assert_eq!(special_network_json("none")["Driver"], "null");
        assert_eq!(host["Labels"], json!({}));
    }
}
