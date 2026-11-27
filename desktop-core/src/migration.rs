//! Copy a resource from one engine or cluster to another. The source and the destination
//! are their own connections, apart from the engine the window browses.

use crate::engine::{self, List};
use crate::images;
use podbag::client::{Choice, Client, Kind, Location, Target, migration};
use podbag::dashboard::cluster_migration;
use podbag::image::reference::{Reference, is_digest};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// What to copy, and from where to where.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// An engine (`podbag`, `docker`, …) or a Kubernetes context.
    pub source: String,
    pub destination: String,
    /// `containers`, `images`, `volumes`, `networks`, or `pods`, `services`,
    /// `deployments`, `claims`.
    pub kind: String,
    #[serde(default)]
    pub destination_defaults: bool,
    pub name: String,
    /// The image id or the Kubernetes uid that was chosen: the copy fails if the name
    /// now means something else.
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub source_namespace: String,
    #[serde(default)]
    pub destination_namespace: String,
}

fn client(podbag: &Path, name: &str) -> Result<Client, String> {
    let choice = Choice::parse(name).ok_or_else(|| format!("Unknown engine {name}"))?;
    Ok(Client::new(Target::find(Some(&choice), podbag)?.endpoint))
}

/// The local engines that run now (by their setting name).
pub fn sources(podbag: &Path) -> Vec<String> {
    podbag::dashboard::local_engines(podbag)
        .into_iter()
        .map(|t| t.choice.to_setting())
        .collect()
}

/// The registry an image comes from, for its icon: the location id and its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Registry {
    pub id: &'static str,
    pub label: String,
}

impl Registry {
    fn of(reference: &str, configured: &[Location]) -> Registry {
        let location = images::registry_location(reference, configured);
        Registry {
            id: location.id(),
            label: location.label(),
        }
    }
}

/// A list of an engine, with the registries of its images.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub list: List,
    /// Images: the registry of each tag with a name. Containers: the registry of each
    /// container (by id) whose image has a name.
    pub registries: BTreeMap<String, Registry>,
}

/// A list of the engine `source`. `hosts`: the registry hosts the user gave, by location
/// id.
pub fn list(
    podbag: &Path,
    source: &str,
    kind: &str,
    hosts: &[(String, String)],
) -> Result<Listing, String> {
    list_on(&client(podbag, source)?, engine::parse_kind(kind)?, hosts)
}

fn list_on(client: &Client, kind: Kind, hosts: &[(String, String)]) -> Result<Listing, String> {
    let path = match kind {
        Kind::Images => "/images/json",
        Kind::Containers => "/containers/json?all=1",
        _ => {
            return Ok(Listing {
                list: engine::list(client, kind)?,
                registries: BTreeMap::new(),
            });
        }
    };
    let rows = client
        .call("GET", path, None)
        .map_err(|error| error.message)?;
    listing(kind, rows, hosts)
}

/// The list in `rows` (as the engine gives it) with its registries.
pub fn listing(kind: Kind, rows: Value, hosts: &[(String, String)]) -> Result<Listing, String> {
    let entries = rows
        .as_array()
        .ok_or("The source engine did not return a list")?;
    let configured = images::configured_locations(hosts);
    let mut registries = BTreeMap::new();
    for entry in entries {
        let entry = entry
            .as_object()
            .ok_or("The source engine returned an invalid list entry")?;
        if kind == Kind::Images {
            let tags = entry.get("RepoTags").and_then(Value::as_array);
            for tag in tags.into_iter().flatten().filter_map(Value::as_str) {
                if named_reference(tag) {
                    registries.insert(tag.to_owned(), Registry::of(tag, &configured));
                }
            }
        } else if let (Some(reference), Some(id)) = (
            container_reference(entry),
            entry.get("Id").and_then(Value::as_str),
        ) {
            registries.insert(id.to_owned(), Registry::of(reference, &configured));
        }
    }
    let list = match kind {
        Kind::Images => List::Images(serde_json::from_value(rows).map_err(|e| e.to_string())?),
        _ => List::Containers(serde_json::from_value(rows).map_err(|e| e.to_string())?),
    };
    Ok(Listing { list, registries })
}

