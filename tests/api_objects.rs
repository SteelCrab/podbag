#![cfg(target_os = "linux")]
//! Volumes and networks through the Docker Engine API.

mod common;
use common::api::*;
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::*;
use std::process::Output;

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

struct Setup {
    _reg: FakeRegistry,
    sb: Sandbox,
    service: Service,
    image: String,
}

fn setup() -> Setup {
    let reg = FakeRegistry::start(false);
    reg.push(
        "img",
        "v1",
        &TestImage {
            layers: vec![alpine_layer()],
            config: serde_json::json!({"Env": [PATH], "Cmd": ["sh"]}),
        },
    );
    let image = format!("{}/img:v1", reg.host);
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    let s = Setup {
        _reg: reg,
        sb,
        service,
        image,
    };
    s.ok(&["pull", &s.image]);
    s
}

impl Setup {
    fn docker(&self, args: &[&str]) -> Output {
        docker_out(&self.sb, &self.service, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.docker(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "docker {args:?}: {}",
            stderr(&out)
        );
        stdout(&out)
    }

    fn lines(&self, args: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = self.ok(args).lines().map(String::from).collect();
        v.sort();
        v
    }
}

#[test]
fn docker_dual_stack_bridge_assigns_ipv6_and_connects_peers() {
    let s = setup();
    let prefix = format!("fd89:{:x}::", std::process::id() % 65536);
    let subnet = format!("{prefix}/64");
    s.ok(&["network", "create", "--ipv6", "--subnet", &subnet, "dual"]);
    s.ok(&[
        "run",
        "-d",
        "--name",
        "dual-peer",
        "--network",
        "dual",
        &s.image,
        "sleep",
        "30",
    ]);
    let address = s.ok(&[
        "inspect",
        "--format",
        "{{.NetworkSettings.Networks.dual.GlobalIPv6Address}}",
        "dual-peer",
    ]);
    assert_eq!(address, format!("{prefix}2"));
    let ipam = s.ok(&[
        "network",
        "inspect",
        "--format",
        "{{.EnableIPv6}} {{json .IPAM.Config}}",
        "dual",
    ]);
    assert!(ipam.starts_with("true "), "{ipam}");
    assert!(ipam.contains(&subnet), "{ipam}");
    let output = s.ok(&[
        "run",
        "--rm",
        "--network",
        "dual",
        &s.image,
        "ping6",
        "-c",
        "1",
        "-W",
        "2",
        "dual-peer",
    ]);
    assert!(output.contains(&address), "{output}");
    s.ok(&["rm", "-f", "dual-peer"]);
    s.ok(&["network", "rm", "dual"]);
}

#[test]
fn docker_bridge_keeps_fixed_addresses_and_refuses_duplicates() {
    let s = setup();
    let prefix = format!("fd8a:{:x}::", std::process::id() % 65536);
    let host = std::process::id() % 250;
    let subnet4 = format!("10.73.{host}.0/24");
    let subnet6 = format!("{prefix}/64");
    let ipv4 = format!("10.73.{host}.42");
    let ipv6 = format!("{prefix}42");
    s.ok(&[
        "network", "create", "--ipv6", "--subnet", &subnet4, "--subnet", &subnet6, "fixed",
    ]);
    s.ok(&[
        "run",
        "-d",
        "--name",
        "fixed-first",
        "--network",
        "fixed",
        "--ip",
        &ipv4,
        "--ip6",
        &ipv6,
        &s.image,
        "sleep",
        "30",
    ]);
    let address = s.ok(&[
        "inspect",
        "--format",
        "{{with .NetworkSettings.Networks.fixed}}{{.IPAddress}} {{.GlobalIPv6Address}}{{end}}",
        "fixed-first",
    ]);
    assert_eq!(address, format!("{ipv4} {ipv6}"));
    let duplicate = s.docker(&[
        "run",
        "--rm",
        "--network",
        "fixed",
        "--ip6",
        &ipv6,
        &s.image,
        "true",
    ]);
    assert!(
        !duplicate.status.success(),
        "duplicate address was assigned"
    );
    assert!(
        stderr(&duplicate).contains("in use"),
        "{}",
        stderr(&duplicate)
    );
    s.ok(&["stop", "fixed-first"]);
    s.ok(&["start", "fixed-first"]);
    assert_eq!(
        s.ok(&[
            "inspect",
            "--format",
            "{{with .NetworkSettings.Networks.fixed}}{{.IPAddress}} {{.GlobalIPv6Address}}{{end}}",
            "fixed-first",
        ]),
        address,
    );
    s.ok(&["create", "--name", "fixed-second", &s.image, "sleep", "30"]);
    let other = format!("{prefix}43");
    s.ok(&[
        "network",
        "connect",
        "--ip6",
        &other,
        "fixed",
        "fixed-second",
    ]);
    s.ok(&["start", "fixed-second"]);
    assert_eq!(
        s.ok(&[
            "inspect",
            "--format",
            "{{.NetworkSettings.Networks.fixed.GlobalIPv6Address}}",
            "fixed-second"
        ]),
        other,
    );
    s.ok(&["rm", "-f", "fixed-first", "fixed-second"]);
    s.ok(&["network", "rm", "fixed"]);
}

#[test]
fn docker_volume_lifecycle() {
    let s = setup();
    assert_eq!(s.ok(&["volume", "create", "v1"]), "v1");
    assert!(
        s.lines(&["volume", "ls", "--format", "{{.Name}}"])
            .contains(&"v1".to_string())
    );
    let mountpoint = s.ok(&["volume", "inspect", "--format", "{{.Mountpoint}}", "v1"]);
    assert!(mountpoint.ends_with("/volumes/v1/data"), "{mountpoint}");
    assert_eq!(
        s.ok(&[
            "volume",
            "inspect",
            "--format",
            "{{.Driver}} {{.Scope}}",
            "v1"
        ]),
        "local local"
    );
    s.ok(&["volume", "rm", "v1"]);
    assert!(s.lines(&["volume", "ls", "-q"]).is_empty());
    let out = s.docker(&["volume", "inspect", "v1"]);
    assert_ne!(out.status.code(), Some(0));

    // A removed container leaves its anonymous volume; prune takes it, and with -a the
    // named ones too.
    s.ok(&["run", "--name", "anon", "-v", "/data", &s.image, "true"]);
    s.ok(&["rm", "anon"]);
    let left = s.lines(&["volume", "ls", "-q"]);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].len(), 64, "{left:?}");
    s.ok(&["volume", "create", "named"]);
    s.ok(&["volume", "prune", "-f"]);
    assert_eq!(s.lines(&["volume", "ls", "-q"]), ["named"]);
    s.ok(&["volume", "prune", "-a", "-f"]);
    assert!(s.lines(&["volume", "ls", "-q"]).is_empty());
}

