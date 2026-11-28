//! Appearance and startup dialogs.
use super::dialog::{Result, Screen};
pub(super) use crate::preferences::Appearance;
use crate::preferences::{self, LANGS, THEMES};
use ratatui::style::{Color, Style};
use serde_json::Value;
use std::path::Path;
use std::sync::OnceLock;

fn catalog() -> &'static Value {
    static TABLE: OnceLock<Value> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("labels.json")).expect("terminal language catalog")
    })
}
impl Appearance {
    pub fn text(&self, key: &str) -> String {
        let index = LANGS.iter().position(|l| *l == self.lang()).unwrap_or(0);
        catalog()
            .get(key)
            .and_then(|v| v.get(index))
            .and_then(Value::as_str)
            .unwrap_or(key)
            .into()
    }
    pub fn hint(&self, name: &str) -> String {
        let key = match name {
            "actions" => "col.actions",
            "start" => "action.start",
            "stop" => "action.stop",
            "restart" => "action.restart",
            "logs" => "action.logs",
            "shell" => "action.shell",
            "remove" => "action.remove",
            "pull" => "action.pull",
            "prune" => "action.prune",
            "find" => "search.placeholder",
            "create" => "volume.create",
            "inspect" => "action.inspect",
            "quit" => "tray.quit",
            "back" => "action.back",
            "follow" => "action.follow",
            "help" => "terminal.keys",
            "login" => "pull.login",
            "from" | "registries" => "pull.location",
            "cancel" => "action.cancel",
            "ok" | "done" => "action.save",
            "move" | "choose" => "pull.keyMove",
            "yes" => "terminal.yes",
            "no" => "terminal.no",
            "training" => "training.title",
            _ => return name.to_owned(),
        };
        if self.lang() == "en" {
            name.to_owned()
        } else {
            self.text(key)
        }
    }
    pub fn style(&self) -> Style {
        match self.theme.as_str() {
            "light" => Style::default().fg(Color::Black).bg(Color::White),
            "dark" => Style::default().fg(Color::White).bg(Color::Black),
            _ => Style::default(),
        }
    }
}

pub fn appearance(screen: &mut Screen, language: bool) -> Result<()> {
    let next = if language {
        let mut labels = vec![screen.t("settings.system")];
        labels.extend(
            catalog()["language.name"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().into()),
        );
        let index = screen.pick(&screen.t("settings.language"), &labels)?;
        Appearance::set_language(if index == 0 {
            "system"
        } else {
            LANGS[index - 1]
        })?
    } else {
        let index = screen.menu(
            "settings.theme",
            &["settings.system", "settings.light", "settings.dark"],
        )?;
        Appearance::set_theme(THEMES[index])?
    };
    *screen.look = next;
    Ok(())
}

pub fn startup(screen: &mut Screen, exe: &Path) -> Result<()> {
    let enabled = preferences::startup(exe, None)?;
    let title = format!(
        "{}   [{}]",
        screen.t("settings.launchAtLogin"),
        if enabled { "✓" } else { " " }
    );
    let index = screen.pick(
        &title,
        &[
            screen.t("action.cancel"),
            format!(
                "[{}] {}",
                if enabled { " " } else { "✓" },
                screen.t("settings.launchAtLogin")
            ),
        ],
    )?;
    if index == 1 {
        preferences::startup(exe, Some(!enabled))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `{name}`, `{n}`, … in a text, sorted (as desktop/src/strings.rs).
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
    fn every_label_has_seven_languages() {
        let table: serde_json::Map<String, Value> =
            serde_json::from_str(include_str!("labels.json")).unwrap();
        for (key, words) in &table {
            let words = words.as_array().unwrap();
            assert_eq!(words.len(), 7, "{key}");
            let en = placeholders(words[0].as_str().unwrap());
            for w in words {
                let w = w.as_str().unwrap();
                assert!(!w.trim().is_empty(), "{key}");
                assert_eq!(placeholders(w), en, "{key}: {w}");
            }
        }
    }

    #[test]
    fn the_training_hint_names_the_panel() {
        let ko = Appearance {
            language: "ko".into(),
            ..Default::default()
        };
        assert_eq!(ko.hint("training"), ko.text("training.title"));
        assert_ne!(ko.text("training.title"), "training.title");
    }
}
