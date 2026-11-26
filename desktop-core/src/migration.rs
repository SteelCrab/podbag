//! Copy a resource from one engine or cluster to another. The source and the destination
//! are their own connections, apart from the engine the window browses.

use crate::engine::{self, List};
use podbag::client::{Choice, Client, Kind, Target, migration};
use podbag::dashboard::cluster_migration;
use serde::Deserialize;
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

/// A list of the engine `source`.
pub fn list(podbag: &Path, source: &str, kind: &str) -> Result<List, String> {
    let client = client(podbag, source)?;
    engine::list(&client, engine::parse_kind(kind)?)
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
            list(podbag, "nowhere engine", "volumes").unwrap_err(),
            "Unknown engine nowhere engine"
        );
    }
}
