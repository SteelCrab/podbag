//! Installed instructions and completion records for hands-on volume lessons. Lessons can include a small offline animation.
pub mod containers;
pub mod course;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Module {
    Containers,
    Web,
    Persistence,
    Service,
    Volumes,
    Tui,
}

impl Module {
    pub const ALL: [Self; 6] = [
        Self::Containers,
        Self::Web,
        Self::Persistence,
        Self::Service,
        Self::Volumes,
        Self::Tui,
    ];

    pub fn format(self) -> &'static str {
        match self {
            Self::Containers => containers::FORMAT,
            Self::Web => "podbag-web-basics-v1",
            Self::Persistence => "podbag-volume-persistence-v1",
            Self::Service => "podbag-web-service-lifecycle-v1",
            Self::Volumes => gui::FORMAT,
            Self::Tui => FORMAT,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Containers => "container-basics",
            Self::Web => "web-basics",
            Self::Persistence => "volume-persistence",
            Self::Service => "web-service-lifecycle",
            Self::Volumes => "volume-basics",
            Self::Tui => "tui-volumes",
        }
    }

    pub fn parse(id: &str) -> io::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|m| m.id() == id)
            .ok_or_else(|| io::Error::other("Unknown practice module. Run podbag lab list."))
    }

    pub fn for_format(format: &str) -> io::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|module| module.format() == format)
            .ok_or_else(|| io::Error::other("Unsupported practice module."))
    }

    pub fn guide_url(self, lang: &str) -> Option<String> {
        let lang = lang.split(['-', '_', '.']).next().unwrap_or("en");
        let (post, locale) = match self {
            Self::Containers | Self::Web | Self::Persistence | Self::Service => {
                (self.id(), if lang == "ko" { "ko/" } else { "" })
            }
            Self::Volumes => (
                "cli-tui-volumes",
                match lang {
                    "ko" => "ko/",
                    "ja" => "ja/",
                    "zh" => "zh-CN/",
                    "es" => "es/",
                    "fil" => "fil/",
                    "vi" => "vi/",
                    _ => "",
                },
            ),
            Self::Tui => return None,
        };
        let origin = if cfg!(debug_assertions) {
            option_env!("PODBAG_TRAINING_PREVIEW_ORIGIN").unwrap_or("https://podbag.dev")
        } else {
            "https://podbag.dev"
        };
        Some(format!("{origin}/{locale}blog/{post}/"))
    }

    fn path(self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.json", self.id()))
    }
}

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

