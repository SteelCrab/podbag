//! The pull panel and the import from Docker: where images come from and who is logged
//! in there, Docker Hub search, pulls with a login, and copies from one engine to
//! another. No Tauri here, so it is tested against fake engines.

use podbag::client::registries::{config_dir, registry_host};
use podbag::client::{Client, ImageSummary, Location, Target, logins};
use serde::Serialize;

/// A pull location, as the panel shows it. No secrets: only the account name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationInfo {
    /// The stable preference key and icon ID.
    pub id: &'static str,
    pub label: String,
    pub host: String,
    pub host_example: &'static str,
    /// The user gives the host (ECR, GAR, ACR, another registry).
    pub needs_host: bool,
    /// Who is logged in there.
    pub account: Option<String>,
    /// The command to log in with, where a password is not enough (ECR, GAR, ACR).
    pub hint: Option<String>,
}

pub fn location_id(l: &Location) -> &'static str {
    match l {
        Location::DockerHub => "dockerhub",
        Location::GitHub => "github",
        Location::Ecr(_) => "ecr",
        Location::Quay => "quay",
        Location::Gar(_) => "gar",
        Location::GitLab => "gitlab",
        Location::Acr(_) => "acr",
        Location::Preset(preset, _) => preset.id,
        Location::Custom(_) => "custom",
    }
}

/// The location `id`, at `host` where it needs one.
pub fn location(id: &str, host: &str) -> Result<Location, String> {
    Location::all()
        .into_iter()
        .find(|l| location_id(l) == id)
        .map(|l| l.with_host(host))
        .ok_or_else(|| format!("unknown location {id}"))
}

fn info(l: &Location, logins: &[(String, String)]) -> LocationInfo {
    LocationInfo {
        id: location_id(l),
        label: l.label(),
        host: l.host(),
        host_example: l.host_example(),
        needs_host: l.needs_host(),
        account: l.account(logins).map(str::to_string),
        hint: l.login_hint(),
    }
}

/// Every location with its login state. `hosts`: the hosts the user gave (ECR, GAR, ACR,
/// another registry), by id.
pub fn locations(logins: &[(String, String)], hosts: &[(String, String)]) -> Vec<LocationInfo> {
    Location::all()
        .iter()
        .map(|l| {
            let host = hosts
                .iter()
                .find(|(id, _)| id == location_id(l))
                .map(|(_, h)| h.as_str())
                .unwrap_or("");
            info(&l.with_host(host), logins)
        })
        .collect()
}

/// The logins of this user, as the docker CLI keeps them (account names only).
pub fn current_logins() -> Vec<(String, String)> {
    config_dir().map(|d| logins(&d)).unwrap_or_default()
}

/// What a search found; `supported` is false when the engine has no search.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Search {
    pub supported: bool,
    pub results: Vec<podbag::client::SearchResult>,
    pub error: Option<String>,
}

/// Search Docker Hub through the engine.
pub fn search(client: &Client, term: &str) -> Search {
    let term = term.trim();
    if term.is_empty() {
        return Search {
            supported: true,
            ..Search::default()
        };
    }
    match client.search(term, 25) {
        Ok(results) => Search {
            supported: true,
            results,
            error: None,
        },
        Err(e) if e.unsupported() => Search::default(),
        Err(e) => Search {
            supported: true,
            results: Vec::new(),
            error: Some(e.message),
        },
    }
}

/// The reference to pull: `image` at `location` (a short name at GitHub is
/// `ghcr.io/<name>`). Empty names and spaces are refused.
pub fn reference(image: &str, location: Option<&Location>) -> Result<String, String> {
    let image = image.trim();
    if image.is_empty() || image.chars().any(char::is_whitespace) {
        return Err(format!("{image:?} is not an image name"));
    }
    Ok(match location {
        Some(l) => l.qualify(image),
        None => image.to_string(),
    })
}

/// Pull `reference`, with the login for its registry when there is one and the engine is
/// local (`auth` gives it; it is a secret and goes nowhere else).
pub fn pull(
    client: &Client,
    target: &Target,
    reference: &str,
    auth: impl Fn(&str) -> Option<String>,
    progress: impl FnMut(&str),
) -> Result<(), String> {
    let login = if target.allows_registry_auth() {
        auth(reference)
    } else {
        None
    };
    client
        .pull_auth(reference, login.as_deref(), progress)
        .map_err(|e| e.message)
}

