//! CNI: the network plugins that connect a pod (`ADD`), check it (`CHECK`) and disconnect
//! it (`DEL`), as the CNI specification describes them. The configuration is the first
//! file of the configuration folder; a list runs its plugins in order, each one getting
//! the result of the one before.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::{Error, ErrorKind, Result};

/// A network configuration list (a single plugin's configuration counts as a list of one).
#[derive(Debug, Clone, PartialEq)]
pub struct NetConf {
    pub name: String,
    pub cni_version: String,
    pub plugins: Vec<Value>,
    /// The file it came from.
    pub file: PathBuf,
}

/// The configuration the pods use: the first `.conflist`, `.conf` or `.json` file in name
/// order, as the kubelet's runtimes pick it.
pub fn load(dir: &Path) -> Result<NetConf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| not_ready(dir, &e.to_string()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("conf" | "conflist" | "json")
            )
        })
        .collect();
    files.sort();
    let file = files
        .into_iter()
        .next()
        .ok_or_else(|| not_ready(dir, "the folder has no configuration"))?;
    let text = std::fs::read_to_string(&file).map_err(|e| not_ready(&file, &e.to_string()))?;
    parse(&text, &file)
}

fn not_ready(path: &Path, why: &str) -> Error {
    Error::new(
        ErrorKind::MissingRequirement,
        format!("no CNI network configuration in {}", path.display()),
        why,
        "install a CNI network (the cluster's network plugin writes it)",
    )
}

/// Read a configuration list, or a single plugin's configuration.
pub fn parse(text: &str, file: &Path) -> Result<NetConf> {
    let bad = |why: &str| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid CNI configuration {}", file.display()),
            why,
            "fix the file, or remove it",
        )
    };
    let v: Value = serde_json::from_str(text).map_err(|e| bad(&e.to_string()))?;
    let name = v["name"].as_str().unwrap_or_default().to_string();
    let cni_version = v["cniVersion"].as_str().unwrap_or("0.4.0").to_string();
    let plugins = match v.get("plugins") {
        Some(Value::Array(list)) => list.clone(),
        Some(_) => return Err(bad("plugins is not a list")),
        None => vec![v.clone()],
    };
    if plugins.is_empty() {
        return Err(bad("it has no plugins"));
    }
    if plugins
        .iter()
        .any(|p| p["type"].as_str().unwrap_or_default().is_empty())
    {
        return Err(bad("a plugin has no type"));
    }
    if name.is_empty() {
        return Err(bad("it has no name"));
    }
    Ok(NetConf {
        name,
        cni_version,
        plugins,
        file: file.to_path_buf(),
    })
}

/// A port the pod publishes on the host (`portmap`'s `portMappings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    pub host_port: i32,
    pub container_port: i32,
    pub protocol: String,
    pub host_ip: String,
}

/// What one ADD, CHECK or DEL is about.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub container_id: String,
    /// The network namespace's path.
    pub netns: String,
    pub ifname: String,
    /// `CNI_ARGS` pairs (the pod's namespace, name and UID).
    pub args: Vec<(String, String)>,
    pub ports: Vec<PortMapping>,
}

/// The CNI_ARGS a kubelet's runtime passes.
pub fn pod_args(namespace: &str, name: &str, sandbox: &str, uid: &str) -> Vec<(String, String)> {
    vec![
        ("IgnoreUnknown".into(), "1".into()),
        ("K8S_POD_NAMESPACE".into(), namespace.into()),
        ("K8S_POD_NAME".into(), name.into()),
        ("K8S_POD_INFRA_CONTAINER_ID".into(), sandbox.into()),
        ("K8S_POD_UID".into(), uid.into()),
    ]
}

