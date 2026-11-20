//! Installed instructions and completion records for hands-on volume lessons. Lessons can include a small offline animation.
pub mod gui;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

pub const SCOPE: &str = "PODBAG_VOLUME_LAB";
pub const FORMAT: &str = "podbag-cli-tui-volumes-v1";
pub const LIMIT: usize = 65_536;

#[derive(Serialize, Deserialize, Clone)]
pub struct Introduction {
    pub title: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Animation {
    pub mp4: String,
    pub webp: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Words {
    pub title: String,
    pub guide: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intro: Option<Introduction>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Package {
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animation: Option<Animation>,
    pub translations: BTreeMap<String, Words>,
}

#[derive(Serialize, Deserialize)]
pub struct Installed {
    pub package: Package,
    pub completed: bool,
    generation: String,
}

pub fn directory() -> io::Result<PathBuf> {
    crate::client::ui_dir()
        .map(|p| p.join("tutorials"))
        .ok_or_else(|| io::Error::other("Cannot locate the tutorial library."))
}

pub fn read(dir: &Path) -> io::Result<Option<Installed>> {
    match fs::read(dir.join("cli-tui-volumes.json")) {
        Ok(bytes) => {
            let record: Installed = serde_json::from_slice(&bytes)?;
            if !record.package.translations.contains_key("en") {
                return Err(io::Error::other(
                    "The installed lesson is missing its English guide.",
                ));
            }
            Ok(Some(record))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn locked(dir: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("cli-tui-volumes.lock"))?;
    file.lock()?;
    Ok(file)
}

fn write(dir: &Path, record: &Installed) -> io::Result<()> {
    let mut tmp = NamedTempFile::new_in(dir)?;
    serde_json::to_writer(tmp.as_file_mut(), record)?;
    tmp.flush()?;
    tmp.persist(dir.join("cli-tui-volumes.json"))?;
    Ok(())
}

pub fn install(dir: &Path, text: &str) -> io::Result<()> {
    if text.len() > LIMIT {
        return Err(io::Error::other("The module is too large."));
    }
    let package: Package = serde_json::from_str(text)?;
    let valid_text =
        |s: &str| !s.trim().is_empty() && !s.chars().any(|c| c.is_control() && c != '\n');
    if (package.format != FORMAT && package.format != gui::FORMAT)
        || !package.translations.contains_key("en")
        || package.animation.as_ref().is_some_and(|a| {
            package.translations["en"].intro.is_none()
                || [&a.mp4, &a.webp].into_iter().any(|s| {
                    s.is_empty()
                        || !s.len().is_multiple_of(4)
                        || !s
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'='))
                })
        })
        || package.translations.values().any(|w| {
            !valid_text(&w.title)
                || !valid_text(&w.guide)
                || w.intro
                    .as_ref()
                    .is_some_and(|i| !valid_text(&i.title) || !valid_text(&i.text))
        })
    {
        return Err(io::Error::other("Invalid volume lesson."));
    }
    let _lock = locked(dir)?;
    let ticket = NamedTempFile::new_in(dir)?;
    let generation = ticket
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let completed = read(dir)
        .ok()
        .flatten()
        .is_some_and(|r| r.package.format == package.format && r.completed);
    write(
        dir,
        &Installed {
            package,
            generation,
            completed,
        },
    )
}

pub fn remove(dir: &Path) -> io::Result<()> {
    let _lock = locked(dir)?;
    match fs::remove_file(dir.join("cli-tui-volumes.json")) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

impl Installed {
    pub fn finish(&self, dir: &Path) -> io::Result<()> {
        let _lock = locked(dir)?;
        if let Some(mut current) = read(dir)?
            && current.generation == self.generation
            && !current.completed
        {
            current.completed = true;
            write(dir, &current)?;
        }
        Ok(())
    }
    pub fn words(&self, lang: &str) -> &Words {
        let translations = &self.package.translations;
        translations
            .get(lang)
            .or_else(|| translations.get(lang.split(['-', '_', '.']).next().unwrap_or("en")))
            .unwrap_or(&translations["en"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_and_reinstalling_cannot_restore_an_earlier_sessions_progress() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let package = r#"{"format":"podbag-cli-tui-volumes-v1","translations":{"en":{"title":"Volumes","guide":"Open a terminal."}}}"#;
        install(dir, package).unwrap();
        let previous = read(dir).unwrap().unwrap();
        remove(dir).unwrap();
        previous.finish(dir).unwrap();
        assert!(read(dir).unwrap().is_none());
        install(dir, package).unwrap();
        previous.finish(dir).unwrap();
        let current = read(dir).unwrap().unwrap();
        assert!(!current.completed);
        assert_eq!(current.words("ko").title, "Volumes");
        current.finish(dir).unwrap();
        assert!(read(dir).unwrap().unwrap().completed);
        install(dir, package).unwrap();
        assert!(read(dir).unwrap().unwrap().completed);
        install(dir, &package.replace(FORMAT, gui::FORMAT)).unwrap();
        assert!(!read(dir).unwrap().unwrap().completed);
        assert!(install(dir, "{}").is_err());
    }
}
