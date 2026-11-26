//! Explicit source and destination connections are independent of the browsing session.
use super::AppState;
use podbag_desktop_core::migration::{self, Request};
use serde_json::Value;
use tauri::{State, ipc::Channel};

#[tauri::command(async)]
pub fn migration_list(
    state: State<'_, AppState>,
    source: String,
    kind: String,
) -> Result<Value, String> {
    let list = migration::list(state.core.podbag(), &source, &kind)?;
    serde_json::to_value(list).map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn migrate(
    state: State<'_, AppState>,
    request: Request,
    updates: Channel<String>,
) -> Result<(), String> {
    state.core.migrate(&request, |s: &str| {
        let _ = updates.send(s.to_owned());
    })
}

#[tauri::command(async)]
pub fn migration_sources(state: State<'_, AppState>) -> Vec<String> {
    migration::sources(state.core.podbag())
}
