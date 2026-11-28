//! Declarative Desktop extensions. A manifest is data and never names files or code to run.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

const MAX_MANIFEST: u64 = 128 * 1024;
const MAX_INSTALLED: usize = 64;
const MAX_STORE: u64 = 9 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct Failure {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Failure {
    fn new(kind: &'static str) -> Self {
        Self { kind, detail: None }
    }

    pub(super) fn detail(kind: &'static str, detail: impl ToString) -> Self {
        Self {
            kind,
            detail: Some(detail.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, Failure>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    api_version: u8,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    permissions: Vec<Permission>,
    pub content: Vec<Block>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Permission {
    EngineSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Block {
    Heading { text: String },
    Text { text: String },
    List { items: Vec<String> },
    Code { text: String },
    EngineSummary {},
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Installed {
    pub manifest: Manifest,
    pub enabled: bool,
    pub engine_summary: bool,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
    List,
    Install { source: String },
    SetEnabled { id: String, enabled: bool },
    SetEngineSummary { id: String, allow: bool },
    Remove { id: String },
}

fn allowed_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(b))
}

impl Manifest {
    fn from_source(source: &str) -> Result<Self> {
        if source.len() as u64 > MAX_MANIFEST {
            return Err(Failure::new("large"));
        }
        let manifest: Self =
            serde_json::from_str(source).map_err(|error| Failure::detail("manifest", error))?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if self.api_version != 1 {
            return Err(Failure::new("version"));
        }
        let bad = |detail| Err(Failure::detail("manifest", detail));
        if !allowed_id(&self.id) {
            return bad("Use a lowercase ID with up to 64 letters, digits, periods, or hyphens.");
        }
        if !text_within(&self.name, 120)
            || !text_within(&self.version, 40)
            || !text_within(&self.description, 2_000)
        {
            return bad("Provide a name, version, and description within the documented limits.");
        }
        if !(1..=40).contains(&self.content.len()) {
            return bad("The extension needs between 1 and 40 content blocks.");
        }
        let mut summary = 0;
        for block in &self.content {
            let valid = match block {
                Block::Heading { text } => text_within(text, 120),
                Block::Text { text } | Block::Code { text } => text_within(text, 8_000),
                Block::List { items } => {
                    (1..=40).contains(&items.len())
                        && items.iter().all(|item| text_within(item, 1_000))
                }
                Block::EngineSummary {} => {
                    summary += 1;
                    summary == 1 && self.requests_summary()
                }
            };
            if !valid {
                return bad(
                    "A content block is empty, too large, or missing its declared permission.",
                );
            }
        }
        if self.permissions.len() != summary {
            return bad(
                "Declare engineSummary once when the extension has an engineSummary block.",
            );
        }
        Ok(())
    }

    pub fn requests_summary(&self) -> bool {
        self.permissions.contains(&Permission::EngineSummary)
    }
}

fn text_within(text: &str, limit: usize) -> bool {
    !text.trim().is_empty()
        && text.chars().count() <= limit
        && text
            .chars()
            .all(|c| !c.is_control() || c == '\t' || c == '\n' || c == '\r')
}

fn load(file: &Path) -> Result<Vec<Installed>> {
    let input = match File::open(file) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Failure::detail("storage", error)),
    };
    let mut json = Vec::new();
    input
        .take(MAX_STORE + 1)
        .read_to_end(&mut json)
        .map_err(|error| Failure::detail("storage", error))?;
    if json.len() as u64 > MAX_STORE {
        return Err(Failure::new("storage"));
    }
    let installed: Vec<Installed> =
        serde_json::from_slice(&json).map_err(|error| Failure::detail("storage", error))?;
    let mut ids = std::collections::HashSet::new();
    if installed.len() > MAX_INSTALLED
        || installed.iter().any(|ext| {
            ext.manifest.validate().is_err()
                || !ids.insert(&ext.manifest.id)
                || (ext.engine_summary && (!ext.enabled || !ext.manifest.requests_summary()))
        })
    {
        return Err(Failure::new("storage"));
    }
    Ok(installed)
}

fn save(dir: &Path, file: &Path, installed: &[Installed]) -> Result<()> {
    let mut tmp =
        tempfile::NamedTempFile::new_in(dir).map_err(|error| Failure::detail("storage", error))?;
    serde_json::to_writer(&mut tmp, installed)
        .map_err(|error| Failure::detail("storage", error))?;
    tmp.flush()
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|error| Failure::detail("storage", error))?;
    tmp.persist(file)
        .map_err(|error| Failure::detail("storage", error.error))?;
    Ok(())
}

/// Read only the regular file the user chose. Bound the read even if the file grows.
pub fn install_file(path: &Path) -> Result<Action> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|e| Failure::detail("manifest", e))?;
    let meta = file
        .metadata()
        .map_err(|e| Failure::detail("manifest", e))?;
    if !meta.is_file() {
        return Err(Failure::new("manifest"));
    }
    if meta.len() > MAX_MANIFEST {
        return Err(Failure::new("large"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Failure::detail("manifest", e))?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(Failure::new("large"));
    }
    let source = String::from_utf8(bytes).map_err(|e| Failure::detail("manifest", e))?;
    Ok(Action::Install { source })
}

pub fn apply(dir: &Path, action: Action) -> Result<Vec<Installed>> {
    let file = dir.join("installed.json");
    if matches!(action, Action::List) {
        return load(&file);
    }
    std::fs::create_dir_all(dir).map_err(|error| Failure::detail("storage", error))?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("installed.lock"))
        .map_err(|error| Failure::detail("storage", error))?;
    lock.lock()
        .map_err(|error| Failure::detail("storage", error))?;
    let mut installed = load(&file)?;
    update(&mut installed, action)?;
    installed.sort_by(|a, b| {
        a.manifest
            .name
            .cmp(&b.manifest.name)
            .then(a.manifest.id.cmp(&b.manifest.id))
    });
    save(dir, &file, &installed)?;
    Ok(installed)
}