/// The JSON one plugin of `conf` reads on stdin: its own configuration, with the list's name
/// and version, the result so far, and the runtime values its capabilities ask for.
pub fn plugin_input(conf: &NetConf, plugin: &Value, prev: Option<&Value>, a: &Attachment) -> Value {
    let mut input = plugin.clone();
    input["name"] = json!(conf.name);
    input["cniVersion"] = json!(conf.cni_version);
    if let Some(prev) = prev {
        input["prevResult"] = prev.clone();
    }
    let wants = |cap: &str| plugin["capabilities"][cap].as_bool().unwrap_or(false);
    let mut runtime = serde_json::Map::new();
    if wants("portMappings") {
        let ports: Vec<Value> = a
            .ports
            .iter()
            .filter(|p| p.host_port > 0)
            .map(|p| {
                let mut m = json!({
                    "hostPort": p.host_port,
                    "containerPort": p.container_port,
                    "protocol": p.protocol,
                });
                if !p.host_ip.is_empty() {
                    m["hostIP"] = json!(p.host_ip);
                }
                m
            })
            .collect();
        runtime.insert("portMappings".into(), Value::Array(ports));
    }
    if !runtime.is_empty() {
        input["runtimeConfig"] = Value::Object(runtime);
    }
    input
}

/// The pod's addresses in a result, without their prefix lengths: IPv4 first.
pub fn ips(result: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // CNI 0.3 and later: `ips`; 0.2: `ip4` and `ip6`.
    let listed = result["ips"].as_array().cloned().unwrap_or_default();
    let old = ["ip4", "ip6"]
        .iter()
        .filter_map(|k| result.get(*k).cloned())
        .collect::<Vec<_>>();
    for entry in listed.iter().chain(old.iter()) {
        if let Some(addr) = entry["address"].as_str() {
            let ip = addr.split('/').next().unwrap_or(addr).to_string();
            if !ip.is_empty() && !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out.sort_by_key(|ip| ip.contains(':'));
    out
}

/// The plugin program for a `type`, from the plugin folders.
fn program(kind: &str, bin_dirs: &[PathBuf]) -> Result<PathBuf> {
    if kind.contains('/') {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("invalid CNI plugin type {kind:?}"),
            "a type is a program name",
            "fix the CNI configuration",
        ));
    }
    bin_dirs
        .iter()
        .map(|d| d.join(kind))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::MissingRequirement,
                format!("cannot find the CNI plugin {kind}"),
                format!(
                    "it is not in {}",
                    bin_dirs
                        .iter()
                        .map(|d| d.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "install the CNI plugins, or give their folder with --cni-bin-dir",
            )
        })
}

/// Run one plugin. Returns its result (`Null` when it printed nothing).
fn run(command: &str, input: &Value, a: &Attachment, bin_dirs: &[PathBuf]) -> Result<Value> {
    let kind = input["type"].as_str().unwrap_or_default();
    let prog = program(kind, bin_dirs)?;
    let path = std::env::join_paths(bin_dirs).unwrap_or_default();
    let args = a
        .args
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    let fail = |why: String| {
        Error::new(
            ErrorKind::Runtime,
            format!("the CNI plugin {kind} failed ({command})"),
            why,
            "check the CNI network configuration and plugins",
        )
    };
    let mut child = Command::new(&prog)
        .env("CNI_COMMAND", command)
        .env("CNI_CONTAINERID", &a.container_id)
        .env("CNI_NETNS", &a.netns)
        .env("CNI_IFNAME", &a.ifname)
        .env("CNI_ARGS", args)
        .env("CNI_PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| fail(format!("cannot start {}: {e}", prog.display())))?;
    let bytes = serde_json::to_vec(input).unwrap_or_default();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&bytes);
    }
    let out = child.wait_with_output().map_err(|e| fail(e.to_string()))?;
    let parsed: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    if !out.status.success() {
        let msg = parsed["msg"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| {
                let text = String::from_utf8_lossy(&out.stderr);
                let text = text.trim();
                if text.is_empty() {
                    out.status.to_string()
                } else {
                    text.to_string()
                }
            });
        let details = parsed["details"].as_str().unwrap_or_default();
        return Err(fail(if details.is_empty() {
            msg
        } else {
            format!("{msg}: {details}")
        }));
    }
    Ok(parsed)
}

