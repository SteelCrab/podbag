//! The words the app shows outside its web page (the tray menu), from the same string
//! tables as the page (`ui/i18n/<language>.json`).

use std::collections::HashMap;
use std::sync::OnceLock;

/// The languages, as their tables are named.
pub const LANGUAGES: [&str; 7] = ["en", "ko", "ja", "zh-CN", "fil", "es", "vi"];

const TABLES: [(&str, &str); 7] = [
    ("en", include_str!("../ui/i18n/en.json")),
    ("ko", include_str!("../ui/i18n/ko.json")),
    ("ja", include_str!("../ui/i18n/ja.json")),
    ("zh-CN", include_str!("../ui/i18n/zh-CN.json")),
    ("fil", include_str!("../ui/i18n/fil.json")),
    ("es", include_str!("../ui/i18n/es.json")),
    ("vi", include_str!("../ui/i18n/vi.json")),
];

type Table = HashMap<String, String>;

fn tables() -> &'static HashMap<&'static str, Table> {
    static T: OnceLock<HashMap<&'static str, Table>> = OnceLock::new();
    T.get_or_init(|| {
        TABLES
            .iter()
            .map(|(lang, json)| (*lang, serde_json::from_str(json).unwrap_or_default()))
            .collect()
    })
}

/// One of [`LANGUAGES`] for a locale tag (`ko-KR`, `zh_Hans_CN`, `tl`), or None.
pub fn match_locale(tag: &str) -> Option<&'static str> {
    let tag = tag.replace('_', "-").to_ascii_lowercase();
    let base = tag.split('-').next().unwrap_or("");
    match base {
        "zh" => Some("zh-CN"),
        "fil" | "tl" => Some("fil"),
        _ => LANGUAGES.iter().find(|l| **l == base).copied(),
    }
}

/// The language to use: the setting, or the system's when it is `system`.
pub fn resolve(setting: &str, system: Option<String>) -> &'static str {
    LANGUAGES
        .iter()
        .find(|l| **l == setting)
        .copied()
        .or_else(|| system.as_deref().and_then(match_locale))
        .unwrap_or("en")
}

/// The text for `key`, with `{n}` replaced; English when the language lacks it.
pub fn text(lang: &str, key: &str, n: Option<usize>) -> String {
    let t = tables();
    let s = t
        .get(lang)
        .and_then(|table| table.get(key))
        .or_else(|| t.get("en").and_then(|table| table.get(key)))
        .cloned()
        .unwrap_or_else(|| key.to_string());
    match n {
        Some(n) => s.replace("{n}", &n.to_string()),
        None => s,
    }
}

/// The text for `key`, with each `{name}` replaced by its value.
pub fn fill(lang: &str, key: &str, vars: &[(&str, &str)]) -> String {
    vars.iter().fold(text(lang, key, None), |s, (name, value)| {
        s.replace(&format!("{{{name}}}"), value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `{name}`, `{n}`, … in a text, sorted.
    fn placeholders(s: &str) -> Vec<&str> {
        let mut out: Vec<&str> = s
            .split('{')
            .skip(1)
            .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn every_language_has_every_key() {
        let t = tables();
        let en = &t["en"];
        assert!(en.len() > 50, "the English table did not load");
        for lang in LANGUAGES {
            let table = &t[lang];
            let missing: Vec<&String> = en.keys().filter(|k| !table.contains_key(*k)).collect();
            assert!(missing.is_empty(), "{lang} lacks {missing:?}");
            let extra: Vec<&String> = table.keys().filter(|k| !en.contains_key(*k)).collect();
            assert!(extra.is_empty(), "{lang} has unknown keys {extra:?}");
            for (k, v) in table {
                assert!(!v.trim().is_empty(), "{lang}: {k} is empty");
                // Placeholders stay the same.
                assert_eq!(
                    placeholders(&en[k]),
                    placeholders(v),
                    "{lang}: {k} has other placeholders"
                );
            }
        }
    }

    #[test]
    fn every_training_step_has_a_name() {
        use podbag::volume_lesson::{Module, course};
        let t = tables();
        let en = &t["en"];
        for module in Module::ALL {
            let steps = serde_json::to_value(course::instructions(module)).unwrap();
            for step in steps.as_array().unwrap() {
                let key = format!("training.course.{}", step["key"].as_str().unwrap());
                assert!(en.contains_key(&key), "{} lacks {key}", module.id());
            }
        }
    }

    #[test]
    fn locales_match_the_tables() {
        assert_eq!(match_locale("ko-KR"), Some("ko"));
        assert_eq!(match_locale("ja"), Some("ja"));
        assert_eq!(match_locale("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(match_locale("zh_CN"), Some("zh-CN"));
        assert_eq!(match_locale("tl-PH"), Some("fil"));
        assert_eq!(match_locale("fil"), Some("fil"));
        assert_eq!(match_locale("es-419"), Some("es"));
        assert_eq!(match_locale("vi-VN"), Some("vi"));
        assert_eq!(match_locale("de-DE"), None);
        assert_eq!(resolve("system", Some("de-DE".into())), "en");
        assert_eq!(resolve("system", Some("ko-KR".into())), "ko");
        assert_eq!(resolve("ja", Some("ko-KR".into())), "ja");
    }

    #[test]
    fn counts_fill_in() {
        assert_eq!(text("en", "tray.running", Some(3)), "Running: 3");
        assert_eq!(text("xx", "tray.open", None), "Open podbag");
        assert_eq!(text("en", "no.such.key", None), "no.such.key");
        assert_eq!(
            fill("en", "tray.update", &[("version", "0.2.0")]),
            "Update available (0.2.0)…"
        );
        assert_eq!(
            fill("en", "tray.engine", &[("name", "Docker")]),
            "Engine: Docker"
        );
    }
}
