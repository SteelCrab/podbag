use podbag_desktop_core::kubernetes::{self, Resource, Snapshot};

#[tauri::command(async)]
pub fn kube_contexts() -> Result<Vec<String>, String> {
    kubernetes::contexts()
}

#[tauri::command(async)]
pub fn kube_list(
    kind: Resource,
    all_namespaces: bool,
    context: Option<String>,
) -> Result<Snapshot, String> {
    kubernetes::list(kind, all_namespaces, context)
}

#[tauri::command(async)]
pub fn kube_counts(
    context: Option<String>,
    all_namespaces: bool,
) -> Result<std::collections::BTreeMap<String, usize>, String> {
    kubernetes::counts(context, all_namespaces)
}
