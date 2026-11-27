//! Re-create resources through the destination API. Bound filesystem claims also transfer their files.
use super::kubernetes::kubectl_command;
use crate::client::migration::file_manifest;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Seek, Write};
use std::process::{Command, Output, Stdio};

pub struct Scope<'a> {
    pub context: &'a str,
    pub namespace: &'a str,
}
impl Scope<'_> {
    fn command(&self) -> Result<Command, String> {
        if self.context.is_empty() || !dns_name(self.namespace) {
            return Err("Choose a context and valid namespace".into());
        }
        let mut cmd = kubectl_command()?;
        cmd.arg(format!("--context={}", self.context))
            .arg(format!("--namespace={}", self.namespace));
        Ok(cmd)
    }
    fn get(&self, args: &[&str]) -> Result<Value, String> {
        let out = self
            .command()?
            .args(args)
            .args(["--request-timeout=30s", "-o", "json"])
            .output()
            .map_err(ioerr)?;
        serde_json::from_slice(&checked(out)?).map_err(|e| e.to_string())
    }
    fn json(&self, args: &[&str], data: &Value) -> Result<Vec<u8>, String> {
        let mut child = self
            .command()?
            .args(args)
            .arg("--request-timeout=30s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(ioerr)?;
        let mut input = child.stdin.take().unwrap();
        let payload = serde_json::to_vec(data).map_err(|e| e.to_string())?;
        // Reading the API error while writing also handles early rejection of large manifests.
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || input.write_all(&payload));
            let out = checked(child.wait_with_output().map_err(ioerr)?)?;
            writer
                .join()
                .map_err(|_| "kubectl input failed")?
                .map_err(ioerr)?;
            Ok(out)
        })
    }
    fn create(&self, data: &Value) -> Result<Value, String> {
        let out = self.json(&["create", "-f", "-", "-o", "json"], data)?;
        serde_json::from_slice(&out).map_err(|e| e.to_string())
    }
    fn delete_owned(&self, kind: &str, object: &Value) -> Result<(), String> {
        let name = required(object.pointer("/metadata/name"), "name")?;
        let uid = required(object.pointer("/metadata/uid"), "UID")?;
        let url = format!("/api/v1/namespaces/{}/{kind}/{name}", self.namespace);
        self.json(&["delete","--raw",&url,"-f","-"],&json!({"apiVersion":"v1","kind":"DeleteOptions","preconditions":{"uid":uid},"gracePeriodSeconds":0})).map(drop)
    }
}
pub(super) fn dns_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && !s.starts_with('-')
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
}
fn required<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a str, String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Missing {name}"))
}
fn ioerr(e: std::io::Error) -> String {
    e.to_string()
}
fn checked(out: Output) -> Result<Vec<u8>, String> {
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "kubectl {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}
fn drop_fields(v: &mut Value, fields: &[&str]) {
    if let Some(map) = v.as_object_mut() {
        for key in fields {
            map.remove(*key);
        }
    }
}
fn portable(src: &Value, ns: &str) -> Result<Value, String> {
    let meta = &src["metadata"];
    if meta["annotations"][COPY_STATE] == "copying" {
        return Err("Source data migration is incomplete".into());
    }
    if meta["deletionTimestamp"].is_string()
        || meta["ownerReferences"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    {
        return Err(
            "Select the owning workload instead of a managed or terminating resource".into(),
        );
    }
    let mut out = json!({"apiVersion":src["apiVersion"],"kind":src["kind"],"metadata":{"name":meta["name"],"namespace":ns},"spec":src["spec"]});
    if meta["labels"].is_object() {
        out["metadata"]["labels"] = meta["labels"].clone();
    }
    if let Some(mut a) = meta["annotations"].as_object().cloned() {
        a.retain(|k, _| {
            !matches!(
                k.as_str(),
                "kubectl.kubernetes.io/last-applied-configuration"
                    | "deployment.kubernetes.io/revision"
                    | "volume.kubernetes.io/selected-node"
                    | "volume.kubernetes.io/storage-provisioner"
                    | "volume.beta.kubernetes.io/storage-provisioner"
            ) && !k.starts_with("pv.kubernetes.io/")
        });
        out["metadata"]["annotations"] = json!(a);
    }
    let spec = &mut out["spec"];
    match src["kind"].as_str() {
        Some("Service") => {
            if spec["clusterIP"] != "None" {
                drop_fields(spec, &["clusterIP", "clusterIPs"]);
            }
            drop_fields(spec, &["healthCheckNodePort", "ipFamilies"]);
            for port in spec["ports"].as_array_mut().into_iter().flatten() {
                drop_fields(port, &["nodePort"]);
            }
        }
        Some("Pod") => {
            drop_fields(spec, &["nodeName", "ephemeralContainers"]);
            // Admission creates fresh service account token projections in the target cluster.
            let mut generated = std::collections::BTreeSet::new();
            if let Some(vols) = spec["volumes"].as_array_mut() {
                vols.retain(|v| {
                    let name = v["name"].as_str().unwrap_or("");
                    if name.starts_with("kube-api-access-") && v["projected"].is_object() {
                        generated.insert(name.to_owned());
                        false
                    } else {
                        true
                    }
                });
            }
            for key in ["containers", "initContainers"] {
                if let Some(items) = spec.get_mut(key).and_then(Value::as_array_mut) {
                    for c in items {
                        if let Some(m) = c.get_mut("volumeMounts").and_then(Value::as_array_mut) {
                            m.retain(|v| !generated.contains(v["name"].as_str().unwrap_or("")));
                        }
                    }
                }
            }
        }
        Some("Deployment") => {}
        Some("PersistentVolumeClaim") => {
            if spec["volumeMode"] == "Block"
                || !spec["dataSource"].is_null()
                || !spec["dataSourceRef"].is_null()
                || !spec["selector"].is_null()
            {
                return Err("This claim needs a storage-specific migration (raw block, selector or data source)".into());
            }
            drop_fields(spec, &["volumeName"]);
        }
        _ => return Err("Unsupported Kubernetes resource".into()),
    }
    Ok(out)
}
fn helper(ns: &str, claim: &str, ro: bool) -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"generateName":"podbag-copy-","namespace":ns},"spec":{
        "restartPolicy":"Never","automountServiceAccountToken":false,"terminationGracePeriodSeconds":0,
        "containers":[{"name":"copy","image":"docker.io/library/busybox:1.36.1","command":["sleep","86400"],
            "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true},
            "volumeMounts":[{"name":"data","mountPath":"/data","readOnly":ro}]}],
        "volumes":[{"name":"data","persistentVolumeClaim":{"claimName":claim,"readOnly":ro}}]}})
}
fn after<T>(result: Result<T, String>, cleanup: Result<(), String>) -> Result<T, String> {
    match (result, cleanup) {
        (r, Ok(())) => r,
        (Ok(_), Err(e)) => Err(e),
        (Err(e), Err(c)) => Err(format!("{e}. Cleanup: {c}")),
    }
}
fn use_helper<T>(
    scope: &Scope,
    claim: &str,
    ro: bool,
    run: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, String> {
    let pod = scope.create(&helper(scope.namespace, claim, ro))?;
    let name = required(pod.pointer("/metadata/name"), "helper name")?;
    let result = (|| {
        let out = scope
            .command()?
            .args([
                "wait",
                "--for=condition=Ready",
                "--timeout=180s",
                &format!("pod/{name}"),
            ])
            .output()
            .map_err(ioerr)?;
        checked(out)?;
        run(name)
    })();
    after(result, scope.delete_owned("pods", &pod))
}
fn volume_tar(scope: &Scope, pod: &str) -> Result<File, String> {
    let mut f = tempfile::tempfile().map_err(ioerr)?;
    let out = scope
        .command()?
        .args(["exec", pod, "--", "tar", "-C", "/data", "-cf", "-", "."])
        .stdout(f.try_clone().map_err(ioerr)?)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ioerr)?
        .wait_with_output()
        .map_err(ioerr)?;
    checked(out)?;
    f.rewind().map_err(ioerr)?;
    Ok(f)
}
fn copy_claim(
    src: &Scope,
    dst: &Scope,
    name: &str,
    progress: &impl Fn(&str),
) -> Result<(), String> {
    use_helper(src, name, true, |reader| {
        use_helper(dst, name, false, |writer| {
            // A statically provisioned target PV may already contain unrelated files.
            let out = dst
                .command()?
                .args([
                    "exec",
                    writer,
                    "--",
                    "find",
                    "/data",
                    "-mindepth",
                    "1",
                    "-maxdepth",
                    "1",
                    "!",
                    "-name",
                    "lost+found",
                    "-print",
                ])
                .output()
                .map_err(ioerr)?;
            if !checked(out)?.is_empty() {
                return Err("Destination storage is not empty".into());
            }
            progress("files");
            let mut tar = volume_tar(src, reader)?;
            let mut expected = file_manifest(&mut tar)?;
            tar.rewind().map_err(ioerr)?;
            let out = dst
                .command()?
                .args([
                    "exec", "-i", writer, "--", "tar", "-C", "/data", "-xpf", "-",
                ])
                .stdin(tar)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(ioerr)?
                .wait_with_output()
                .map_err(ioerr)?;
            checked(out)?;
            progress("verify");
            let mut actual = file_manifest(volume_tar(dst, writer)?)?;
            // Filesystems may create this empty directory at format time.
            expected.remove(b"lost+found".as_slice());
            actual.remove(b"lost+found".as_slice());
            if actual != expected {
                return Err(format!(
                    "Destination file verification failed for PVC {name}"
                ));
            }
            Ok(())
        })
    })
}
const COPY_STATE: &str = "podbag.io/migration-state";

