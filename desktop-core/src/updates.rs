//! Update checks and upgrades of the app and of `podbag`.

use std::path::PathBuf;

pub use podbag::dashboard::cli::{Check, Updates, Upgraded};

/// `never`, `due` (when the window opens: at most once a day) or `now`.
pub fn parse_check(text: &str) -> Check {
    match text {
        "now" => Check::Now,
        "due" => Check::IfDue,
        _ => Check::Never,
    }
}

/// Where the last update check is saved.
pub fn update_file() -> Option<PathBuf> {
    podbag::client::ui_dir().map(|d| d.join("update.json"))
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The page of the latest release.
pub fn open_releases() -> Result<(), String> {
    let url = format!("{}/releases/latest", env!("CARGO_PKG_REPOSITORY"));
    open::that_detached(url).map_err(|e| e.to_string())
}

/// The project's page.
pub fn open_repository() -> Result<(), String> {
    open::that_detached(env!("CARGO_PKG_REPOSITORY")).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_are_named_as_the_window_names_them() {
        assert_eq!(parse_check("now"), Check::Now);
        assert_eq!(parse_check("due"), Check::IfDue);
        assert_eq!(parse_check("never"), Check::Never);
        assert_eq!(parse_check("soon"), Check::Never);
        assert_eq!(
            env!("CARGO_PKG_REPOSITORY"),
            "https://github.com/SteelCrab/podbag"
        );
    }
}