/// Connect: run the list's plugins in order. Returns the last result.
pub fn add(conf: &NetConf, a: &Attachment, bin_dirs: &[PathBuf]) -> Result<Value> {
    let mut prev: Option<Value> = None;
    for plugin in &conf.plugins {
        let input = plugin_input(conf, plugin, prev.as_ref(), a);
        let result = run("ADD", &input, a, bin_dirs)?;
        if !result.is_null() {
            prev = Some(result);
        }
    }
    Ok(prev.unwrap_or(Value::Null))
}

/// Check: every plugin in order, with the result of the ADD.
pub fn check(conf: &NetConf, a: &Attachment, result: &Value, bin_dirs: &[PathBuf]) -> Result<()> {
    for plugin in &conf.plugins {
        let input = plugin_input(conf, plugin, Some(result), a);
        run("CHECK", &input, a, bin_dirs)?;
    }
    Ok(())
}

/// Disconnect: the plugins in reverse order, with the result of the ADD when it is known.
/// Every plugin runs; the first failure is returned.
pub fn del(
    conf: &NetConf,
    a: &Attachment,
    result: Option<&Value>,
    bin_dirs: &[PathBuf],
) -> Result<()> {
    let mut first: Option<Error> = None;
    for plugin in conf.plugins.iter().rev() {
        let input = plugin_input(conf, plugin, result, a);
        if let Err(e) = run("DEL", &input, a, bin_dirs) {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLANNEL: &str = r#"{
      "name": "cbr0",
      "cniVersion": "1.0.0",
      "plugins": [
        {"type": "flannel", "delegate": {"hairpinMode": true, "forceAddress": true, "isDefaultGateway": true}},
        {"type": "portmap", "capabilities": {"portMappings": true}},
        {"type": "bandwidth", "capabilities": {"bandwidth": true}}
      ]
    }"#;

    fn attachment() -> Attachment {
        Attachment {
            container_id: "abc".into(),
            netns: "/run/podbag/cri/ns/abc/net".into(),
            ifname: "eth0".into(),
            args: pod_args("default", "web", "abc", "u1"),
            ports: vec![
                PortMapping {
                    host_port: 8080,
                    container_port: 80,
                    protocol: "tcp".into(),
                    host_ip: String::new(),
                },
                PortMapping {
                    host_port: 0,
                    container_port: 81,
                    protocol: "tcp".into(),
                    host_ip: String::new(),
                },
            ],
        }
    }

    #[test]
    fn lists_and_single_plugins_load() {
        let list = parse(FLANNEL, Path::new("/x/10-flannel.conflist")).unwrap();
        assert_eq!(list.name, "cbr0");
        assert_eq!(list.cni_version, "1.0.0");
        assert_eq!(list.plugins.len(), 3);
        let one = parse(
            r#"{"name":"br","cniVersion":"0.4.0","type":"bridge","bridge":"cni0"}"#,
            Path::new("/x/br.conf"),
        )
        .unwrap();
        assert_eq!(one.plugins.len(), 1);
        assert_eq!(one.plugins[0]["bridge"], "cni0");
        for bad in [
            "{",
            r#"{"name":"x","plugins":[]}"#,
            r#"{"name":"x","plugins":[{"bridge":"b"}]}"#,
            r#"{"plugins":[{"type":"bridge"}]}"#,
        ] {
            assert_eq!(
                parse(bad, Path::new("/x")).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad}"
            );
        }
    }

    #[test]
    fn the_first_file_in_name_order_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            load(tmp.path()).unwrap_err().kind,
            ErrorKind::MissingRequirement
        );
        std::fs::write(
            tmp.path().join("99-last.conf"),
            r#"{"name":"b","type":"bridge"}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("10-flannel.conflist"), FLANNEL).unwrap();
        std::fs::write(tmp.path().join("00-readme.txt"), "not a config").unwrap();
        assert_eq!(load(tmp.path()).unwrap().name, "cbr0");
    }

    #[test]
    fn plugins_get_the_list_name_the_result_so_far_and_their_runtime_values() {
        let conf = parse(FLANNEL, Path::new("/x")).unwrap();
        let a = attachment();
        let first = plugin_input(&conf, &conf.plugins[0], None, &a);
        assert_eq!(first["name"], "cbr0");
        assert_eq!(first["cniVersion"], "1.0.0");
        assert!(first.get("prevResult").is_none());
        assert!(first.get("runtimeConfig").is_none());
        let prev = json!({"ips": [{"address": "10.42.0.5/24"}]});
        let portmap = plugin_input(&conf, &conf.plugins[1], Some(&prev), &a);
        assert_eq!(portmap["prevResult"], prev);
        assert_eq!(
            portmap["runtimeConfig"]["portMappings"],
            json!([{"hostPort": 8080, "containerPort": 80, "protocol": "tcp"}])
        );
        assert_eq!(
            a.args
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(";"),
            "IgnoreUnknown=1;K8S_POD_NAMESPACE=default;K8S_POD_NAME=web;\
             K8S_POD_INFRA_CONTAINER_ID=abc;K8S_POD_UID=u1"
        );
    }

    #[test]
    fn addresses_come_from_the_result() {
        let r = json!({
            "cniVersion": "1.0.0",
            "interfaces": [{"name": "eth0"}],
            "ips": [
                {"address": "fd00::5/64", "interface": 0},
                {"address": "10.42.0.5/24", "gateway": "10.42.0.1", "interface": 0}
            ]
        });
        assert_eq!(ips(&r), ["10.42.0.5", "fd00::5"]);
        assert_eq!(
            ips(&json!({"ip4": {"address": "10.1.0.2/16"}})),
            ["10.1.0.2"]
        );
        assert!(ips(&Value::Null).is_empty());
    }

    /// A plugin chain against shell scripts: order, the result passed on, the environment,
    /// DEL in reverse, and a plugin's error message.
    #[test]
    fn plugins_run_in_order_and_errors_carry_the_message() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let log = tmp.path().join("calls");
        let script = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let l = log.display();
        script(
            "one",
            &format!(
                "cat >/dev/null; echo \"one $CNI_COMMAND $CNI_IFNAME $CNI_NETNS\" >>{l}; \
                 echo '{{\"cniVersion\":\"1.0.0\",\"ips\":[{{\"address\":\"10.0.0.9/24\"}}]}}'"
            ),
        );
        script(
            "two",
            &format!(
                "input=$(cat); echo \"two $CNI_COMMAND\" >>{l}; \
                 case \"$input\" in *10.0.0.9*) ;; *) exit 9;; esac; echo \"$input\" | sed -n 's/.*\"prevResult\":\\({{[^}}]*}}\\]}}\\).*/\\1/p'"
            ),
        );
        script(
            "bad",
            "cat >/dev/null; echo '{\"code\":7,\"msg\":\"no address left\",\"details\":\"pool full\"}'; exit 1",
        );
        let conf = parse(
            r#"{"name":"n","cniVersion":"1.0.0","plugins":[{"type":"one"},{"type":"two"}]}"#,
            Path::new("/x"),
        )
        .unwrap();
        let a = attachment();
        let dirs = [tmp.path().join("missing"), bin.clone()];
        let result = add(&conf, &a, &dirs).unwrap();
        assert_eq!(ips(&result), ["10.0.0.9"]);
        del(&conf, &a, Some(&result), &dirs).unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls,
            "one ADD eth0 /run/podbag/cri/ns/abc/net\ntwo ADD\ntwo DEL\none DEL eth0 /run/podbag/cri/ns/abc/net\n"
        );
        let broken = parse(
            r#"{"name":"n","cniVersion":"1.0.0","plugins":[{"type":"bad"}]}"#,
            Path::new("/x"),
        )
        .unwrap();
        let e = add(&broken, &a, &dirs).unwrap_err();
        assert!(e.why.contains("no address left: pool full"), "{e}");
        let missing = parse(r#"{"name":"n","type":"nope"}"#, Path::new("/x")).unwrap();
        assert_eq!(
            add(&missing, &a, &dirs).unwrap_err().kind,
            ErrorKind::MissingRequirement
        );
    }
}
