use podbag::dashboard::kubernetes::{self, Resource, Snapshot};
use podbag::dashboard::local_kubernetes::{self, LocalCluster};

#[tauri::command(async)]
pub fn local_kube_status() -> Result<Option<LocalCluster>, String> {
    local_kubernetes::status()
}

#[tauri::command(async)]
pub fn local_kube_create(version: Option<String>, nodes: u8) -> Result<LocalCluster, String> {
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
