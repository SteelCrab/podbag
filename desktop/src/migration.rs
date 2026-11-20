//! Explicit source and destination connections are independent of the browsing session.
use super::{AppState, cluster_migration, commands};
use podbag::client::{Choice, Client, Kind, Target, migration};
use serde::Deserialize;
use serde_json::Value;
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
) -> Result<Value, String> {
    commands::list(&client(&state, &source)?, &kind)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    source: String,
    destination: String,
    kind: String,
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
            &request.uid,
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
                progress,
            )
        }
    }
}

#[tauri::command(async)]
pub fn migration_sources(state: State<'_, AppState>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    [Choice::Podbag, Choice::Podman, Choice::Kiri, Choice::Docker]
        .into_iter()
        .filter_map(|choice| {
            let target = Target::find(Some(&choice), &state.podbag).ok()?;
            let identity = match &target.endpoint {
                podbag::client::Endpoint::Socket { path, .. } => std::fs::canonicalize(path)
                    .unwrap_or_else(|_| path.clone())
                    .to_string_lossy()
                    .into_owned(),
                cmd => cmd.describe(),
            };
            if !seen.insert(identity) {
                return None;
            }
            #[cfg(target_os = "macos")]
            if choice == Choice::Podbag
                && commands::vm_status(&state.podbag).as_deref() != Some("running")
            {
                return None;
            }
            target.endpoint.listening().then(|| choice.to_setting())
        })
        .collect()
}