/// A tagged image as shown in the Docker import list.
#[derive(Serialize)]
pub struct ImportImage {
    name: String,
    size: i64,
    registry: &'static str,
}

impl ImportImage {
    pub fn from_image(image: ImageSummary, configured: &[Location]) -> Option<Self> {
        let name = image
            .repo_tags
            .into_iter()
            .find(|tag| tag != "<none>:<none>")?;
        let host = registry_host(&name);
        let detected = Location::of(&host);
        let location = configured
            .iter()
            .find(|l| l.host() == host)
            .unwrap_or(&detected);
        let registry = location_id(location);
        Some(Self {
            name,
            size: image.size,
            registry,
        })
    }
}

/// Copy images from one engine to another (`GET /images/get` into `POST
/// /images/load`). Only read from `from`.
pub fn import(from: &Client, to: &Client, names: &[String]) -> Result<(), String> {
    if names.is_empty() {
        return Err("no images chosen".into());
    }
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let archive = from.export_images(&names).map_err(|e| e.message)?;
    to.load_images(archive).map_err(|e| e.message)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, json, small_engine};
    use podbag::client::{Choice, Endpoint};
    use std::cell::Cell;

    fn target_of(choice: Choice, endpoint: Endpoint) -> Target {
        Target { choice, endpoint }
    }

    #[test]
    fn locations_show_who_is_logged_in() {
        let logins = vec![
            ("ghcr.io".to_string(), "octo".to_string()),
            (
                "123456789012.dkr.ecr.eu-west-1.amazonaws.com".to_string(),
                "AWS".to_string(),
            ),
        ];
        let all = locations(&logins, &[]);
        let ids: Vec<&str> = all.iter().map(|l| l.id).collect();
        assert_eq!(
            &ids[..7],
            ["dockerhub", "github", "ecr", "quay", "gar", "gitlab", "acr"]
        );
        assert_eq!(ids.last(), Some(&"custom"));
        assert_eq!(
            ids.len(),
            ids.iter().collect::<std::collections::HashSet<_>>().len()
        );
        let sprite = include_str!("../ui/registry-icons.svg");
        for id in ids {
            assert!(
                sprite.contains(&format!("<symbol id=\"{id}\"")),
                "missing icon for {id}"
            );
        }
        assert_eq!(all[0].account, None);
        assert_eq!(all[1].account.as_deref(), Some("octo"));
        // ECR with no host yet: any ECR login.
        assert_eq!(all[2].account.as_deref(), Some("AWS"));
        assert!(all[2].needs_host && all[2].hint.as_deref().unwrap().contains("aws ecr"));
        assert!(all[4].hint.as_deref().unwrap().contains("gcloud"));
        let acr_host = "contoso-hash.azurecr.io";
        let logins = [(acr_host.into(), "azure-user".into())];
        let given = locations(
            &logins,
            &[
                ("acr".into(), acr_host.into()),
                ("custom".into(), "registry.example.com".into()),
            ],
        );
        let acr = given.iter().find(|l| l.id == "acr").unwrap();
        assert_eq!(acr.host, acr_host);
        assert_eq!(acr.account.as_deref(), Some("azure-user"));
        assert_eq!(acr.hint.as_deref(), Some("az acr login --name contoso"));
        let custom = given.iter().find(|l| l.id == "custom").unwrap();
        assert_eq!(custom.host, "registry.example.com");
        assert_eq!(custom.account, None);
        assert!(location("nowhere", "").is_err());
    }

    #[test]
    fn references_are_qualified_by_location() {
        let gh = location("github", "").unwrap();
        assert_eq!(
            reference("me/app:1", Some(&gh)).unwrap(),
            "ghcr.io/me/app:1"
        );
        let hub = location("dockerhub", "").unwrap();
        assert_eq!(reference(" nginx ", Some(&hub)).unwrap(), "nginx");
        assert_eq!(
            reference("quay.io/x/y", Some(&gh)).unwrap(),
            "quay.io/x/y",
            "a full reference stays"
        );
        assert!(reference("", None).is_err());
        assert!(reference("a b", None).is_err());
    }

    #[test]
    fn search_hides_itself_when_the_engine_has_none() {
        let podbag = FakeEngine::start(small_engine);
        let s = search(&podbag.client, "nginx");
        assert!(!s.supported, "{s:?}");
        let docker = FakeEngine::start(|method, path| {
            if path.starts_with("/images/search") {
                json(
                    200,
                    serde_json::json!([{"name": "nginx", "description": "Official build of Nginx.",
                        "star_count": 20000, "is_official": true}]),
                )
            } else {
                small_engine(method, path)
            }
        });
        let s = search(&docker.client, "ngi nx");
        assert!(s.supported);
        assert_eq!(s.results[0].name, "nginx");
        assert!(s.results[0].is_official);
        assert_eq!(
            docker.requests(),
            ["GET /images/search?term=ngi%20nx&limit=25"]
        );
        assert!(search(&docker.client, "  ").results.is_empty());
    }

    #[test]
    fn a_login_goes_only_to_a_local_engine() {
        let engine = FakeEngine::start(small_engine);
        let asked = Cell::new(0);
        let auth = |r: &str| {
            asked.set(asked.get() + 1);
            assert_eq!(r, "ghcr.io/me/app:1");
            Some(String::from("e30"))
        };
        let local = target_of(Choice::Docker, engine.client.endpoint.clone());
        pull(&engine.client, &local, "ghcr.io/me/app:1", auth, |_| {}).unwrap();
        assert_eq!(asked.get(), 1);
        // Docker reached through `docker system dial-stdio` (tcp, ssh): no login.
        let remote = target_of(
            Choice::Docker,
            Endpoint::Command(vec!["docker".into(), "system".into(), "dial-stdio".into()]),
        );
        assert!(!remote.allows_registry_auth());
        assert!(
            target_of(Choice::Podbag, Endpoint::Command(vec!["podbag.exe".into()]))
                .allows_registry_auth()
        );
        pull(&engine.client, &remote, "ghcr.io/me/app:1", auth, |_| {}).unwrap();
        assert_eq!(asked.get(), 1, "not asked for a remote engine");
        assert_eq!(
            engine.requests(),
            [
                "POST /images/create?fromImage=ghcr.io%2Fme%2Fapp&tag=1",
                "POST /images/create?fromImage=ghcr.io%2Fme%2Fapp&tag=1"
            ]
        );
    }

    #[test]
    fn import_icons_follow_the_first_named_tag() {
        for (name, registry) in [
            ("redis:7", "dockerhub"),
            ("ghcr.io/team/api:1", "github"),
            ("contoso.azurecr.io/team/api:1", "acr"),
            ("example.org/worker:1", "custom"),
            ("harbor.example.org:8443/tools/app:1", "harbor"),
        ] {
            let configured = [location("harbor", "harbor.example.org:8443").unwrap()];
            let row = ImportImage::from_image(
                ImageSummary {
                    repo_tags: vec!["<none>:<none>".into(), name.into(), "other:2".into()],
                    ..Default::default()
                },
                &configured,
            )
            .unwrap();
            assert_eq!(row.name, name);
            assert_eq!(row.registry, registry);
        }
        assert!(ImportImage::from_image(ImageSummary::default(), &[]).is_none());
    }

    #[test]
    fn images_are_copied_from_one_engine_to_another() {
        let docker = FakeEngine::start(|method, path| {
            if path.starts_with("/images/get") {
                (200, "application/x-tar", b"a tar archive".to_vec())
            } else {
                small_engine(method, path)
            }
        });
        let podbag = FakeEngine::start(|method, path| {
            if path == "/images/load" {
                json(
                    200,
                    serde_json::json!({"stream": "Loaded image: nginx:alpine\n"}),
                )
            } else {
                small_engine(method, path)
            }
        });
        import(
            &docker.client,
            &podbag.client,
            &["nginx:alpine".to_string(), "redis:7".to_string()],
        )
        .unwrap();
        assert_eq!(
            docker.requests(),
            ["GET /images/get?names=nginx%3Aalpine&names=redis%3A7"]
        );
        let seen = podbag.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            (seen[0].0.as_str(), seen[0].1.as_str()),
            ("POST", "/images/load")
        );
        assert_eq!(seen[0].2, "a tar archive");
        assert!(import(&docker.client, &podbag.client, &[]).is_err());
    }
}
