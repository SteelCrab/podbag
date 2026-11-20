//! Operations shared by the terminal and desktop dashboards.
pub mod cli;
pub mod cluster_migration;
pub mod kubernetes;
pub mod lab;
pub mod tutorial;

use crate::client::{Choice, Target};
use std::path::Path;

/// Running local engine presets, with socket aliases shown only once.
pub fn local_engines(podbag: &Path) -> Vec<Target> {
    let mut engines: Vec<Target> = Vec::new();
    for choice in [Choice::Podbag, Choice::Podman, Choice::Kiri, Choice::Docker] {
        #[cfg(target_os = "macos")]
        if choice == Choice::Podbag && crate::macos::cli::vm_status().ok() != Some("running") {
            continue;
        }
        #[cfg(windows)]
        if choice == Choice::Podbag
            && cli::vm_status(podbag)
                .ok()
                .is_none_or(|state| state["state"] != "running")
        {
            continue;
        }
        if let Ok(target) = Target::find(Some(&choice), podbag)
            && !engines
                .iter()
                .any(|t| t.endpoint.same_target(&target.endpoint))
            && target.endpoint.listening()
        {
            engines.push(target);
        }
    }
    engines
}

#[derive(serde::Serialize)]
pub struct ImportableImage {
    pub tag: String,
    pub size: i64,
    pub present: bool,
}

/// The tagged images offered by the import picker, in tag order.
pub fn importable_images(
    source: &crate::client::Client,
    destination: &crate::client::Client,
) -> crate::client::Result<Vec<ImportableImage>> {
    let images = source.images()?;
    let present: std::collections::HashSet<_> = destination
        .images()?
        .into_iter()
        .flat_map(|image| image.repo_tags)
        .collect();
    let mut tags = std::collections::BTreeMap::new();
    for image in images {
        for tag in image.repo_tags.into_iter().filter(|t| t != "<none>:<none>") {
            tags.insert(tag, image.size);
        }
    }
    Ok(tags
        .into_iter()
        .map(|(tag, size)| ImportableImage {
            present: present.contains(&tag),
            tag,
            size,
        })
        .collect())
}
