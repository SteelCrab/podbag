use podbag::dashboard::kubernetes::{self, Resource, Snapshot};

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
