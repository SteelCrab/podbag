//! The engine's four lists, their search filter and the bits of each
//! object that the views show.

use podbag_desktop_core::{ContainerSummary, ImageSummary, Kind, NetworkSummary, VolumeSummary};

pub fn kind_index(kind: Kind) -> usize {
    match kind {
        Kind::Containers => 0,
        Kind::Images => 1,
        Kind::Volumes => 2,
        Kind::Networks => 3,
    }
}

/// The first 12 characters of an id, without `sha256:`.
pub fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    id.get(..12).unwrap_or(id)
}

/// The published ports: `8080→80/tcp, 53/udp`, each once.
pub fn ports_text(c: &ContainerSummary) -> String {
    let mut seen: Vec<String> = Vec::new();
    for p in &c.ports {
        let kind = if p.kind.is_empty() { "tcp" } else { &p.kind };
        let text = if p.public_port > 0 {
            format!("{}→{}/{kind}", p.public_port, p.private_port)
        } else {
            format!("{}/{kind}", p.private_port)
        };
        if !seen.contains(&text) {
            seen.push(text);
        }
    }
    seen.join(", ")
}

/// The container's name, or its short id when it has none.
pub fn container_name(c: &ContainerSummary) -> String {
    match c.name() {
        "" => short_id(&c.id).to_string(),
        n => n.to_string(),
    }
}

/// Running, paused or restarting: removing it needs force.
pub fn is_up(c: &ContainerSummary) -> bool {
    matches!(c.state.as_str(), "running" | "paused" | "restarting")
}

pub fn gateway(n: &NetworkSummary) -> &str {
    n.ipam.config.first().map_or("", |c| c.gateway.as_str())
}

#[derive(Default)]
pub struct Lists {
    pub containers: Vec<ContainerSummary>,
    pub images: Vec<ImageSummary>,
    pub volumes: Vec<VolumeSummary>,
    pub networks: Vec<NetworkSummary>,
    pub errors: [Option<String>; 4],
    /// The lower-case text the search looks in, per object.
    hay: [Vec<String>; 4],
    /// The filtered indices per kind, and the search text they are for.
    shown: [Vec<usize>; 4],
    shown_for: [Option<String>; 4],
}

impl Lists {
    pub fn len(&self, kind: Kind) -> usize {
        match kind {
            Kind::Containers => self.containers.len(),
            Kind::Images => self.images.len(),
            Kind::Volumes => self.volumes.len(),
            Kind::Networks => self.networks.len(),
        }
    }

    pub fn running(&self) -> usize {
        self.containers
            .iter()
            .filter(|c| c.state == "running")
            .count()
    }

    /// Lower-case the searched fields once per refresh.
    pub fn index(&mut self) {
        let lower = |v: Vec<&str>| v.join("\n").to_lowercase();
        self.hay[0] = self
            .containers
            .iter()
            .map(|c| {
                lower(vec![
                    &container_name(c),
                    &c.image,
                    &c.id,
                    &c.state,
                    &ports_text(c),
                ])
            })
            .collect();
        self.hay[1] = self
            .images
            .iter()
            .map(|i| {
                lower(
                    i.repo_tags
                        .iter()
                        .map(String::as_str)
                        .chain([i.id.as_str()])
                        .collect(),
                )
            })
            .collect();
        self.hay[2] = self
            .volumes
            .iter()
            .map(|v| lower(vec![&v.name, &v.driver]))
            .collect();
        self.hay[3] = self
            .networks
            .iter()
            .map(|n| lower(vec![&n.name, &n.driver, &n.id, n.subnet()]))
            .collect();
        self.shown_for = Default::default();
    }

    /// The indices that match `q` (trimmed, any case), computed again only when it changed.
    pub fn filter(&mut self, kind: Kind, q: &str) -> &[usize] {
        let i = kind_index(kind);
        let q = q.trim().to_lowercase();
        if self.shown_for[i].as_deref() != Some(q.as_str()) {
            self.shown[i] = self.hay[i]
                .iter()
                .enumerate()
                .filter(|(_, h)| q.is_empty() || h.contains(&q))
                .map(|(i, _)| i)
                .collect();
            self.shown_for[i] = Some(q);
        }
        &self.shown[i]
    }

    /// The id the API takes for object `i` (a volume's name).
    pub fn id(&self, kind: Kind, i: usize) -> String {
        match kind {
            Kind::Containers => self.containers[i].id.clone(),
            Kind::Images => self.images[i].id.clone(),
            Kind::Volumes => self.volumes[i].name.clone(),
            Kind::Networks => self.networks[i].id.clone(),
        }
    }

    /// The name an object shows.
    pub fn name(&self, kind: Kind, i: usize) -> String {
        match kind {
            Kind::Containers => container_name(&self.containers[i]),
            Kind::Images => self.images[i].name(),
            Kind::Volumes => self.volumes[i].name.clone(),
            Kind::Networks => self.networks[i].name.clone(),
        }
    }

    /// By the API id, or by the engine id.
    pub fn find(&self, kind: Kind, id: &str) -> Option<usize> {
        match kind {
            Kind::Containers => self.containers.iter().position(|c| c.id == id),
            Kind::Images => self.images.iter().position(|o| o.id == id),
            Kind::Volumes => self.volumes.iter().position(|o| o.name == id),
            Kind::Networks => self.networks.iter().position(|o| o.id == id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use podbag_desktop_core::Port;

    fn container(name: &str, image: &str) -> ContainerSummary {
        ContainerSummary {
            id: format!("{name}0123456789abcdef"),
            names: vec![format!("/{name}")],
            image: image.into(),
            state: "running".into(),
            ports: vec![
                Port {
                    ip: "0.0.0.0".into(),
                    private_port: 80,
                    public_port: 8080,
                    kind: "tcp".into(),
                },
                Port {
                    ip: "::".into(),
                    private_port: 80,
                    public_port: 8080,
                    kind: "tcp".into(),
                },
            ],
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn search_matches_any_field_in_any_case() {
        let mut l = Lists {
            containers: vec![
                container("web", "nginx:alpine"),
                container("db", "postgres"),
            ],
            ..Lists::default()
        };
        l.index();
        assert_eq!(l.filter(Kind::Containers, " NGINX "), [0]);
        assert_eq!(l.filter(Kind::Containers, "8080→80"), [0, 1]);
        assert_eq!(l.filter(Kind::Containers, ""), [0, 1]);
        assert!(l.filter(Kind::Containers, "redis").is_empty());
        assert_eq!(ports_text(&l.containers[0]), "8080→80/tcp");
    }
}
