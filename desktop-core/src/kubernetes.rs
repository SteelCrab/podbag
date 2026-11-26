//! Workloads and storage of a kubeconfig context, through `kubectl`.

use podbag::dashboard::kubernetes;
use std::collections::BTreeMap;

pub use podbag::dashboard::kubernetes::{Resource, Snapshot};

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

/// How many objects there are of each kind (`pods`, `services`, `deployments`, `claims`).
pub fn counts(
    context: Option<String>,
    all_namespaces: bool,
) -> Result<BTreeMap<String, usize>, String> {
    kubernetes::kube_counts(context, all_namespaces)
}
