//! Explicit source and destination connections are independent of the browsing session.
use super::{AppState, cluster_migration, commands, images};
use podbag::client::{Choice, Client, Kind, Location, Target, migration};
use podbag::image::reference::{Reference, is_digest};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tauri::{State, ipc::Channel};

fn client(state: &AppState, name: &str) -> Result<Client, String> {
    let choice = Choice::parse(name).ok_or_else(|| format!("Unknown engine {name}"))?;
    Ok(Client::new(
        Target::find(Some(&choice), &state.podbag)?.endpoint,
    ))
}

#[tauri::command(async)]
pub fn migration_list(
    state: State<'_, AppState>,
    source: String,
    kind: String,
    hosts: Option<Vec<(String, String)>>,
) -> Result<Value, String> {
    list(&client(&state, &source)?, &kind, &hosts.unwrap_or_default())
}

fn list(client: &Client, kind: &str, hosts: &[(String, String)]) -> Result<Value, String> {
    let path = match kind {
        "images" => "/images/json",
        "containers" => "/containers/json?all=1",
        _ => return commands::list(client, kind),
    };
    let mut rows = client
        .call("GET", path, None)
        .map_err(|error| error.message)?;
    let entries = rows
        .as_array_mut()
        .ok_or("The source engine did not return a list")?;
    let configured = images::configured_locations(hosts);
    for entry in entries {
        let entry = entry
            .as_object_mut()
            .ok_or("The source engine returned an invalid list entry")?;
        if kind == "images" {
            let registries = entry
                .get("RepoTags")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|tag| named_reference(tag))
                .map(|tag| (tag.to_owned(), registry(tag, &configured)))
                .collect::<Map<_, _>>();
            entry.insert("MigrationRegistries".into(), Value::Object(registries));
        } else if let Some(reference) = container_reference(entry) {
            let value = registry(reference, &configured);
            entry.insert("MigrationRegistry".into(), value);
        } else {
            entry.remove("MigrationRegistry");
        }
    }
    Ok(rows)
}

fn registry(reference: &str, configured: &[Location]) -> Value {
    let location = images::registry_location(reference, configured);
    json!({ "id": location.id(), "label": location.label() })
}

fn named_reference(reference: &str) -> bool {
    !is_digest(reference)
        && !podbag::id::is_valid(reference)
        && Reference::parse(reference).is_ok_and(|reference| reference.digest.is_none())
}

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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    source: String,
    destination: String,
    kind: String,
    #[serde(default)]
    destination_defaults: bool,
    name: String,
    #[serde(default)]
    uid: String,
    #[serde(default)]
    source_namespace: String,
    #[serde(default)]
    destination_namespace: String,
}
#[tauri::command(async)]
pub fn migrate(
    state: State<'_, AppState>,
    request: Request,
    updates: Channel<String>,
) -> Result<(), String> {
    let _guard = state
        .migration
        .try_lock()
        .map_err(|_| "Another migration is in progress")?;
    let progress = |s: &str| {
        let _ = updates.send(s.to_owned());
    };
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
            let from = client(&state, &request.source)?;
            if kind == Kind::Images
                && !request.uid.is_empty()
                && from.inspect(kind, &request.name).map_err(|e| e.message)?["Id"] != request.uid
            {
                return Err("The image tag changed. Select it again".into());
            }
            migration::migrate(
                &from,
                &client(&state, &request.destination)?,
                kind,
                &request.name,
                request.destination_defaults,
                progress,
            )
        }
    }
}

#[tauri::command(async)]
pub fn migration_sources(state: State<'_, AppState>) -> Vec<String> {
    podbag::dashboard::local_engines(&state.podbag)
        .into_iter()
        .map(|t| t.choice.to_setting())
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, json as reply_json, small_engine};

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
        let mut expected = Map::new();
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
                json!({ "id": location.id(), "label": label }),
            );
        }
        expected.insert(
            "alpine:3.23".into(),
            json!({"id": "dockerhub", "label": "Docker Hub"}),
        );

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
        let mut list = list(&engine.client, "images", &hosts).unwrap();
        assert_eq!(
            list[0]
                .as_object_mut()
                .unwrap()
                .remove("MigrationRegistries"),
            Some(Value::Object(expected))
        );
        for index in 1..3 {
            assert_eq!(
                list[index]
                    .as_object_mut()
                    .unwrap()
                    .remove("MigrationRegistries"),
                Some(json!({}))
            );
        }
        assert_eq!(list, source);
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
        let mut list = list(&engine.client, "containers", &hosts).unwrap();
        for (index, (id, label)) in [
            ("gitlab", "GitLab"),
            ("gitlab-self", "GitLab Self-Managed"),
            ("dockerhub", "Docker Hub"),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                list[index]
                    .as_object_mut()
                    .unwrap()
                    .remove("MigrationRegistry"),
                Some(json!({ "id": id, "label": label }))
            );
        }
        assert_eq!(list, source);
        assert_eq!(engine.requests(), ["GET /containers/json?all=1"]);
    }
}