fn read_file(path: &Path) -> io::Result<Option<Installed>> {
    match fs::read(path) {
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

fn read_legacy(dir: &Path, module: Module) -> io::Result<Option<Installed>> {
    if !matches!(module, Module::Volumes | Module::Tui) {
        return Ok(None);
    }
    Ok(read_file(&dir.join("cli-tui-volumes.json"))?
        .filter(|r| Module::for_format(&r.package.format).ok() == Some(module)))
}

pub fn read(dir: &Path, module: Module) -> io::Result<Option<Installed>> {
    match read_file(&module.path(dir))? {
        Some(record) => Ok(Some(record)),
        None => read_legacy(dir, module),
    }
}

pub fn list(dir: &Path) -> io::Result<Vec<Installed>> {
    let mut items = Vec::new();
    for module in Module::ALL {
        items.extend(read(dir, module)?);
    }
    Ok(items)
}

pub fn selected(dir: &Path, id: Option<&str>) -> io::Result<Module> {
    if let Some(id) = id {
        return Module::parse(id);
    }
    match list(dir)?.as_slice() {
        [] => Err(io::Error::other(
            "Install a module with podbag lab install FILE first.",
        )),
        [record] => Module::for_format(&record.package.format),
        _ => Err(io::Error::other("Choose a module ID from podbag lab list.")),
    }
}

fn locked(dir: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("modules.lock"))?;
    file.lock()?;
    Ok(file)
}

fn write(dir: &Path, module: Module, record: &Installed) -> io::Result<()> {
    let mut tmp = NamedTempFile::new_in(dir)?;
    serde_json::to_writer(tmp.as_file_mut(), record)?;
    tmp.flush()?;
    tmp.persist(module.path(dir))?;
    Ok(())
}

pub fn install(dir: &Path, text: &str) -> io::Result<Module> {
    if text.len() > LIMIT {
        return Err(io::Error::other("The module is too large."));
    }
    let package: Package = serde_json::from_str(text)?;
    let module = Module::for_format(&package.format)?;
    let valid_text =
        |s: &str| !s.trim().is_empty() && !s.chars().any(|c| c.is_control() && c != '\n');
    if !package.translations.contains_key("en")
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
    let completed = read(dir, module)
        .ok()
        .flatten()
        .is_some_and(|r| r.package.format == package.format && r.completed);
    write(
        dir,
        module,
        &Installed {
            package,
            generation,
            completed,
        },
    )?;
    Ok(module)
}

pub fn remove(dir: &Path, module: Module) -> io::Result<()> {
    let _lock = locked(dir)?;
    if read_legacy(dir, module)?.is_some() {
        fs::remove_file(dir.join("cli-tui-volumes.json"))?;
    }
    match fs::remove_file(module.path(dir)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

impl Installed {
    pub fn finish(&self, dir: &Path) -> io::Result<()> {
        let _lock = locked(dir)?;
        let module = Module::for_format(&self.package.format)?;
        if let Some(mut current) = read(dir, module)?
            && current.generation == self.generation
            && !current.completed
        {
            current.completed = true;
            write(dir, module, &current)?;
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
    fn guide_links_use_published_languages() {
        let path = |module: Module, lang| {
            let url = module.guide_url(lang).unwrap();
            url.split_once("://")
                .unwrap()
                .1
                .split_once('/')
                .unwrap()
                .1
                .to_owned()
        };
        assert_eq!(
            path(Module::Containers, "ko_KR.UTF-8"),
            "ko/blog/container-basics/"
        );
        assert_eq!(path(Module::Containers, "ja"), "blog/container-basics/");
        assert_eq!(
            path(Module::Volumes, "zh-CN"),
            "zh-CN/blog/cli-tui-volumes/"
        );
        assert_eq!(path(Module::Volumes, "es_ES"), "es/blog/cli-tui-volumes/");
        assert!(Module::Tui.guide_url("en").is_none());
    }

    #[test]
    fn removing_and_reinstalling_cannot_restore_an_earlier_sessions_progress() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let package = r#"{"format":"podbag-cli-tui-volumes-v1","translations":{"en":{"title":"Volumes","guide":"Open a terminal."}}}"#;
        install(dir, package).unwrap();
        let previous = read(dir, Module::Tui).unwrap().unwrap();
        remove(dir, Module::Tui).unwrap();
        previous.finish(dir).unwrap();
        assert!(read(dir, Module::Tui).unwrap().is_none());
        install(dir, package).unwrap();
        previous.finish(dir).unwrap();
        let current = read(dir, Module::Tui).unwrap().unwrap();
        assert!(!current.completed);
        assert_eq!(current.words("ko").title, "Volumes");
        current.finish(dir).unwrap();
        assert!(read(dir, Module::Tui).unwrap().unwrap().completed);
        install(dir, package).unwrap();
        assert!(read(dir, Module::Tui).unwrap().unwrap().completed);
        install(dir, &package.replace(FORMAT, gui::FORMAT)).unwrap();
        install(dir, &package.replace(FORMAT, containers::FORMAT)).unwrap();
        assert_eq!(list(dir).unwrap().len(), 3);
        assert!(selected(dir, None).is_err());
        assert!(!read(dir, Module::Volumes).unwrap().unwrap().completed);
        read(dir, Module::Containers)
            .unwrap()
            .unwrap()
            .finish(dir)
            .unwrap();
        remove(dir, Module::Containers).unwrap();
        assert!(read(dir, Module::Tui).unwrap().unwrap().completed);
        assert!(!read(dir, Module::Volumes).unwrap().unwrap().completed);
        assert!(install(dir, "{}").is_err());
    }
}
