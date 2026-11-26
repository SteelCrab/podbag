//! The app's settings: language, theme and the sidebar, in `settings.json` in the app's
//! config folder.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// The sidebar's width: the default and the limits, in CSS pixels.
pub const SIDEBAR_WIDTH: u32 = 216;
pub const SIDEBAR_MIN: u32 = 160;
pub const SIDEBAR_MAX: u32 = 360;

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    Light,
    Dark,
    #[default]
    #[serde(other)]
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// `system`, or one of [`crate::strings::LANGUAGES`].
    pub language: String,
    pub theme: ThemePreference,
    pub sidebar_width: u32,
    /// The user collapsed the sidebar to a rail of icons.
    pub sidebar_collapsed: bool,
    /// The user answered "Not now" to installing the `podbag` command.
    pub cli_notice_dismissed: bool,
    /// The app version for which the user answered "Not now" to updating the terminal's
    /// separate `podbag`.
    pub cli_update_dismissed: Option<String>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            language: "system".into(),
            theme: ThemePreference::System,
            sidebar_width: SIDEBAR_WIDTH,
            sidebar_collapsed: false,
            cli_notice_dismissed: false,
            cli_update_dismissed: None,
        }
    }
}

/// An older installation may have a webview profile even if no preferences were saved.
pub fn take_star_prompt(dir: &Path, webview_data: &Path) -> bool {
    if webview_data.try_exists().unwrap_or(true)
        || dir.join("settings.json").try_exists().unwrap_or(true)
    {
        return false;
    }
    std::fs::create_dir_all(dir).is_ok()
        && std::fs::File::create_new(dir.join("star-prompt-shown")).is_ok()
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

    /// Fall back to the system language and clamp the sidebar width.
    pub fn checked(mut self) -> Settings {
        if self.language != "system" && !crate::strings::LANGUAGES.contains(&self.language.as_str())
        {
            self.language = "system".into();
        }
        self.sidebar_width = self.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_first_launch_gets_the_star_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("desktop");
        let profile = temp.path().join("webview");
        let shown = std::thread::scope(|s| {
            let other = s.spawn(|| take_star_prompt(&dir, &profile));
            usize::from(take_star_prompt(&dir, &profile)) + usize::from(other.join().unwrap())
        });
        assert_eq!(shown, 1);
        assert!(!take_star_prompt(&dir, &profile));
    }

    #[test]
    fn an_existing_webview_skips_the_prompt_even_without_settings() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!take_star_prompt(&dir.path().join("desktop"), dir.path()));
    }

    #[test]
    fn existing_settings_skip_the_star_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, r#"{"language":"ko"}"#).unwrap();
        assert!(!take_star_prompt(dir.path(), &dir.path().join("webview")));
        assert_eq!(Settings::load(&file).language, "ko");
    }

    #[test]
    fn the_answers_to_the_terminal_command_notices_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, r#"{"language":"ko"}"#).unwrap();
        let old = Settings::load(&file);
        assert!(!old.cli_notice_dismissed);
        assert_eq!(old.cli_update_dismissed, None);
        let answered = Settings {
            cli_notice_dismissed: true,
            cli_update_dismissed: Some("0.3.1".into()),
            ..old
        };
        answered.save(&file).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(json["cliNoticeDismissed"], true);
        assert_eq!(json["cliUpdateDismissed"], "0.3.1");
        assert_eq!(Settings::load(&file), answered);
    }

    #[test]
    fn the_theme_is_read_and_written_by_name() {
        for (name, theme) in [
            ("system", ThemePreference::System),
            ("light", ThemePreference::Light),
            ("dark", ThemePreference::Dark),
        ] {
            let json = serde_json::json!({ "theme": name });
            let settings: Settings = serde_json::from_value(json).unwrap();
            assert_eq!(settings.theme, theme);
            assert_eq!(serde_json::to_value(settings).unwrap()["theme"], name);
        }
        for json in [
            r#"{"language":"ko"}"#,
            r#"{"language":"ko","theme":"unknown"}"#,
        ] {
            let settings: Settings = serde_json::from_str(json).unwrap();
            assert_eq!(settings.language, "ko");
            assert_eq!(settings.theme, ThemePreference::System);
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
            sidebar_width: 280,
            sidebar_collapsed: true,
            ..Settings::default()
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