#[test]
fn docker_volume_in_use_is_a_conflict() {
    let s = setup();
    s.ok(&["run", "--name", "user", "-v", "busy:/b", &s.image, "true"]);
    let out = s.docker(&["volume", "rm", "busy"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("used by"), "{}", stderr(&out));
    s.ok(&["volume", "prune", "-a", "-f"]);
    assert_eq!(
        s.lines(&["volume", "ls", "-q"]),
        ["busy"],
        "in use: not pruned"
    );
    s.ok(&["rm", "user"]);
    s.ok(&["volume", "rm", "busy"]);
}

#[test]
fn docker_network_lifecycle() {
    let _turn = network_turn();
    let s = setup();
    let id = s.ok(&["network", "create", "--subnet", "10.79.5.0/24", "apiobj-n1"]);
    assert_eq!(id.len(), 64, "{id}");
    let ls = s.lines(&["network", "ls", "--format", "{{.Name}} {{.Driver}}"]);
    for want in [
        "bridge bridge",
        "host host",
        "none null",
        "apiobj-n1 bridge",
    ] {
        assert!(ls.contains(&want.to_string()), "{want} missing from {ls:?}");
    }
    let subnet = s.ok(&[
        "network",
        "inspect",
        "--format",
        "{{(index .IPAM.Config 0).Subnet}}",
        "apiobj-n1",
    ]);
    assert_eq!(subnet, "10.79.5.0/24");
    assert_eq!(
        s.ok(&["network", "inspect", "--format", "{{.Name}}", &id[..12]]),
        "apiobj-n1",
        "found by ID"
    );
    s.ok(&[
        "run",
        "-d",
        "--name",
        "member",
        "--network",
        "apiobj-n1",
        &s.image,
        "sleep",
        "30",
    ]);
    let ip = s.ok(&[
        "inspect",
        "--format",
        "{{(index .NetworkSettings.Networks \"apiobj-n1\").IPAddress}}",
        "member",
    ]);
    assert!(ip.starts_with("10.79.5."), "{ip}");
    let members = s.ok(&[
        "network",
        "inspect",
        "--format",
        "{{range .Containers}}{{.Name}}{{end}}",
        "apiobj-n1",
    ]);
    assert_eq!(members, "member");
    let out = s.docker(&["network", "rm", "apiobj-n1"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("member"), "{}", stderr(&out));
    s.ok(&["rm", "-f", "member"]);
    s.ok(&["network", "rm", "apiobj-n1"]);
    let ls = s.lines(&["network", "ls", "--format", "{{.Name}}"]);
    assert!(!ls.contains(&"apiobj-n1".to_string()), "{ls:?}");

    s.ok(&["network", "create", "apiobj-n2"]);
    s.ok(&["network", "prune", "-f"]);
    let ls = s.lines(&["network", "ls", "--format", "{{.Name}}"]);
    assert!(!ls.contains(&"apiobj-n2".to_string()), "{ls:?}");
    assert!(ls.contains(&"bridge".to_string()), "{ls:?}");

    let out = s.docker(&["network", "rm", "bridge"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("pre-defined"), "{}", stderr(&out));
    let out = s.docker(&["network", "create", "-d", "overlay", "apiobj-n3"]);
    assert_ne!(out.status.code(), Some(0));
}

/// A service without an image.
fn bare() -> Setup {
    let reg = FakeRegistry::start(false);
    let sb = Sandbox::new();
    let service = Service::start(&sb, &["--timeout", "0"]);
    Setup {
        _reg: reg,
        sb,
        service,
        image: String::new(),
    }
}

impl Setup {
    /// A `GET` with `filters`, as JSON.
    fn get(&self, path: &str, filters: &str) -> serde_json::Value {
        let (body, head) = curl(
            &self.service.socket,
            &[
                "-G",
                "--data-urlencode",
                &format!("filters={filters}"),
                path,
            ],
        );
        serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("{path} {filters}: {e}: {head} {body}"))
    }
}

/// The names in a list of networks or volumes.
fn names_in(list: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = list
        .as_array()
        .unwrap_or_else(|| panic!("not a list: {list}"))
        .iter()
        .map(|v| v["Name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn labels_on_networks_and_volumes() {
    let _turn = network_turn();
    let s = bare();
    let labels = |kind: &str, name: &str| -> serde_json::Value {
        serde_json::from_str(&s.ok(&[kind, "inspect", "--format", "{{json .Labels}}", name]))
            .unwrap()
    };
    // Networks.
    s.ok(&[
        "network", "create", "--label", "a=b", "--label", "c", "--label", "eq=x=y", "n1",
    ]);
    s.ok(&["network", "create", "n2"]);
    assert_eq!(
        labels("network", "n1"),
        serde_json::json!({"a": "b", "c": "", "eq": "x=y"})
    );
    assert_eq!(labels("network", "n2"), serde_json::json!({}));
    let networks = |filters: &[&str]| {
        let mut args = vec!["network", "ls", "--format", "{{.Name}}"];
        for f in filters {
            args.extend(["--filter", f]);
        }
        s.lines(&args)
    };
    assert_eq!(networks(&["label=a=b"]), ["n1"]);
    assert_eq!(networks(&["label=a"]), ["n1"]);
    assert_eq!(networks(&["label=c"]), ["n1"]);
    assert_eq!(networks(&["label=eq=x=y"]), ["n1"]);
    assert!(networks(&["label=a=x"]).is_empty());
    assert!(networks(&["label=zz"]).is_empty());
    // Every label that was asked for must be there.
    assert!(networks(&["label=a", "label=zz"]).is_empty());
    assert_eq!(networks(&["label=a", "label=c"]), ["n1"]);
    // Pruning with a label takes the unused networks that have it, or do not have it.
    s.ok(&["network", "create", "--label", "keep=1", "n3"]);
    s.ok(&["network", "prune", "-f", "--filter", "label=a=b"]);
    assert_eq!(networks(&["type=custom"]), ["n2", "n3"]);
    s.ok(&["network", "prune", "-f", "--filter", "label!=keep"]);
    assert_eq!(networks(&["type=custom"]), ["n3"]);

    // Volumes.
    s.ok(&["volume", "create", "--label", "a=b", "--label", "c", "v1"]);
    s.ok(&["volume", "create", "v2"]);
    assert_eq!(
        labels("volume", "v1"),
        serde_json::json!({"a": "b", "c": ""})
    );
    assert_eq!(labels("volume", "v2"), serde_json::json!({}));
    let volumes = |filters: &[&str]| {
        let mut args = vec!["volume", "ls", "--format", "{{.Name}}"];
        for f in filters {
            args.extend(["--filter", f]);
        }
        s.lines(&args)
    };
    assert_eq!(volumes(&["label=a=b"]), ["v1"]);
    assert_eq!(volumes(&["label=c"]), ["v1"]);
    assert!(volumes(&["label=a=x"]).is_empty());
    assert!(volumes(&["label=a", "label=zz"]).is_empty());
    // Creating a volume that exists returns it as it is.
    s.ok(&["volume", "create", "--label", "other=1", "v1"]);
    assert_eq!(
        labels("volume", "v1"),
        serde_json::json!({"a": "b", "c": ""})
    );
    s.ok(&["volume", "create", "--label", "keep=1", "v3"]);
    s.ok(&["volume", "prune", "-a", "-f", "--filter", "label=a=b"]);
    assert_eq!(volumes(&[]), ["v2", "v3"]);
    s.ok(&["volume", "prune", "-a", "-f", "--filter", "label!=keep"]);
    assert_eq!(volumes(&[]), ["v3"]);
}

#[test]
fn docker_network_list_can_match_an_exact_name() {
    let _turn = network_turn();
    let s = bare();
    s.ok(&["network", "create", "kind.local"]);
    s.ok(&["network", "create", "kind.local-extra"]);
    let names = s.lines(&[
        "network",
        "ls",
        "--filter",
        r"name=^kind\.local$",
        "--format",
        "{{.Name}}",
    ]);
    assert_eq!(names, ["kind.local"]);
}

#[test]
fn label_filters_in_every_form() {
    let _turn = network_turn();
    let s = bare();
    s.ok(&[
        "network", "create", "--label", "a=b", "--label", "e=", "--label", "eq=x=y", "n1",
    ]);
    s.ok(&["network", "create", "--label", "a=other", "n2"]);
    s.ok(&[
        "volume", "create", "--label", "a=b", "--label", "e=", "--label", "eq=x=y", "v1",
    ]);
    s.ok(&["volume", "create", "--label", "a=other", "v2"]);
    let cases: [(&str, &[&str]); 9] = [
        // As a list (the docker CLI) and as a map (older clients, Compose).
        (r#"{"label":["a=b"]}"#, &["1"]),
        (r#"{"label":{"a=b":true}}"#, &["1"]),
        // A key alone: any value.
        (r#"{"label":["a"]}"#, &["1", "2"]),
        // Several: all of them.
        (r#"{"label":["a=b","nope"]}"#, &[]),
        (r#"{"label":{"a":true,"eq=x=y":true}}"#, &["1"]),
        // An empty value is a value.
        (r#"{"label":["e="]}"#, &["1"]),
        (r#"{"label":["e=x"]}"#, &[]),
        // A value with an equals sign.
        (r#"{"label":["eq=x=y"]}"#, &["1"]),
        (r#"{"label":["eq=x"]}"#, &[]),
    ];
    for (filters, want) in cases {
        let networks = names_in(&s.get("/networks", filters));
        let want_networks: Vec<String> = want.iter().map(|n| format!("n{n}")).collect();
        assert_eq!(networks, want_networks, "networks with {filters}");
        let volumes = names_in(&s.get("/volumes", filters)["Volumes"]);
        let want_volumes: Vec<String> = want.iter().map(|n| format!("v{n}")).collect();
        assert_eq!(volumes, want_volumes, "volumes with {filters}");
    }
}

impl Setup {
    /// `POST path` with a JSON body: (status code, body).
    fn post(&self, path: &str, body: &serde_json::Value) -> (u16, String) {
        let (body, head) = curl(
            &self.service.socket,
            &[
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body.to_string(),
                path,
            ],
        );
        let code = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (code, body)
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.docker(args);
        assert_ne!(out.status.code(), Some(0), "docker {args:?} succeeded");
        stderr(&out)
    }

    fn inspect(&self, what: &str) -> serde_json::Value {
        let text = self.ok(&["inspect", what]);
        serde_json::from_str::<serde_json::Value>(&text).unwrap()[0].clone()
    }
}

#[test]
fn aliases_and_networks_through_the_api() {
    let _turn = network_turn();
    let s = setup();
    s.ok(&["network", "create", "front"]);
    s.ok(&["network", "create", "back"]);
    // As Compose creates a container: every network with its aliases.
    let (code, body) = s.post(
        "/containers/create?name=web",
        &serde_json::json!({
            "Image": s.image,
            "Cmd": ["sleep", "300"],
            "HostConfig": {"NetworkMode": "front"},
            "NetworkingConfig": {"EndpointsConfig": {
                "front": {"Aliases": ["web", "p-web-1"]},
                "back": {"Aliases": ["w"]}
            }}
        }),
    );
    assert_eq!(code, 201, "{body}");
    let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["Warnings"], serde_json::json!([]), "{body}");
    s.ok(&["start", "web"]);
    let web = s.inspect("web");
    let networks = &web["NetworkSettings"]["Networks"];
    for (name, alias) in [("front", "p-web-1"), ("back", "w")] {
        let id = s.ok(&["network", "inspect", "--format", "{{.Id}}", name]);
        let endpoint = &networks[name];
        assert_eq!(endpoint["NetworkID"], id.trim(), "{networks}");
        assert!(
            endpoint["Aliases"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x == alias)),
            "{networks}"
        );
        assert!(
            !endpoint["IPAddress"].as_str().unwrap_or("").is_empty(),
            "{networks}"
        );
    }
    // The list shows the same network IDs (Compose compares them).
    let list = s.get("/containers/json", r#"{"name":["web"]}"#);
    let listed = &list[0]["NetworkSettings"]["Networks"];
    assert_eq!(listed["back"]["NetworkID"], networks["back"]["NetworkID"]);
    // (The docker CLI lists them in no fixed order.)
    let mut listed: Vec<String> = s
        .ok(&["ps", "--format", "{{.Networks}}"])
        .trim()
        .split(',')
        .map(String::from)
        .collect();
    listed.sort();
    assert_eq!(listed, ["back", "front"]);
    // Another container of `back` finds it by its alias there.
    s.ok(&[
        "run",
        "-d",
        "--name",
        "cache",
        "--network",
        "back",
        &s.image,
        "sleep",
        "300",
    ]);
    s.ok(&["exec", "cache", "ping", "-c1", "-W2", "w"]);
    s.ok(&["rm", "-f", "web", "cache"]);
    s.ok(&["network", "rm", "front", "back"]);
}

#[test]
fn an_alias_cannot_break_the_hosts_file() {
    let _turn = network_turn();
    let s = setup();
    s.ok(&["network", "create", "front"]);
    for alias in ["a b", "x\ny", "#c", ""] {
        let (code, body) = s.post(
            "/containers/create",
            &serde_json::json!({
                "Image": s.image,
                "Cmd": ["true"],
                "HostConfig": {"NetworkMode": "front"},
                "NetworkingConfig": {"EndpointsConfig": {"front": {"Aliases": [alias]}}}
            }),
        );
        assert_eq!(code, 400, "{alias:?}: {body}");
    }
    assert!(s.ok(&["ps", "-aq"]).is_empty());
    s.ok(&["network", "rm", "front"]);
}

#[test]
fn connect_and_disconnect_through_the_api() {
    let _turn = network_turn();
    let s = setup();
    s.ok(&["network", "create", "front"]);
    s.ok(&["network", "create", "back"]);
    s.ok(&[
        "create",
        "--name",
        "web",
        "--network",
        "front",
        &s.image,
        "sleep",
        "300",
    ]);
    // Before its start, as Compose does for older engines.
    s.ok(&["network", "connect", "--alias", "w", "back", "web"]);
    assert!(
        s.fails(&["network", "connect", "back", "web"])
            .contains("already")
    );
    s.ok(&["start", "web"]);
    let networks = s.inspect("web")["NetworkSettings"]["Networks"].clone();
    assert!(
        networks["front"].is_object() && networks["back"].is_object(),
        "{networks}"
    );
    assert!(
        networks["back"]["Aliases"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == "w")),
        "{networks}"
    );
    s.ok(&["network", "create", "third"]);
    let pid = s.inspect("web")["State"]["Pid"].clone();
    s.ok(&["network", "connect", "--alias", "w3", "third", "web"]);
    let live = s.inspect("web");
    assert_eq!(live["State"]["Pid"], pid);
    assert!(
        live["NetworkSettings"]["Networks"]["third"]["IPAddress"]
            .as_str()
            .is_some_and(|ip| !ip.is_empty()),
        "{live}"
    );
    s.ok(&["exec", "web", "ping", "-c1", "-W2", "w3"]);
    s.ok(&["network", "disconnect", "back", "web"]);
    assert_eq!(s.inspect("web")["State"]["Pid"], pid);
    assert!(
        s.fails(&["network", "disconnect", "back", "web"])
            .contains("not connected")
    );
    let links = s.ok(&["exec", "web", "ip", "-o", "link"]);
    assert!(!links.contains("eth1") && links.contains("eth2"), "{links}");
    s.ok(&["stop", "-t", "0", "web"]);
    s.ok(&["network", "disconnect", "third", "web"]);
    // Its primary network stays; the host's network is not one to join.
    s.fails(&["network", "disconnect", "front", "web"]);
    s.fails(&["network", "connect", "host", "web"]);
    let networks = s.inspect("web")["NetworkSettings"]["Networks"].clone();
    assert!(networks["back"].is_null(), "{networks}");
    s.ok(&["rm", "-f", "web"]);
    s.ok(&["network", "rm", "front", "back", "third"]);
}
