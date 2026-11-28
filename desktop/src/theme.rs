//! Podbag Desktop's colors: the framework's light and dark
//! palettes, and the colored squares of the settings.

use podbag_desktop_core::ThemePreference;
use podbag_ui::ui::Theme;
use podbag_ui::{Color, rgb};

pub fn theme(pref: ThemePreference, system_dark: bool) -> Theme {
    let dark = match pref {
        ThemePreference::Dark => true,
        ThemePreference::Light => false,
        ThemePreference::System => system_dark,
    };
    if dark { Theme::dark() } else { Theme::light() }
}

/// The icon square of a setting.
pub fn setting_color(key: &str) -> Color {
    rgb(match key {
        "startup" => 0x24846c,
        "language" => 0x3379bd,
        "theme" => 0x8254b2,
        "engine" => 0x516887,
        "vm" => 0xa76528,
        "updates" => 0x316fb9,
        "terminalCommand" => 0x3d7a55,
        _ => 0x677382,
    })
}
