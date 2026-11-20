//! Read Pods and Services from the user's active kubectl context.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Command;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resource {
    Pods,
    Services,
}

#[derive(Serialize)]
pub struct Snapshot {
    context: String,
    namespace: String,
    items: Vec<Value>,
}

fn kubectl(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("kubectl");
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
    let out = cmd.args(args).output().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => "kubectl-missing".to_owned(),
        _ => format!("kubectl: {err}"),
    })?;
    if !out.status.success() {
        let error = String::from_utf8_lossy(&out.stderr);
        return Err(format!("kubectl ({}): {}", out.status, error.trim()));
    }
    Ok(out.stdout)
}

#[tauri::command(async)]
pub fn kube_list(kind: Resource, all_namespaces: bool) -> Result<Snapshot, String> {
    list(kind, all_namespaces, kubectl)
}

fn list(
    kind: Resource,
    all_namespaces: bool,
    run: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
) -> Result<Snapshot, String> {
    // Request only the selected context entry. It contains references, not credentials.
    let config = run(&[
        "config",
        "view",
        "--minify",
        "-o",
        "jsonpath={.contexts[0]}",
    ])?;
    let config: Value =
        serde_json::from_slice(&config).map_err(|e| format!("kubectl config: {e}"))?;
    let context = config["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("No current kubectl context")?;
    let namespace = config["context"]["namespace"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    let resource = match kind {
        Resource::Pods => "pods",
        Resource::Services => "services",
    };
    let mut args = vec![
        "get",
        resource,
        "--context",
        context,
        "--request-timeout=10s",
        "-o",
        "json",
    ];
    if all_namespaces {
        args.push("--all-namespaces");
    } else {
        args.extend(["--namespace", namespace]);
    }
    #[derive(Deserialize)]
    struct ResourceList {
        items: Vec<Value>,
    }
    let bytes = run(&args).map_err(|error| format!("{context}: {error}"))?;
    let list: ResourceList =
        serde_json::from_slice(&bytes).map_err(|e| format!("kubectl get {resource}: {e}"))?;
    Ok(Snapshot {
        context: context.into(),
        namespace: namespace.into(),
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
            let snapshot = list(kind, all, |args| {
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
    fn distinguishes_empty_lists_from_failures() {
        for (response, ok) in [(r#"{"items":[]}"#, true), ("{}", false), ("invalid", false)] {
            let result = list(Resource::Pods, false, |args| {
                Ok(if args[0] == "config" {
                    br#"{"name":"dev","context":{}}"#.to_vec()
                } else {
                    response.as_bytes().to_vec()
                })
            });
            assert_eq!(result.is_ok(), ok);
        }
        let denied = list(Resource::Services, true, |args| {
            if args[0] == "config" {
                return Ok(br#"{"name":"dev","context":{}}"#.to_vec());
            }
            Err("Forbidden".into())
        });
        assert_eq!(denied.err().as_deref(), Some("dev: Forbidden"));
    }
}
