//! Read workloads and storage from a kubeconfig context.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Command;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resource {
    Pods,
    Services,
    Deployments,
    Claims,
    /// OpenShift only: the public address of a Service.
    Routes,
}

impl Resource {
    pub fn key(self) -> &'static str {
        match self {
            Self::Pods => "pods",
            Self::Services => "services",
            Self::Deployments => "deployments",
            Self::Claims => "claims",
            Self::Routes => "routes",
        }
    }

    fn api_name(self) -> &'static str {
        match self {
            Self::Deployments => "deployments.apps",
            Self::Claims => "persistentvolumeclaims",
            Self::Routes => "routes.route.openshift.io",
            _ => self.key(),
        }
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    pub context: String,
    pub namespace: String,
    pub items: Vec<Value>,
}

pub(super) fn kubectl_command() -> Result<Command, String> {
    let mut command = host_command("kubectl")?;
    if let Some(config) = super::local_kubernetes::kubeconfig() {
        let mut paths: Vec<_> = std::env::var_os("KUBECONFIG")
            .filter(|value| !value.is_empty())
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_else(|| {
                std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                    .map(|home| std::path::PathBuf::from(home).join(".kube").join("config"))
                    .into_iter()
                    .collect()
            });
        paths.push(config);
        command.env(
            "KUBECONFIG",
            std::env::join_paths(paths).map_err(|e| e.to_string())?,
        );
    }
    Ok(command)
}

pub(super) fn host_command(program: &str) -> Result<Command, String> {
    let cmd = Command::new(program);
    #[cfg(any(target_os = "macos", windows))]
    let mut cmd = cmd;
    // Apps launched from Finder do not inherit the user's shell PATH.
    #[cfg(target_os = "macos")]
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs: Vec<_> = std::env::split_paths(&path).collect();
        dirs.extend(["/opt/homebrew/bin".into(), "/usr/local/bin".into()]);
        cmd.env(
            "PATH",
            std::env::join_paths(dirs).map_err(|e| e.to_string())?,
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    Ok(cmd)
}

fn kubectl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = kubectl_command()?
        .args(args)
        .output()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => "kubectl-missing".to_owned(),
            _ => format!("kubectl: {err}"),
        })?;
    if !out.status.success() {
        let error = String::from_utf8_lossy(&out.stderr);
        return Err(format!("kubectl ({}): {}", out.status, error.trim()));
    }
    Ok(out.stdout)
}