fn named_reference(reference: &str) -> bool {
    !is_digest(reference)
        && !podbag::id::is_valid(reference)
        && Reference::parse(reference).is_ok_and(|reference| reference.digest.is_none())
}

/// The image name a container was made from; None for an image id or digest.
fn container_reference(entry: &Map<String, Value>) -> Option<&str> {
    let reference = entry.get("Image")?.as_str()?;
    let opaque_id = reference
        .strip_prefix("sha256:")
        .is_some_and(|hash| !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    if opaque_id || !named_reference(reference) {
        return None;
    }
    if let Some(id) = entry.get("ImageID").and_then(Value::as_str) {
        let short_id = reference.len() >= 12
            && id
                .strip_prefix("sha256:")
                .unwrap_or(id)
                .starts_with(reference)
            && reference.bytes().all(|b| b.is_ascii_hexdigit());
        if reference == id || short_id {
            return None;
        }
    }
    Some(reference)
}

/// Copy as `request` says; `progress` gets each step.
pub fn migrate(podbag: &Path, request: &Request, progress: impl Fn(&str)) -> Result<(), String> {
    match request.kind.as_str() {
        "pods" | "services" | "deployments" | "claims" => cluster_migration::migrate(
            cluster_migration::Scope {
                context: &request.source,
                namespace: &request.source_namespace,
            },
            cluster_migration::Scope {
                context: &request.destination,
                namespace: &request.destination_namespace,
            },
            &request.kind,
            &request.name,
            Some(&request.uid),
            progress,
        ),
        _ => {
            let kind = Kind::ALL
                .into_iter()
                .find(|k| k.segment() == request.kind)
                .ok_or("Unknown engine resource")?;
            let from = client(podbag, &request.source)?;
            if kind == Kind::Images
                && !request.uid.is_empty()
                && from.inspect(kind, &request.name).map_err(|e| e.message)?["Id"] != request.uid
            {
                return Err("The image tag changed. Select it again".into());
            }
            migration::migrate(
                &from,
                &client(podbag, &request.destination)?,
                kind,
                &request.name,
                request.destination_defaults,
                progress,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_read_as_the_window_sends_them() {
        let r: Request = serde_json::from_value(serde_json::json!({
            "source": "docker", "destination": "podbag", "kind": "volumes", "name": "pgdata",
        }))
        .unwrap();
        assert_eq!((r.source.as_str(), r.kind.as_str()), ("docker", "volumes"));
        assert!(!r.destination_defaults && r.uid.is_empty());
    }

    #[test]
    fn unknown_engines_and_kinds_are_refused() {
        let podbag = Path::new("/nonexistent/podbag");
        let request = |kind: &str, source: &str| Request {
            source: source.into(),
            destination: "podbag".into(),
            kind: kind.into(),
            name: "x".into(),
            ..Request::default()
        };
        assert_eq!(
            migrate(podbag, &request("secrets", "docker"), |_| {}),
            Err("Unknown engine resource".into())
        );
        assert_eq!(
            migrate(podbag, &request("volumes", "nowhere engine"), |_| {}),
            Err("Unknown engine nowhere engine".into())
        );
        assert_eq!(
            list(podbag, "nowhere engine", "volumes", &[]).unwrap_err(),
            "Unknown engine nowhere engine"
        );
    }
}

#[cfg(all(test, unix))]
mod engine_tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, json as reply_json, small_engine};
    use serde_json::json;

    fn registry(id: &'static str, label: &str) -> Registry {
        Registry {
            id,
            label: label.into(),
        }
    }

    #[test]
    fn image_list_resolves_each_tag_and_keeps_engine_fields() {
        let hosts = [
            ("harbor".into(), "https://Harbor.Example.org:8443/v2".into()),
            ("nexus".into(), "nexus.example.net:5000".into()),
            ("gitlab-self".into(), "gitlab-registry.example.net".into()),
            (
                "unknown-provider".into(),
                "registry.example.net:5000".into(),
            ),
        ];
        let mut expected = BTreeMap::new();
        for location in Location::all() {
            let host = match location.id() {
                "dockerhub" => "registry-1.docker.io".into(),
                "harbor" => "harbor.example.org:8443".into(),
                "nexus" => "nexus.example.net:5000".into(),
                "gitlab-self" => "gitlab-registry.example.net".into(),
                "custom" => "registry.example.net:5000".into(),
                _ if location.needs_host() => location.host_example().into(),
                _ => location.host(),
            };
            let label = location.with_host(&host).label();
            expected.insert(
                format!("{host}/team/app:1"),
                Registry {
                    id: location.id(),
                    label,
                },
            );
        }
        expected.insert("alpine:3.23".into(), registry("dockerhub", "Docker Hub"));

        let mut tags: Vec<_> = expected.keys().cloned().collect();
        tags.push("<none>:<none>".into());
        let source = json!([
            {"Id": "multi", "RepoTags": tags, "Size": 42, "EngineField": {"sample": true}},
            {"Id": "dangling", "RepoTags": ["<none>:<none>"], "EngineField": [1, 2]},
            {"Id": "untagged", "RepoTags": null},
        ]);
        let engine = FakeEngine::start({
            let source = source.clone();
            move |method, path| match (method, path) {
                ("GET", "/images/json") => reply_json(200, source.clone()),
                _ => small_engine(method, path),
            }
        });
        let listing = list_on(&engine.client, Kind::Images, &hosts).unwrap();
        assert_eq!(listing.registries, expected);
        let List::Images(images) = listing.list else {
            panic!("not images")
        };
        let ids: Vec<&str> = images.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["multi", "dangling", "untagged"]);
        assert_eq!(images[0].size, 42);
        assert_eq!(engine.requests(), ["GET /images/json"]);
    }

    #[test]
    fn container_list_uses_image_names_and_preserves_ids() {
        let hash = "123456789abc".repeat(5) + "def0";
        let digest = format!("sha256:{hash}");
        let source = json!([
            {"Id": "gitlab", "Image": "registry.gitlab.com/project/web:1", "Labels": {"test": "kept"}},
            {"Id": "configured", "Image": "git.example.org:5050/team/web:1"},
            {"Id": "short-name", "Image": "team/web"},
            {"Id": "digest", "Image": digest, "ImageID": digest},
            {"Id": "raw-hash", "Image": hash, "ImageID": digest},
            {"Id": "short-id", "Image": &hash[..12], "ImageID": digest},
            {"Id": "short-digest", "Image": "sha256:abc123"},
            {"Id": "named-digest", "Image": format!("ghcr.io/team/app@{digest}")},
            {"Id": "none", "Image": "<none>:<none>"},
            {"Id": "missing"}
        ]);
        let engine = FakeEngine::start({
            let source = source.clone();
            move |method, path| match (method, path) {
                ("GET", "/containers/json?all=1") => reply_json(200, source.clone()),
                _ => small_engine(method, path),
            }
        });
        let hosts = [("gitlab-self".into(), "git.example.org:5050".into())];
        let listing = list_on(&engine.client, Kind::Containers, &hosts).unwrap();
        let expected: BTreeMap<String, Registry> = [
            ("gitlab", registry("gitlab", "GitLab")),
            ("configured", registry("gitlab-self", "GitLab Self-Managed")),
            ("short-name", registry("dockerhub", "Docker Hub")),
        ]
        .into_iter()
        .map(|(id, r)| (id.to_string(), r))
        .collect();
        assert_eq!(listing.registries, expected);
        assert_eq!(listing.list.len(), 10);
        assert_eq!(engine.requests(), ["GET /containers/json?all=1"]);
    }
}
