//! The app's settings: language and theme, in `settings.json` in the app's config folder.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    Light,
    Dark,
    #[default]
    #[serde(other)]
    System,
}

impl ThemePreference {
    pub fn native(self) -> Option<tauri::Theme> {
        match self {
            Self::Light => Some(tauri::Theme::Light),
            Self::Dark => Some(tauri::Theme::Dark),
            Self::System => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `system`, or one of [`crate::strings::LANGUAGES`].
    pub language: String,
    pub theme: ThemePreference,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            language: "system".into(),
            theme: ThemePreference::System,
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

    /// An unknown language follows the system.
    pub fn checked(mut self) -> Settings {
        if self.language != "system" && !crate::strings::LANGUAGES.contains(&self.language.as_str())
        {
            self.language = "system".into();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_theme_matches_saved_preference() {
        for (name, native) in [
            ("system", None),
            ("light", Some(tauri::Theme::Light)),
            ("dark", Some(tauri::Theme::Dark)),
        ] {
            let json = serde_json::json!({ "theme": name });
            let settings: Settings = serde_json::from_value(json).unwrap();
            assert_eq!(settings.theme.native(), native);
            assert_eq!(serde_json::to_value(settings).unwrap()["theme"], name);
        }
        for json in [
            r#"{"language":"ko"}"#,
            r#"{"language":"ko","theme":"unknown"}"#,
        ] {
            let settings: Settings = serde_json::from_str(json).unwrap();
            assert_eq!(settings.language, "ko");
            assert_eq!(settings.theme.native(), None);
        }
    }

    #[test]
    fn saved_and_loaded() {
        let dir = std::env::temp_dir().join(format!("podbag-desktop-{}", std::process::id()));
        let file = dir.join("settings.json");
        assert_eq!(Settings::load(&file), Settings::default());
        let s = Settings {
            language: "ko".into(),
            theme: ThemePreference::Dark,
        };
        s.save(&file).unwrap();
        assert_eq!(Settings::load(&file), s);
        std::fs::write(&file, r#"{"language": "xx", "theme": "pink"}"#).unwrap();
        assert_eq!(Settings::load(&file), Settings::default());
        let _ = std::fs::remove_dir_all(dir);
    }
}
