//! Workloads and storage of a kubeconfig context, through `kubectl`.

use podbag::dashboard::kubernetes;
use podbag::dashboard::local_kubernetes;
use std::collections::BTreeMap;

pub use podbag::dashboard::kubernetes::{Resource, Snapshot, route_url};
pub use podbag::dashboard::local_kubernetes::{LocalCluster, Provider};

pub enum LocalAction {
    Create {
        version: Option<String>,
        nodes: u8,
        managed_vm: bool,
    },
    Reset(String),
    Remove(String),
}

pub fn local_status() -> Result<Option<LocalCluster>, String> {
    local_kubernetes::status()
}

pub fn local_change(action: LocalAction) -> Result<Option<LocalCluster>, String> {
    match action {
        LocalAction::Create {
            version,
            nodes,
            managed_vm: true,
        } => local_kubernetes::create_on_vm(version, nodes).map(Some),
        LocalAction::Create {
            version,
            nodes,
            managed_vm: false,
        } => local_kubernetes::create(version, nodes).map(Some),
        LocalAction::Reset(context) => local_kubernetes::reset_context(&context).map(Some),
        LocalAction::Remove(context) => local_kubernetes::remove_context(&context).map(|()| None),
    }
}

/// The contexts of the kubeconfig.
pub fn contexts() -> Result<Vec<String>, String> {
    kubernetes::kube_contexts()
}

/// The objects of `kind` in the context (None: the current one).
pub fn list(
    kind: Resource,
    all_namespaces: bool,
    context: Option<String>,
) -> Result<Snapshot, String> {
    kubernetes::kube_list(kind, all_namespaces, context)
}

/// How many objects there are of each kind (`pods`, `services`, `deployments`, `claims`,
/// and `routes` on OpenShift).
pub fn counts(
    context: Option<String>,
    all_namespaces: bool,
) -> Result<BTreeMap<String, usize>, String> {
    kubernetes::kube_counts(context, all_namespaces)
}

/// Whether the context (None: the current one) serves OpenShift Routes.
pub fn openshift(context: Option<&str>) -> Result<bool, String> {
    kubernetes::kube_openshift(context)
}

/// Open a Route's address in the browser. The address is read from the cluster again, so
/// only a Route's own host and path open.
pub fn open_route(context: &str, namespace: &str, name: &str) -> Result<(), String> {
    let route = kubernetes::kube_inspect(Resource::Routes, name, Some(context), Some(namespace))?;
    open::that_detached(route_url(&route)?).map_err(|e| e.to_string())
}
