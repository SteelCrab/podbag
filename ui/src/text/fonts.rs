//! Font loading: the platform UI font, a monospace font and CJK fallbacks, read from
//! known files instead of scanning every system font. A full scan is the fallback.

use std::path::PathBuf;
use std::time::Instant;

use glyphon::cosmic_text::{Fallback, PlatformFallback};
use glyphon::{FontSystem, fontdb};
use unicode_script::Script;

pub struct Fonts {
    pub system: FontSystem,
    pub family: String,
    pub mono_family: String,
    /// Font faces loaded.
    pub faces: usize,
    pub load_ms: f64,
    /// How the fonts were found, for logs.
    pub strategy: &'static str,
}

impl Fonts {
    /// No fonts at all: text shapes to nothing. For tests that do not look at text.
    pub fn empty() -> Self {
        let system = FontSystem::new_with_locale_and_db("en".into(), fontdb::Database::new());
        Self {
            system,
            family: "sans-serif".into(),
            mono_family: "monospace".into(),
            faces: 0,
            load_ms: 0.,
            strategy: "none",
        }
    }
}

/// Load the platform UI font, a monospace font and CJK fallbacks (Hangul, Japanese,
/// Simplified Chinese). Falls back to a full scan if the known files are missing.
pub fn load_fonts() -> Fonts {
    let start = Instant::now();
    let mut db = fontdb::Database::new();
    let mut strategy = "platform files";
    for path in platform_font_files() {
        let _ = db.load_font_file(&path);
    }
    if db.is_empty() {
        db.load_system_fonts();
        strategy = "full system scan";
    }
    let family = pick(
        &db,
        &[
            "System Font",
            ".SF NS",
            "Segoe UI",
            "Cantarell",
            "Noto Sans",
            "Ubuntu",
            "DejaVu Sans",
        ],
    );
    let mono_family = pick(
        &db,
        &[
            ".SF NS Mono",
            "SF Mono",
            "Consolas",
            "DejaVu Sans Mono",
            "Noto Sans Mono",
            "Liberation Mono",
        ],
    );
    db.set_sans_serif_family(family.clone());
    db.set_monospace_family(mono_family.clone());
    let faces = db.len();
    let system = FontSystem::new_with_locale_and_db_and_fallback("en".into(), db, UiFallback);
    Fonts {
        system,
        family,
        mono_family,
        faces,
        load_ms: start.elapsed().as_secs_f64() * 1000.,
        strategy,
    }
}

/// The platform's fallback lists, except Han text on macOS: it follows the UI language
/// with the fonts loaded here (PingFang, cosmic-text's choice there, is not a plain file).
pub(super) struct UiFallback;

impl Fallback for UiFallback {
    fn common_fallback(&self) -> &'static [&'static str] {
        PlatformFallback.common_fallback()
    }

    fn forbidden_fallback(&self) -> &'static [&'static str] {
        PlatformFallback.forbidden_fallback()
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &'static [&'static str] {
        if !cfg!(target_os = "macos") || !matches!(script, Script::Han | Script::Bopomofo) {
            return PlatformFallback.script_fallback(script, locale);
        }
        match locale {
            "ja" => &["Hiragino Sans"],
            "ko" => &["Apple SD Gothic Neo", "Hiragino Sans GB"],
            _ => &["Hiragino Sans GB", "Hiragino Sans"],
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_font_files() -> Vec<PathBuf> {
    // SF Pro (variable), SF Mono, Apple SD Gothic Neo (Hangul), Hiragino Sans W3/W6
    // (Japanese), Hiragino Sans GB (Simplified Chinese; PingFang is not a plain file).
    let dir = PathBuf::from("/System/Library/Fonts");
    let mut files: Vec<PathBuf> = [
        "SFNS.ttf",
        "SFNSMono.ttf",
        "AppleSDGothicNeo.ttc",
        "Hiragino Sans GB.ttc",
    ]
    .iter()
    .map(|f| dir.join(f))
    .collect();
    // Hiragino file names are not ASCII (and may be NFD on disk): match on the stable parts.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.contains('角') && (name.ends_with("W3.ttc") || name.ends_with("W6.ttc")) {
                files.push(e.path());
            }
        }
    }
    files
}

#[cfg(target_os = "windows")]
fn platform_font_files() -> Vec<PathBuf> {
    // Segoe UI, Consolas, Malgun Gothic (Hangul), Yu Gothic (Japanese), Microsoft YaHei.
    let dir = std::env::var_os("WINDIR")
        .map_or("C:\\Windows".into(), PathBuf::from)
        .join("Fonts");
    [
        "segoeui.ttf",
        "seguisb.ttf",
        "segoeuib.ttf",
        "consola.ttf",
        "malgun.ttf",
        "malgunbd.ttf",
        "YuGothM.ttc",
        "YuGothB.ttc",
        "msyh.ttc",
        "msyhbd.ttc",
    ]
    .iter()
    .map(|f| dir.join(f))
    .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_font_files() -> Vec<PathBuf> {
    // No fixed paths on Linux and the BSDs, so ask fontconfig which files it would use
    // for the UI. All queries run at once. Without `fc-match` the list is empty and
    // load_fonts() scans every system font instead.
    const PATTERNS: [&str; 9] = [
        "sans-serif",
        "sans-serif:weight=600",
        "sans-serif:bold",
        "monospace",
        "monospace:bold",
        "sans-serif:lang=ko",
        "sans-serif:lang=ko:bold",
        "sans-serif:lang=ja",
        "sans-serif:lang=zh-cn",
    ];
    let children: Vec<_> = PATTERNS
        .iter()
        .filter_map(|pattern| {
            std::process::Command::new("fc-match")
                .args(["-f", "%{file}", pattern])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()
        })
        .collect();
    let mut files: Vec<PathBuf> = Vec::new();
    for child in children {
        let Ok(out) = child.wait_with_output() else {
            continue;
        };
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if out.status.success() && path.is_file() && !files.contains(&path) {
            files.push(path);
        }
    }
    files
}

fn pick(db: &fontdb::Database, preferred: &[&str]) -> String {
    let has = |name: &str| {
        db.faces()
            .any(|f| f.families.iter().any(|(n, _)| n == name))
    };
    preferred
        .iter()
        .find(|n| has(n))
        .map(|n| n.to_string())
        .or_else(|| {
            db.faces()
                .next()
                .and_then(|f| f.families.first().map(|(n, _)| n.clone()))
        })
        .unwrap_or_else(|| "sans-serif".into())
}
