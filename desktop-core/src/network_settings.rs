//! Network defaults and active bridge state read from the local engine.
use crate::NetworkPreferences;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSettings {
    pub preferences: NetworkPreferences,
    pub revision: String,
    pub active_subnet: Option<String>,
    pub active_ipv6_subnet: Option<String>,
    pub overrides: Vec<String>,
}
