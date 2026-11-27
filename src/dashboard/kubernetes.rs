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
}

impl Resource {
    pub fn key(self) -> &'static str {
        match self {
            Self::Pods => "pods",
            Self::Services => "services",
            Self::Deployments => "deployments",
            Self::Claims => "claims",
        }
    }

    fn api_name(self) -> &'static str {
        match self {
            Self::Deployments => "deployments.apps",
            Self::Claims => "persistentvolumeclaims",
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
        for kind in [
            Resource::Pods,
            Resource::Services,
            Resource::Deployments,
            Resource::Claims,
        ] {
            assert_eq!(
                kube_list(kind, false, Some(context.clone()))
                    .unwrap()
                    .context,
                context
            );
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

pub fn kube_counts(
    context: Option<String>,
    all_namespaces: bool,
) -> Result<std::collections::BTreeMap<String, usize>, String> {
    let mut args = vec![
        "get",
        "pods,services,deployments.apps,persistentvolumeclaims",
        "--request-timeout=10s",
        "--no-headers",
        "-o",
        "custom-columns=KIND:.kind",
    ];
    if let Some(ref c) = context {
        args.extend(["--context", c]);
    }
    if all_namespaces {
        args.push("--all-namespaces");
    }
    let bytes = kubectl(&args)?;
    let mut counts = std::collections::BTreeMap::from([
        ("pods".into(), 0),
        ("services".into(), 0),
        ("deployments".into(), 0),
        ("claims".into(), 0),
    ]);
    for line in String::from_utf8_lossy(&bytes).lines() {
        let key = match line.trim() {
            "Pod" => "pods",
            "Service" => "services",
            "Deployment" => "deployments",
            "PersistentVolumeClaim" => "claims",
            _ => continue,
        };
        *counts.get_mut(key).unwrap() += 1;
    }
    Ok(counts)
}