pub fn migrate(
    src: Scope,
    dst: Scope,
    kind: &str,
    name: &str,
    uid: Option<&str>,
    progress: impl Fn(&str),
) -> Result<(), String> {
    let resource = match kind {
        "pods" => "pods",
        "services" => "services",
        "deployments" => "deployments.apps",
        "claims" => "persistentvolumeclaims",
        // The host names an address of the source cluster's router.
        "routes" => {
            return Err(
                "A Route's host belongs to its cluster. Create a new Route in the destination cluster"
                    .into(),
            );
        }
        _ => return Err("Unsupported Kubernetes resource".into()),
    };
    if !dns_name(name) || src.context == dst.context {
        return Err("Choose a resource and two different cluster contexts".into());
    }
    progress("prepare");
    let object = src.get(&["get", resource, name])?;
    let observed_uid = required(object.pointer("/metadata/uid"), "source UID")?;
    if uid.is_some_and(|uid| uid != observed_uid) {
        return Err("The source object changed. Select it again".into());
    }
    let mut manifest = portable(&object, dst.namespace)?;
    let bound = kind == "claims" && object["status"]["phase"] == "Bound";
    if kind == "claims"
        && !matches!(
            object["status"]["phase"].as_str(),
            Some("Pending" | "Bound")
        )
    {
        return Err("Only pending or bound volume claims can be migrated".into());
    }
    if bound {
        let pods = src.get(&["get", "pods"])?;
        for p in pods["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| !matches!(p["status"]["phase"].as_str(), Some("Succeeded" | "Failed")))
        {
            if p["spec"]["volumes"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| {
                    v["persistentVolumeClaim"]["claimName"] == name
                        && v["persistentVolumeClaim"]["readOnly"] != true
                })
            {
                return Err(format!(
                    "Stop workloads using PVC {name} before copying its data"
                ));
            }
        }
    }
    let template = if kind == "deployments" {
        &manifest["spec"]["template"]["spec"]
    } else {
        &manifest["spec"]
    };
    for vol in template["volumes"].as_array().into_iter().flatten() {
        if let Some(claim) = vol["persistentVolumeClaim"]["claimName"].as_str() {
            let dependency = dst
                .get(&["get", "pvc", claim])
                .map_err(|e| format!("Migrate destination PVC {claim} first: {e}"))?;
            if dependency["metadata"]["annotations"][COPY_STATE].as_str() == Some("copying") {
                return Err(format!(
                    "Data migration is incomplete for destination PVC {claim}"
                ));
            }
        }
        if vol["hostPath"].is_object() {
            return Err("A hostPath volume requires a separate host data transfer".into());
        }
    }
    if bound {
        manifest["metadata"]["annotations"][COPY_STATE] = json!("copying");
    }
    progress("create");
    let created = dst.create(&manifest)?;
    if bound {
        copy_claim(&src, &dst, name, &progress).map_err(|e| {
            format!(
                "{e}. Destination PVC {name} was retained with an incomplete migration annotation"
            )
        })?;
        let uid = required(created.pointer("/metadata/uid"), "created UID")?;
        let patch = json!([
            {"op":"test","path":"/metadata/uid","value":uid},
            {"op":"test","path":"/metadata/annotations/podbag.io~1migration-state","value":"copying"},
            {"op":"remove","path":"/metadata/annotations/podbag.io~1migration-state"}
        ]);
        dst.get(&[
            "patch",
            "pvc",
            name,
            "--type=json",
            "-p",
            &patch.to_string(),
        ])?;
    }
    progress("verify");
    let observed = dst.get(&["get", resource, name])?;
    if required(observed.pointer("/metadata/uid"), "destination UID")?
        != required(created.pointer("/metadata/uid"), "created UID")?
    {
        return Err("Destination object changed during verification".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_is_portable_without_reusing_allocations() {
        let v = json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"web","uid":"old","resourceVersion":"5","labels":{"app":"web"}},
            "spec":{"selector":{"app":"web"},"clusterIP":"10.0.0.3","clusterIPs":["10.0.0.3"],"ports":[{"port":80,"nodePort":30010}]},"status":{"loadBalancer":{}}});
        let result = portable(&v, "new").unwrap();
        assert_eq!(
            result["metadata"],
            json!({"name":"web","namespace":"new","labels":{"app":"web"}})
        );
        assert!(result.get("status").is_none());
        assert_eq!(
            result["spec"],
            json!({"selector":{"app":"web"},"ports":[{"port":80}]})
        );
    }

    #[test]
    fn managed_pods_and_block_claims_require_a_different_transfer() {
        assert!(
            portable(
                &json!({"kind":"Pod","metadata":{"ownerReferences":[{"kind":"ReplicaSet"}]}}),
                "dst"
            )
            .is_err()
        );
        assert!(
            portable(
                &json!({"kind":"PersistentVolumeClaim","spec":{"volumeMode":"Block"}}),
                "dst"
            )
            .is_err()
        );
    }

    #[test]
    fn routes_are_not_copied_between_clusters() {
        let scope = |context| Scope {
            context,
            namespace: "default",
        };
        let error = migrate(scope("a"), scope("b"), "routes", "web", None, |_| {
            panic!("A Route migration must not start")
        })
        .unwrap_err();
        assert!(error.contains("Route's host belongs to its cluster"));
    }

    // The two disposable contexts contain the fixtures described in docs/usage.md's migration section.
    #[test]
    #[ignore = "requires two Kubernetes clusters and the PODBAG_MIGRATION_* fixture variables"]
    fn copies_real_cluster_resource() {
        let var = |key: &str| std::env::var(format!("PODBAG_MIGRATION_{key}")).unwrap();
        let (source, destination, namespace, kind, name) = (
            var("FROM"),
            var("TO"),
            var("NAMESPACE"),
            var("KIND"),
            var("NAME"),
        );
        let src = Scope {
            context: &source,
            namespace: &namespace,
        };
        let actual = if kind == "claims" { "pvc" } else { &kind };
        let object = src.get(&["get", actual, &name]).unwrap();
        let uid = object["metadata"]["uid"].as_str().unwrap();
        migrate(
            src,
            Scope {
                context: &destination,
                namespace: &namespace,
            },
            &kind,
            &name,
            Some(uid),
            |s| println!("{s}"),
        )
        .unwrap();
        let target = Scope {
            context: &destination,
            namespace: &namespace,
        }
        .get(&["get", actual, &name])
        .unwrap();
        assert_ne!(object["metadata"]["uid"], target["metadata"]["uid"]);
        assert_eq!(object["metadata"]["labels"], target["metadata"]["labels"]);
    }
}
