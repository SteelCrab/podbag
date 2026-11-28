use podbag::dashboard::kubernetes::{self, Resource, Snapshot};
use podbag::dashboard::local_kubernetes::{self, LocalCluster};

#[tauri::command(async)]
pub fn local_kube_status() -> Result<Option<LocalCluster>, String> {
    local_kubernetes::status()
}

#[tauri::command(async)]
pub fn local_kube_create(
    version: Option<String>,
    nodes: u8,
    managed_vm: Option<bool>,
) -> Result<LocalCluster, String> {
    if managed_vm == Some(true) {
        return local_kubernetes::create_on_vm(version, nodes);
    }
    local_kubernetes::create(version, nodes)
}

#[tauri::command(async)]
pub fn local_kube_reset() -> Result<LocalCluster, String> {
    local_kubernetes::reset()
}

#[tauri::command(async)]
pub fn local_kube_remove() -> Result<(), String> {
    local_kubernetes::remove()
}

#[tauri::command(async)]
pub fn kube_contexts() -> Result<Vec<String>, String> {
    kubernetes::kube_contexts()
}

#[tauri::command(async)]
pub fn kube_list(
    kind: Resource,
    all_namespaces: bool,
    context: Option<String>,
) -> Result<Snapshot, String> {
    kubernetes::kube_list(kind, all_namespaces, context)
}

#[tauri::command(async)]
pub fn kube_counts(
    context: Option<String>,
    all_namespaces: bool,
) -> Result<std::collections::BTreeMap<String, usize>, String> {
    kubernetes::kube_counts(context, all_namespaces)
}

#[tauri::command(async)]
pub fn kube_openshift(context: Option<String>) -> Result<bool, String> {
    kubernetes::kube_openshift(context.as_deref())
}

/// Open a Route's address in the browser. The address comes from the cluster, not the page.
#[tauri::command(async)]
pub fn kube_open_route(context: String, namespace: String, name: String) -> Result<(), String> {
    let route =
        kubernetes::kube_inspect(Resource::Routes, &name, Some(&context), Some(&namespace))?;
    open::that_detached(kubernetes::route_url(&route)?).map_err(|e| e.to_string())
}