pub(crate) fn update(installed: &mut Vec<Installed>, action: Action) -> Result<()> {
    let find = |items: &[Installed], id: &str| {
        items
            .iter()
            .position(|ext| ext.manifest.id == id)
            .ok_or_else(|| Failure::new("notFound"))
    };
    match action {
        Action::List => {}
        Action::Install { source } => {
            let manifest = Manifest::from_source(&source)?;
            if find(installed, &manifest.id).is_ok() {
                return Err(Failure::new("duplicate"));
            }
            if installed.len() >= MAX_INSTALLED {
                return Err(Failure::new("full"));
            }
            installed.push(Installed {
                manifest,
                enabled: false,
                engine_summary: false,
            });
        }
        Action::SetEnabled { id, enabled } => {
            let index = find(installed, &id)?;
            let ext = &mut installed[index];
            ext.enabled = enabled;
            if !enabled {
                ext.engine_summary = false;
            }
        }
        Action::SetEngineSummary { id, allow } => {
            let index = find(installed, &id)?;
            let ext = &mut installed[index];
            if !ext.enabled || !ext.manifest.requests_summary() {
                return Err(Failure::new("permission"));
            }
            ext.engine_summary = allow;
        }
        Action::Remove { id } => {
            let index = find(installed, &id)?;
            installed.remove(index);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(id: &str) -> serde_json::Value {
        json!({
            "apiVersion": 1, "id": id, "name": "Workspace checklist",
            "version": "1.0.0", "description": "Review the workspace.",
            "permissions": ["engineSummary"],
            "content": [{"type":"text","text":"Review containers before changing an image."}, {"type":"engineSummary"}]
        })
    }

    fn install(dir: &Path, source: serde_json::Value) -> Result<Vec<Installed>> {
        apply(
            dir,
            Action::Install {
                source: source.to_string(),
            },
        )
    }

    #[test]
    fn lifecycle_persists_and_grants_need_a_separate_action() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().join("extensions");
        let id = || "test.checklist".to_string();
        assert!(apply(&dir, Action::List).unwrap().is_empty());
        assert!(!dir.exists(), "listing has no side effects");
        let installed = install(&dir, manifest(&id())).unwrap();
        assert!(!installed[0].enabled && !installed[0].engine_summary);
        assert_eq!(
            install(&dir, manifest(&id())).unwrap_err().kind,
            "duplicate"
        );
        let grant = || Action::SetEngineSummary {
            id: id(),
            allow: true,
        };
        assert_eq!(apply(&dir, grant()).unwrap_err().kind, "permission");
        let enable = |enabled| Action::SetEnabled { id: id(), enabled };
        let enabled = apply(&dir, enable(true)).unwrap();
        assert!(enabled[0].enabled && !enabled[0].engine_summary);
        assert!(apply(&dir, grant()).unwrap()[0].engine_summary);
        assert!(apply(&dir, Action::List).unwrap()[0].engine_summary);
        let disabled = apply(&dir, enable(false)).unwrap();
        assert!(!disabled[0].enabled && !disabled[0].engine_summary);
        assert!(!apply(&dir, enable(true)).unwrap()[0].engine_summary);
        assert!(apply(&dir, Action::Remove { id: id() }).unwrap().is_empty());
        assert!(apply(&dir, Action::List).unwrap().is_empty());
    }

    #[test]
    fn executable_content_paths_and_undeclared_permissions_are_rejected() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().join("extensions");
        let sample = manifest("safe.example");
        for (field, value) in [
            ("id", json!("../escaped")),
            ("id", json!(r"C:\Windows\test")),
            ("entrypoint", json!("../index.js")),
            ("permissions", json!(["engineWrite"])),
            ("permissions", json!([])),
            (
                "content",
                json!([{"type":"html","html":"<b>unsafe format</b>"}]),
            ),
            (
                "content",
                json!([{"type":"engineSummary","path":"../token"}]),
            ),
        ] {
            let mut invalid = sample.clone();
            invalid[field] = value;
            assert_eq!(
                install(&dir, invalid).unwrap_err().kind,
                "manifest",
                "{field}"
            );
        }
        let mut incompatible = sample;
        incompatible["apiVersion"] = json!(2);
        assert_eq!(install(&dir, incompatible).unwrap_err().kind, "version");
        let big = " ".repeat(MAX_MANIFEST as usize + 1);
        assert_eq!(
            apply(&dir, Action::Install { source: big })
                .unwrap_err()
                .kind,
            "large"
        );
        assert!(apply(&dir, Action::List).unwrap().is_empty());
        assert!(!dir.join("escaped").exists());
    }

    #[test]
    fn plain_text_can_describe_html_without_granting_engine_access() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path();
        let mut local = manifest("test.plain");
        local["permissions"] = json!([]);
        local["content"] = json!([{"type":"code","text":"<script>let example = 1;</script>"}]);
        let stored = install(dir, local).unwrap();
        assert_eq!(
            stored[0].manifest.content[0],
            Block::Code {
                text: "<script>let example = 1;</script>".into()
            }
        );
        apply(
            dir,
            Action::SetEnabled {
                id: "test.plain".into(),
                enabled: true,
            },
        )
        .unwrap();
        assert_eq!(
            apply(
                dir,
                Action::SetEngineSummary {
                    id: "test.plain".into(),
                    allow: true
                }
            )
            .unwrap_err()
            .kind,
            "permission"
        );
    }

    #[test]
    fn concurrent_installs_keep_both_manifests() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path();
        std::thread::scope(|threads| {
            let first = threads.spawn(|| install(dir, manifest("test.first")).unwrap());
            let second = threads.spawn(|| install(dir, manifest("test.second")).unwrap());
            first.join().unwrap();
            second.join().unwrap();
        });
        assert_eq!(apply(dir, Action::List).unwrap().len(), 2);
    }
}
