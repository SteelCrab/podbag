//! Preferences shared by the CLI and TUI.
use podbag::client::{Location, choice};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const LANGS: [&str; 7] = ["en", "ko", "ja", "zh-CN", "fil", "es", "vi"];
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub language: String,
    pub theme: String,
    pub sidebar: bool,
    pub sidebar_width: u16,
}
impl Default for Appearance {
    fn default() -> Self {
        Self {
            language: "system".into(),
            theme: "system".into(),
            sidebar: false,
            sidebar_width: 20,
        }
    }
}
pub fn file(name: &str) -> std::result::Result<PathBuf, String> {
    podbag::client::ui_dir()
        .map(|p| p.join(name))
        .ok_or_else(|| "No user configuration directory".into())
}
impl Appearance {
    pub fn load() -> Self {
        file("terminal.json")
            .ok()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    pub fn lang(&self) -> &str {
        static SYSTEM: OnceLock<String> = OnceLock::new();
        let locale = if self.language == "system" {
            SYSTEM.get_or_init(|| sys_locale::get_locale().unwrap_or_default())
        } else {
            &self.language
        };
        match locale.split(['_', '-', '.']).next().unwrap_or("en") {
            "zh" => "zh-CN",
            "tl" => "fil",
            base => LANGS.iter().copied().find(|l| *l == base).unwrap_or("en"),
        }
    }
}

pub const THEMES: [&str; 3] = ["system", "light", "dark"];

impl Appearance {
    pub fn set_language(value: &str) -> Result<Self, String> {
        if value != "system" && !LANGS.contains(&value) {
            return Err(format!("Choose system or {}", LANGS.join(", ")));
        }
        Self::update(|v| v["language"] = value.into())
    }

    pub fn set_theme(value: &str) -> Result<Self, String> {
        if !THEMES.contains(&value) {
            return Err(format!("Choose {}", THEMES.join(", ")));
        }
        Self::update(|v| v["theme"] = value.into())
    }

    pub fn set_sidebar(visible: Option<bool>, width: Option<u16>) -> Result<Self, String> {
        if width.is_some_and(|n| !(14..=36).contains(&n)) {
            return Err("Choose a sidebar width from 14 to 36 columns".into());
        }
        Self::update(|v| {
            if let Some(visible) = visible {
                v["sidebar"] = visible.into();
            }
            if let Some(width) = width {
                v["sidebar_width"] = width.into();
            }
        })
    }

    fn update(change: impl FnOnce(&mut Value)) -> Result<Self, String> {
        choice::update_json(&file("terminal.json")?, change)?;
        Ok(Self::load())
    }
}

pub fn registry_hosts() -> Value {
    file("registry-hosts.json")
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|s| serde_json::from_slice(&s).ok())
        .unwrap_or_default()
}

pub fn registry_place(name: &str, host: Option<&str>, saved: &Value) -> Option<Location> {
    Location::all()
        .into_iter()
        .find(|place| place.id() == name)
        .map(|place| {
            let host = host.or_else(|| saved[place.id()].as_str()).unwrap_or("");
            place.with_host(host)
        })
}

pub fn save_registry_host(place: &Location) -> Result<(), String> {
    let host = place.host();
    if !place.needs_host()
        || host.is_empty()
        || host.contains('@')
        || host.chars().any(char::is_whitespace)
    {
        return Err("Enter a configurable registry hostname without credentials".into());
    }
    choice::update_json(&file("registry-hosts.json")?, |v| {
        v[place.id()] = host.into()
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn desktop_binary(exe: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "podbag-desktop.exe"
    } else {
        "podbag-desktop"
    };
    let mut paths = vec![exe.with_file_name(name)];
    if cfg!(target_os = "macos") {
        for dir in [
            Some(PathBuf::from("/Applications")),
            std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Applications")),
        ]
        .into_iter()
        .flatten()
        {
            paths.push(dir.join("podbag.app/Contents/MacOS/podbag-desktop"));
            paths.push(dir.join("podbag.app/Contents/MacOS/podbag"));
        }
    }
    #[cfg(windows)]
    for dir in [
        "LOCALAPPDATA",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    {
        for product in ["Podbag Desktop", "podbag"] {
            paths.push(dir.join(product).join(name));
            paths.push(dir.join("Programs").join(product).join(name));
        }
    }
    paths
        .into_iter()
        .find(|p| p.is_file())
        .or_else(|| podbag::client::choice::find_tool("podbag-desktop"))
}

/// Inspect or change the operating system's Desktop login entry.
pub fn startup(exe: &Path, enabled: Option<bool>) -> Result<bool, String> {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        let path = desktop_binary(exe).ok_or("Install Podbag Desktop to manage its login entry")?;
        // The Windows MSI uses this product name for its Desktop login entry.
        let app_name = if cfg!(windows)
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "Podbag Desktop")
        {
            "Podbag Desktop"
        } else {
            "podbag"
        };
        let mut builder = auto_launch::AutoLaunchBuilder::new();
        builder
            .set_app_name(app_name)
            .set_app_path(path.to_str().ok_or("Invalid desktop path")?);
        #[cfg(target_os = "macos")]
        builder.set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent);
        let launch = builder.build().map_err(|e| e.to_string())?;
        let current = launch.is_enabled().map_err(|e| e.to_string())?;
        match enabled {
            Some(true) if !current => launch.enable().map_err(|e| e.to_string())?,
            Some(false) if current => launch.disable().map_err(|e| e.to_string())?,
            _ => {}
        }
        Ok(enabled.unwrap_or(current))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (exe, enabled);
        Err("Desktop login entries are unavailable on this platform".into())
    }
}
