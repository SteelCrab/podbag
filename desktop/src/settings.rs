//! The app's settings: language, theme and the sidebar, in `settings.json` in the app's
//! config folder.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// The sidebar's width: the default and the limits, in CSS pixels.
pub const SIDEBAR_WIDTH: u32 = 216;
pub const SIDEBAR_MIN: u32 = 160;
pub const SIDEBAR_MAX: u32 = 360;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// `system`, or one of [`crate::strings::LANGUAGES`].
    pub language: String,
    /// `system`, `light` or `dark`.
    pub theme: String,
    pub sidebar_width: u32,
    /// The user collapsed the sidebar to a rail of icons.
    pub sidebar_collapsed: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            language: "system".into(),
            theme: "system".into(),
            sidebar_width: SIDEBAR_WIDTH,
            sidebar_collapsed: false,
        }
    }
}

impl Settings {
    /// The saved settings; the defaults when there are none or they cannot be read.
    pub fn load(file: &Path) -> Settings {
        std::fs::read(file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .map(Settings::checked)
            .unwrap_or_default()
    }

    pub fn save(&self, file: &Path) -> Result<(), String> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(file, json).map_err(|e| format!("{}: {e}", file.display()))
    }

    /// Unknown values become `system`; the width comes within its limits.
    pub fn checked(mut self) -> Settings {
        if self.language != "system" && !crate::strings::LANGUAGES.contains(&self.language.as_str())
        {
            self.language = "system".into();
        }
        if !matches!(self.theme.as_str(), "system" | "light" | "dark") {
            self.theme = "system".into();
        }
        self.sidebar_width = self.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_and_loaded() {
        let dir = std::env::temp_dir().join(format!("podbag-desktop-{}", std::process::id()));
        let file = dir.join("settings.json");
        assert_eq!(Settings::load(&file), Settings::default());
        let s = Settings {
            language: "ko".into(),
            theme: "dark".into(),
            sidebar_width: 280,
            sidebar_collapsed: true,
        };
        s.save(&file).unwrap();
        assert_eq!(Settings::load(&file), s);
        std::fs::write(&file, r#"{"language": "xx", "theme": "pink"}"#).unwrap();
        assert_eq!(Settings::load(&file), Settings::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_sidebar_stays_within_its_limits() {
        let wide: Settings = serde_json::from_str(r#"{"sidebarWidth": 900}"#).unwrap();
        assert_eq!(wide.checked().sidebar_width, SIDEBAR_MAX);
        let narrow: Settings = serde_json::from_str(r#"{"sidebarWidth": 20}"#).unwrap();
        assert_eq!(narrow.checked().sidebar_width, SIDEBAR_MIN);
        // Settings from before the sidebar setting.
        let old: Settings = serde_json::from_str(r#"{"language": "ja"}"#).unwrap();
        assert_eq!(old.sidebar_width, SIDEBAR_WIDTH);
        assert!(!old.sidebar_collapsed);
    }
}
