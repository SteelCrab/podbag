//! Image names: `alpine`, `nginx:1.27`, `ghcr.io/owner/app@sha256:…`.

use crate::{Error, ErrorKind, Result};

const DOCKER_HUB: &str = "docker.io";

/// A parsed image name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl Reference {
    /// Parse an image name the way `docker pull` does.
    pub fn parse(s: &str) -> Result<Reference> {
        let invalid = |why: &str| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("invalid image name {s:?}"),
                why,
                "use a name like alpine, nginx:1.27 or ghcr.io/owner/app:tag",
            )
        };
        if s.is_empty() || s.chars().any(char::is_whitespace) {
            return Err(invalid("the name is empty or contains spaces"));
        }

        let (rest, digest) = match s.split_once('@') {
            Some((rest, d)) => {
                if !is_digest(d) {
                    return Err(invalid(
                        "a digest must be sha256: followed by 64 lowercase hex characters",
                    ));
                }
                (rest, Some(d.to_string()))
            }
            None => (s, None),
        };

        let last_slash = rest.rfind('/');
        let tag_at = rest
            .rfind(':')
            .filter(|&i| last_slash.is_none_or(|j| i > j));
        let (name, tag) = match tag_at {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        if let Some(t) = tag {
            if !is_tag(t) {
                return Err(invalid(
                    "a tag uses letters, digits, '_', '.' and '-', at most 128 characters",
                ));
            }
        }

        let (registry, repository) = match name.split_once('/') {
            Some((first, remainder))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), remainder.to_string())
            }
            _ => (DOCKER_HUB.to_string(), name.to_string()),
        };
        let repository = if registry == DOCKER_HUB && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        if !repository.split('/').all(is_path_component) {
            return Err(invalid(
                "a repository uses lowercase letters, digits and the separators '.', '_', '-' and '/'",
            ));
        }

        let tag = match (tag, &digest) {
            (Some(t), _) => Some(t.to_string()),
            (None, None) => Some("latest".to_string()),
            (None, Some(_)) => None,
        };
        Ok(Reference {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// The full name: `docker.io/library/alpine:latest` or `…@sha256:…`.
    pub fn name(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(d), _) => format!("{}/{}@{d}", self.registry, self.repository),
            (None, Some(t)) => format!("{}/{}:{t}", self.registry, self.repository),
            (None, None) => format!("{}/{}", self.registry, self.repository),
        }
    }

    /// What to ask the registry for: the digest if there is one, else the tag.
    pub fn reference(&self) -> &str {
        self.digest
            .as_deref()
            .or(self.tag.as_deref())
            .unwrap_or("latest")
    }

    /// The host that serves the registry API.
    pub fn api_host(&self) -> &str {
        if self.registry == DOCKER_HUB {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    /// Local registries are reached over plain HTTP, like Docker does.
    pub fn insecure(&self) -> bool {
        let host = match self.registry.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(""),
            None => self.registry.split(':').next().unwrap_or(""),
        };
        matches!(host, "localhost" | "127.0.0.1" | "::1")
    }
}

/// `sha256:` followed by 64 lowercase hex characters.
pub fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

fn is_tag(t: &str) -> bool {
    let mut chars = t.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    first_ok
        && t.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn is_path_component(c: &str) -> bool {
    let alnum = |ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit();
    !c.is_empty()
        && c.starts_with(alnum)
        && c.ends_with(alnum)
        && c.chars()
            .all(|ch| alnum(ch) || matches!(ch, '.' | '_' | '-'))
        && !c.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    #[test]
    fn parses_like_docker() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let cases = [
            (
                "alpine",
                "docker.io/library/alpine:latest".to_string(),
                false,
            ),
            (
                "nginx:1.27-alpine",
                "docker.io/library/nginx:1.27-alpine".to_string(),
                false,
            ),
            (
                "bitnami/redis",
                "docker.io/bitnami/redis:latest".to_string(),
                false,
            ),
            (
                "ghcr.io/owner/app:v1",
                "ghcr.io/owner/app:v1".to_string(),
                false,
            ),
            (
                "localhost:5000/app",
                "localhost:5000/app:latest".to_string(),
                true,
            ),
            (
                "127.0.0.1:8080/a/b:t",
                "127.0.0.1:8080/a/b:t".to_string(),
                true,
            ),
            (
                &format!("alpine@{digest}"),
                format!("docker.io/library/alpine@{digest}"),
                false,
            ),
        ];
        for (input, name, insecure) in cases {
            let r = Reference::parse(input).unwrap();
            assert_eq!(r.name(), name, "{input}");
            assert_eq!(r.insecure(), insecure, "{input}");
        }
        let r = Reference::parse(&format!("alpine@{digest}")).unwrap();
        assert_eq!(r.reference(), digest);
        assert_eq!(Reference::parse("alpine").unwrap().reference(), "latest");
    }

    #[test]
    fn rejects_invalid_names() {
        for bad in [
            "",
            "Alpine",
            "alpine:",
            "a@sha256:123",
            "a b",
            "/alpine",
            "alpine//x",
        ] {
            let e = Reference::parse(bad).unwrap_err();
            assert_eq!(e.kind, ErrorKind::InvalidInput, "{bad:?}");
            assert_eq!(e.what, format!("invalid image name {bad:?}"));
        }
    }

    #[test]
    fn api_host_of_docker_hub() {
        assert_eq!(
            Reference::parse("alpine").unwrap().api_host(),
            "registry-1.docker.io"
        );
        assert_eq!(Reference::parse("ghcr.io/x").unwrap().api_host(), "ghcr.io");
    }
}
