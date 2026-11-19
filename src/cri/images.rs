//! CRI images from podbag's image store: one CRI image per digest, with every stored name
//! that points to it as a tag.

use super::pb;
use crate::image::Image;
use crate::image::reference::{Reference, is_digest};

/// CRI images for the store's names, in the order of their first name.
pub fn group(names: &[Image]) -> Vec<pb::Image> {
    let mut out: Vec<pb::Image> = Vec::new();
    for image in names {
        let i = match out.iter().position(|c| c.id == image.digest) {
            Some(i) => i,
            None => {
                let (uid, username) = user(image.config.user.as_deref());
                out.push(pb::Image {
                    id: image.digest.clone(),
                    size: image.size,
                    uid,
                    username,
                    spec: Some(pb::ImageSpec {
                        image: image.digest.clone(),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                out.len() - 1
            }
        };
        let cri = &mut out[i];
        if !cri.repo_tags.contains(&image.name) {
            cri.repo_tags.push(image.name.clone());
        }
        let repo_digest = format!("{}@{}", repository(&image.name), image.digest);
        if !cri.repo_digests.contains(&repo_digest) {
            cri.repo_digests.push(repo_digest);
        }
    }
    out
}

/// The image a kubelet means by `wanted`: an image ID (or its prefix), a name with or
/// without a tag or registry, or a name with a digest.
pub fn find<'a>(images: &'a [pb::Image], wanted: &str) -> Option<&'a pb::Image> {
    if wanted.is_empty() {
        return None;
    }
    if is_digest(wanted) || (wanted.starts_with("sha256:") && wanted.len() >= 12) {
        return images.iter().find(|i| i.id.starts_with(wanted));
    }
    if let Some(full) = full_tag(wanted) {
        return images.iter().find(|i| i.repo_tags.contains(&full));
    }
    let reference = Reference::parse(wanted).ok()?;
    let digest = reference.digest.as_ref()?;
    let repo = format!("{}/{}", reference.registry, reference.repository);
    images
        .iter()
        .find(|i| i.repo_digests.contains(&format!("{repo}@{digest}")) || &i.id == digest)
}

/// The full tagged name for a name without a digest (`nginx` →
/// `docker.io/library/nginx:latest`); `None` for IDs and names with a digest.
pub fn full_tag(wanted: &str) -> Option<String> {
    if wanted.starts_with("sha256:") {
        return None;
    }
    let reference = Reference::parse(wanted).ok()?;
    match (&reference.digest, &reference.tag) {
        (Some(_), _) => None,
        (None, Some(_)) => Some(reference.name()),
        (None, None) => Some(format!("{}:latest", reference.name())),
    }
}

/// The name without its tag: `docker.io/library/nginx:1.25` → `docker.io/library/nginx`.
fn repository(name: &str) -> &str {
    let name = name.split('@').next().unwrap_or(name);
    match name.rfind(':') {
        Some(colon) if !name[colon..].contains('/') => &name[..colon],
        _ => name,
    }
}

/// The image's user as the kubelet wants it (as containerd reports it): a numeric user is a
/// uid, anything else a user name; a "user:group" counts by its user part.
pub fn user(user: Option<&str>) -> (Option<pb::Int64Value>, String) {
    let Some(user) = user.filter(|u| !u.is_empty()) else {
        return (None, String::new());
    };
    let user = user.split(':').next().unwrap_or(user);
    match user.parse::<i64>() {
        Ok(uid) => (Some(pb::Int64Value { value: uid }), String::new()),
        Err(_) => (None, user.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ImageConfig;

    fn image(name: &str, digest: &str, user: Option<&str>) -> Image {
        Image {
            name: name.into(),
            digest: digest.into(),
            size: 7,
            config: ImageConfig {
                user: user.map(Into::into),
                ..Default::default()
            },
        }
    }

    const A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn groups_names_by_digest() {
        let images = group(&[
            image("docker.io/library/nginx:1.25", A, Some("101")),
            image("registry.k8s.io/pause:3.10", B, None),
            image("docker.io/library/nginx:latest", A, Some("101")),
            image("localhost:5000/app:v1", B, None),
        ]);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].id, A);
        assert_eq!(
            images[0].repo_tags,
            [
                "docker.io/library/nginx:1.25",
                "docker.io/library/nginx:latest"
            ]
        );
        assert_eq!(
            images[0].repo_digests,
            [format!("docker.io/library/nginx@{A}")]
        );
        assert_eq!(images[0].uid, Some(pb::Int64Value { value: 101 }));
        assert_eq!(
            images[1].repo_digests,
            [
                format!("registry.k8s.io/pause@{B}"),
                format!("localhost:5000/app@{B}")
            ]
        );
    }

    #[test]
    fn finds_images_the_way_a_kubelet_names_them() {
        let images = group(&[
            image("docker.io/library/nginx:latest", A, None),
            image("registry.k8s.io/pause:3.10", B, None),
        ]);
        let id = |w: &str| find(&images, w).map(|i| i.id.as_str());
        assert_eq!(id("nginx"), Some(A));
        assert_eq!(id("nginx:latest"), Some(A));
        assert_eq!(id("docker.io/library/nginx:latest"), Some(A));
        assert_eq!(id(A), Some(A));
        assert_eq!(id(&A[..19]), Some(A));
        assert_eq!(id(&format!("nginx@{A}")), Some(A));
        assert_eq!(id("registry.k8s.io/pause:3.10"), Some(B));
        assert_eq!(id("nginx:1.25"), None);
        assert_eq!(id("pause:3.10"), None, "a different registry");
        assert_eq!(id(""), None);
    }

    #[test]
    fn names_a_tag_only_for_names_without_a_digest() {
        assert_eq!(
            full_tag("nginx").as_deref(),
            Some("docker.io/library/nginx:latest")
        );
        assert_eq!(
            full_tag("localhost:5000/app:v1").as_deref(),
            Some("localhost:5000/app:v1")
        );
        assert_eq!(full_tag(A), None);
        assert_eq!(full_tag(&format!("nginx@{A}")), None);
    }

    #[test]
    fn reads_the_image_user_like_containerd() {
        assert_eq!(user(None), (None, String::new()));
        assert_eq!(user(Some("")), (None, String::new()));
        assert_eq!(
            user(Some("1000")),
            (Some(pb::Int64Value { value: 1000 }), String::new())
        );
        assert_eq!(
            user(Some("1000:1000")),
            (Some(pb::Int64Value { value: 1000 }), String::new())
        );
        assert_eq!(user(Some("nginx")), (None, "nginx".to_string()));
        assert_eq!(user(Some("nginx:www")), (None, "nginx".to_string()));
    }
}