pub fn kube_contexts() -> Result<Vec<String>, String> {
    let names = kubectl(&["config", "get-contexts", "-o", "name"])?;
    Ok(String::from_utf8_lossy(&names)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Whether the context's API serves OpenShift Routes.
pub fn kube_openshift(context: Option<&str>) -> Result<bool, String> {
    openshift(context, kubectl)
}

fn openshift(
    context: Option<&str>,
    run: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
) -> Result<bool, String> {
    // `api-versions` lists every API group, so "not OpenShift" (a missing group) and
    // "cluster unreachable" (an error) stay apart. kubectl skips its discovery cache here.
    let mut args = vec!["api-versions", "--request-timeout=10s"];
    if let Some(context) = context {
        args.extend(["--context", context]);
    }
    let versions = run(&args)?;
    Ok(String::from_utf8_lossy(&versions)
        .lines()
        .any(|line| line.trim() == "route.openshift.io/v1"))
}

/// The address a Route publishes: https when it has TLS settings, http otherwise.
pub fn route_url(route: &Value) -> Result<String, String> {
    let spec = &route["spec"];
    let host = spec["host"]
        .as_str()
        .filter(|host| super::cluster_migration::dns_name(host))
        .ok_or("The Route has no valid host")?;
    let path = spec["path"].as_str().unwrap_or("");
    if !path.is_empty() && (!path.starts_with('/') || path.bytes().any(|b| b <= b' ' || b == 0x7f))
    {
        return Err("The Route has an invalid path".into());
    }
    let scheme = if spec["tls"].is_object() {
        "https"
    } else {
        "http"
    };
    Ok(format!("{scheme}://{host}{path}"))
}

pub fn kube_list(
    kind: Resource,
    all_namespaces: bool,
    context: Option<String>,
) -> Result<Snapshot, String> {
    list(kind, all_namespaces, context.as_deref(), kubectl)
}

/// Resolve the current or named context once before issuing a resource request.
pub fn kube_scope(
    context: Option<&str>,
    namespace: Option<&str>,
) -> Result<(String, String), String> {
    scope(context, namespace, kubectl)
}

fn scope(
    selected: Option<&str>,
    namespace: Option<&str>,
    run: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
) -> Result<(String, String), String> {
    // Request only the context entry. It contains references, not credentials.
    let mut args = vec![
        "config",
        "view",
        "--minify",
        "-o",
        "jsonpath={.contexts[0]}",
    ];
    if let Some(context) = selected {
        args.extend(["--context", context]);
    }
    let config = run(&args)?;
    let config: Value =
        serde_json::from_slice(&config).map_err(|e| format!("kubectl config: {e}"))?;
    let context = config["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("No current kubectl context")?;
    if selected.is_some_and(|name| name != context) {
        return Err("kubectl returned a different context".into());
    }
    let namespace = namespace
        .or_else(|| {
            config["context"]["namespace"]
                .as_str()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("default");
    Ok((context.into(), namespace.into()))
}

pub fn kube_list_in(
    kind: Resource,
    all_namespaces: bool,
    context: Option<&str>,
    namespace: Option<&str>,
) -> Result<Snapshot, String> {
    list_in(kind, all_namespaces, context, namespace, kubectl)
}

pub fn kube_inspect(
    kind: Resource,
    name: &str,
    context: Option<&str>,
    namespace: Option<&str>,
) -> Result<Value, String> {
    if !super::cluster_migration::dns_name(name) {
        return Err("Enter a Kubernetes resource name".into());
    }
    let (context, namespace) = kube_scope(context, namespace)?;
    let bytes = kubectl(&[
        "get",
        kind.api_name(),
        name,
        "--context",
        &context,
        "--request-timeout=10s",
        "-o",
        "json",
        "--namespace",
        &namespace,
    ])?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn list(
    kind: Resource,
    all_namespaces: bool,
    selected: Option<&str>,
    run: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
) -> Result<Snapshot, String> {
    list_in(kind, all_namespaces, selected, None, run)
}

fn list_in(
    kind: Resource,
    all_namespaces: bool,
    selected: Option<&str>,
    namespace: Option<&str>,
    run: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
) -> Result<Snapshot, String> {
    let (context, namespace) = scope(selected, namespace, &run)?;
    let resource = kind.api_name();
    let mut args = vec![
        "get",
        resource,
        "--context",
        &context,
        "--request-timeout=10s",
        "-o",
        "json",
    ];
    if all_namespaces {
        args.push("--all-namespaces");
    } else {
        args.extend(["--namespace", &namespace]);
    }
    #[derive(Deserialize)]
    struct ResourceList {
        items: Vec<Value>,
    }
    let bytes = run(&args).map_err(|error| format!("{context}: {error}"))?;
    let list: ResourceList =
        serde_json::from_slice(&bytes).map_err(|e| format!("kubectl get {resource}: {e}"))?;
    Ok(Snapshot {
        context,
        namespace,
        items: list.items,
    })
}

pub fn kube_counts(
    context: Option<String>,
    all_namespaces: bool,
) -> Result<std::collections::BTreeMap<String, usize>, String> {
    counts(context.as_deref(), all_namespaces, kubectl)
}

/// Counts per kind. `routes` is present only on OpenShift: other clusters reject that type.
fn counts(
    context: Option<&str>,
    all_namespaces: bool,
    run: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
) -> Result<std::collections::BTreeMap<String, usize>, String> {
    let openshift = openshift(context, &run)?;
    let resources = if openshift {
        "pods,services,deployments.apps,persistentvolumeclaims,routes.route.openshift.io"
    } else {
        "pods,services,deployments.apps,persistentvolumeclaims"
    };
    let mut args = vec![
        "get",
        resources,
        "--request-timeout=10s",
        "--no-headers",
        "-o",
        "custom-columns=KIND:.kind",
    ];
    if let Some(c) = context {
        args.extend(["--context", c]);
    }
    if all_namespaces {
        args.push("--all-namespaces");
    }
    let bytes = run(&args)?;
    let mut counts = std::collections::BTreeMap::from([
        ("pods".into(), 0),
        ("services".into(), 0),
        ("deployments".into(), 0),
        ("claims".into(), 0),
    ]);
    if openshift {
        counts.insert("routes".into(), 0);
    }
    for line in String::from_utf8_lossy(&bytes).lines() {
        let key = match line.trim() {
            "Pod" => "pods",
            "Service" => "services",
            "Deployment" => "deployments",
            "PersistentVolumeClaim" => "claims",
            "Route" if openshift => "routes",
            _ => continue,
        };
        *counts.get_mut(key).unwrap() += 1;
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_requests_to_the_context_and_resource() {
        for (kind, resource, namespace, all) in [
            (Resource::Pods, "pods", "", false),
            (Resource::Services, "services", "team", false),
            (Resource::Services, "services", "team", true),
        ] {
            let expected_ns = if namespace.is_empty() {
                "default"
            } else {
                namespace
            };
            let snapshot = list(kind, all, None, |args| {
                if args[0] == "config" {
                    assert_eq!(
                        args,
                        [
                            "config",
                            "view",
                            "--minify",
                            "-o",
                            "jsonpath={.contexts[0]}"
                        ]
                    );
                    let config =
                        serde_json::json!({"name": "dev", "context": {"namespace": namespace}});
                    return Ok(config.to_string().into_bytes());
                }
                assert_eq!(
                    &args[..7],
                    [
                        "get",
                        resource,
                        "--context",
                        "dev",
                        "--request-timeout=10s",
                        "-o",
                        "json"
                    ]
                );
                if all {
                    assert_eq!(&args[7..], ["--all-namespaces"]);
                } else {
                    assert_eq!(&args[7..], ["--namespace", expected_ns]);
                }
                Ok(br#"{"items":[{"metadata":{"name":"web"}}]}"#.to_vec())
            })
            .unwrap();
            assert_eq!(snapshot.context, "dev");
            assert_eq!(snapshot.namespace, expected_ns);
            assert_eq!(snapshot.items[0]["metadata"]["name"], "web");
        }
    }

    #[test]
    fn explicit_context_scopes_storage_and_workloads() {
        for (kind, name) in [
            (Resource::Claims, "persistentvolumeclaims"),
            (Resource::Deployments, "deployments.apps"),
        ] {
            list(kind, false, Some("other"), |args| {
                if args[0] == "config" {
                    assert_eq!(&args[5..], ["--context", "other"]);
                    return Ok(br#"{"name":"other","context":{"namespace":"storage"}}"#.to_vec());
                }
                assert_eq!(
                    args,
                    [
                        "get",
                        name,
                        "--context",
                        "other",
                        "--request-timeout=10s",
                        "-o",
                        "json",
                        "--namespace",
                        "storage"
                    ]
                );
                Ok(br#"{"items":[]}"#.to_vec())
            })
            .unwrap();
        }
    }

    #[test]
    #[ignore = "requires KUBECONFIG and PODBAG_TEST_KUBE_CONTEXT for a test cluster"]
    fn reads_a_configured_cluster() {
        let context = std::env::var("PODBAG_TEST_KUBE_CONTEXT").unwrap();
        assert!(kube_contexts().unwrap().contains(&context));
        let mut kinds = vec![
            Resource::Pods,
            Resource::Services,
            Resource::Deployments,
            Resource::Claims,
        ];
        let openshift = kube_openshift(Some(&context)).unwrap();
        if openshift {
            kinds.push(Resource::Routes);
        }
        let counts = kube_counts(Some(context.clone()), false).unwrap();
        assert_eq!(counts.contains_key("routes"), openshift);
        for kind in kinds {
            assert_eq!(
                kube_list(kind, false, Some(context.clone()))
                    .unwrap()
                    .context,
                context
            );
        }
    }

    #[test]
    fn detects_openshift_by_the_route_api_group() {
        for (output, expected) in [
            ("apps/v1\nroute.openshift.io/v1\nv1\n", true),
            ("apps/v1\nnetworking.k8s.io/v1\nv1\n", false),
            ("route.openshift.io/v1beta1\n", false),
        ] {
            let openshift = openshift(Some("dev"), |args| {
                assert_eq!(
                    args,
                    ["api-versions", "--request-timeout=10s", "--context", "dev"]
                );
                Ok(output.as_bytes().to_vec())
            });
            assert_eq!(openshift, Ok(expected));
        }
        let current = openshift(None, |args| {
            assert_eq!(args, ["api-versions", "--request-timeout=10s"]);
            Err("kubectl (exit status: 1): Unable to connect to the server".into())
        });
        assert!(current.unwrap_err().contains("Unable to connect"));
    }

    #[test]
    fn lists_routes_by_their_full_api_name() {
        let snapshot = list(Resource::Routes, false, Some("ocp"), |args| {
            if args[0] == "config" {
                return Ok(br#"{"name":"ocp","context":{"namespace":"shop"}}"#.to_vec());
            }
            assert_eq!(
                args,
                [
                    "get",
                    "routes.route.openshift.io",
                    "--context",
                    "ocp",
                    "--request-timeout=10s",
                    "-o",
                    "json",
                    "--namespace",
                    "shop"
                ]
            );
            Ok(br#"{"items":[{"kind":"Route","metadata":{"name":"web"}}]}"#.to_vec())
        })
        .unwrap();
        assert_eq!(snapshot.items[0]["kind"], "Route");
        assert_eq!(Resource::Routes.key(), "routes");
    }

    #[test]
    fn counts_routes_only_on_openshift() {
        let kinds = "Pod\nPod\nService\nDeployment\nRoute\nRoute\n";
        for (versions, resources, routes) in [
            (
                "route.openshift.io/v1\nv1\n",
                "pods,services,deployments.apps,persistentvolumeclaims,routes.route.openshift.io",
                Some(2),
            ),
            (
                "v1\n",
                "pods,services,deployments.apps,persistentvolumeclaims",
                None,
            ),
        ] {
            let counts = counts(Some("dev"), true, |args| {
                if args[0] == "api-versions" {
                    return Ok(versions.as_bytes().to_vec());
                }
                assert_eq!(
                    args,
                    [
                        "get",
                        resources,
                        "--request-timeout=10s",
                        "--no-headers",
                        "-o",
                        "custom-columns=KIND:.kind",
                        "--context",
                        "dev",
                        "--all-namespaces"
                    ]
                );
                Ok(kinds.as_bytes().to_vec())
            })
            .unwrap();
            assert_eq!(counts["pods"], 2);
            assert_eq!(counts["claims"], 0);
            assert_eq!(counts.get("routes").copied(), routes);
        }
        let unreachable = counts(None, false, |args| {
            assert_eq!(args[0], "api-versions");
            Err("Unable to connect".into())
        });
        assert_eq!(unreachable.unwrap_err(), "Unable to connect");
    }

    #[test]
    fn builds_the_route_address_from_its_host_path_and_tls() {
        let url = |spec: Value| route_url(&serde_json::json!({ "spec": spec }));
        assert_eq!(
            url(
                serde_json::json!({"host": "shop.apps.example.com", "path": "/cart", "tls": {"termination": "edge"}})
            ),
            Ok("https://shop.apps.example.com/cart".into())
        );
        assert_eq!(
            url(serde_json::json!({"host": "shop.apps.example.com"})),
            Ok("http://shop.apps.example.com".into())
        );
        for spec in [
            serde_json::json!({}),
            serde_json::json!({"host": "javascript:alert(1)"}),
            serde_json::json!({"host": "shop.example.com", "path": "cart"}),
            serde_json::json!({"host": "shop.example.com", "path": "/a b"}),
        ] {
            assert!(url(spec).is_err());
        }
    }

    #[test]
    fn distinguishes_empty_lists_from_failures() {
        for (response, ok) in [(r#"{"items":[]}"#, true), ("{}", false), ("invalid", false)] {
            let result = list(Resource::Pods, false, None, |args| {
                Ok(if args[0] == "config" {
                    br#"{"name":"dev","context":{}}"#.to_vec()
                } else {
                    response.as_bytes().to_vec()
                })
            });
            assert_eq!(result.is_ok(), ok);
        }
        let denied = list(Resource::Services, true, None, |args| {
            if args[0] == "config" {
                return Ok(br#"{"name":"dev","context":{}}"#.to_vec());
            }
            Err("Forbidden".into())
        });
        assert_eq!(denied.err().as_deref(), Some("dev: Forbidden"));
    }
}
