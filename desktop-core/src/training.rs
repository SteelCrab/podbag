//! Training: the practice labs and the modules a user installs, and their web guides.

/// A practice lab's view: `action` on the lab of the GUI lesson, or with `containers`
/// the containers lesson, or with `course` the course. On Windows it runs in WSL.
pub fn practice_volumes(
    action: &str,
    session: Option<&str>,
    name: Option<&str>,
    containers: bool,
    course: bool,
) -> Result<serde_json::Value, String> {
    use podbag::volume_lesson as lesson;
    let dir = lesson::directory().map_err(|e| e.to_string())?;
    let request = |action: &str| -> Result<serde_json::Value, String> {
        #[cfg(unix)]
        {
            let target = name.unwrap_or("");
            let result = if course {
                lesson::course::view(&dir, action, session, target).map(serde_json::to_value)
            } else if containers {
                lesson::containers::view(&dir, action, session, target).map(serde_json::to_value)
            } else {
                lesson::gui::view(&dir, action, session, target).map(serde_json::to_value)
            };
            result
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let active = if course {
                lesson::course::ACTIVE
            } else if containers {
                lesson::containers::ACTIVE
            } else {
                lesson::gui::ACTIVE
            };
            if !dir.join(active).exists() && action == "status" {
                return Ok(serde_json::Value::Null);
            }
            let path = podbag::wsl::wsl_path(&dir.to_string_lossy())?;
            let mut command = std::process::Command::new("wsl.exe");
            command.args([
                "-d",
                podbag::wsl::DISTRO,
                "--exec",
                podbag::wsl::ENGINE,
                "lab",
                "desktop-view",
            ]);
            if course {
                command.arg("--course");
            } else if containers {
                command.arg("--containers");
            }
            let out = command
                .args([&path, action])
                .args(session)
                .args(name)
                .creation_flags(0x08000000)
                .output()
                .map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
            }
            serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
        }
    };
    if course && action == "open-web" {
        let view = request("show-web")?;
        let port = view["web"]["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .ok_or("The practice port is unavailable.")?;
        open::that_detached(format!("http://127.0.0.1:{port}/")).map_err(|e| e.to_string())?;
    }
    request(action)
}

/// The installed training modules, after `action`: `status`, `summary` (of `id` only),
/// `install` (the module file `package`), `remove` (`id`) or `sync` (new and changed
/// modules from the Training catalog on podbag.dev, without an app update).
pub fn volume_tools_lab(
    action: &str,
    package: Option<&str>,
    id: Option<&str>,
) -> Result<Vec<LessonRecord>, String> {
    use podbag::volume_lesson as lesson;
    let dir = lesson::directory().map_err(|e| e.to_string())?;
    let module = || lesson::selected(&dir, id).map_err(|e| e.to_string());
    match action {
        "status" | "summary" => {}
        "install" => {
            lesson::install(&dir, package.ok_or("Choose a module file.")?)
                .map_err(|e| e.to_string())?;
        }
        "remove" => lesson::remove(&dir, &module()?).map_err(|e| e.to_string())?,
        "sync" => {
            lesson::catalog::sync(&dir)?;
        }
        _ => return Err("Unknown lesson action.".into()),
    }
    let mut records = Vec::new();
    for mut record in lesson::list(&dir).map_err(|e| e.to_string())? {
        let key = lesson::Module::of(&record.package)
            .map_err(|e| e.to_string())?
            .id()
            .to_owned();
        if id.is_some_and(|id| action == "summary" && id != key) {
            continue;
        }
        record.package.animation = None;
        // An old course format has no steps until a sync replaces it.
        let steps = record
            .package
            .course()
            .map(|course| lesson::course::instructions(&course))
            .unwrap_or_default();
        records.push(LessonRecord {
            id: key,
            record,
            steps,
        });
    }
    Ok(records)
}

/// An installed module, with the steps of its course.
#[derive(serde::Serialize)]
pub struct LessonRecord {
    pub id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<podbag::volume_lesson::course::Instruction>,
    #[serde(flatten)]
    pub record: podbag::volume_lesson::Installed,
}

/// Open the web guide of the module `id`, in `lang`.
pub fn open_training_guide(id: &str, lang: &str) -> Result<(), String> {
    use podbag::volume_lesson as lesson;
    let module = lesson::Module::parse(id).map_err(|e| e.to_string())?;
    let dir = lesson::directory().map_err(|e| e.to_string())?;
    // An installed module may carry a newer guide link than the built-in one.
    let url = match lesson::read(&dir, &module).map_err(|e| e.to_string())? {
        Some(record) => record.guide_url(lang),
        None => module.guide_url(lang),
    }
    .ok_or("The module has no web guide.")?;
    open::that_detached(url).map_err(|e| e.to_string())
}

/// A tutorial lab step by its name (`status`, `start`, `create`, …).
pub fn lab_action(name: &str) -> Result<podbag::dashboard::lab::Action, String> {
    serde_json::from_value(serde_json::Value::String(name.to_string()))
        .map_err(|_| format!("Unknown lab action {name}."))
}

/// Where the JSON lesson is kept, next to `settings.json`.
pub fn lesson_file(config_dir: &std::path::Path) -> std::path::PathBuf {
    config_dir.join("tutorial-volume-api.json")
}

/// The JSON lesson saved in `file`, if it can be read.
pub fn read_lesson(file: &std::path::Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Save the JSON lesson to `file`, or remove it.
pub fn write_lesson(
    file: &std::path::Path,
    lesson: Option<&serde_json::Value>,
) -> Result<(), String> {
    let Some(lesson) = lesson else {
        return match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    };
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec(lesson).map_err(|e| e.to_string())?;
    std::fs::write(file, json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_actions_are_named_as_the_window_names_them() {
        assert!(lab_action("request").is_ok());
        assert_eq!(
            lab_action("explode").err().as_deref(),
            Some("Unknown lab action explode.")
        );
    }

    #[test]
    fn the_json_lesson_is_saved_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = lesson_file(dir.path());
        assert!(read_lesson(&file).is_none());
        let lesson = serde_json::json!({ "pack": { "format": "x" }, "complete": false });
        write_lesson(&file, Some(&lesson)).unwrap();
        assert_eq!(read_lesson(&file), Some(lesson));
        write_lesson(&file, None).unwrap();
        write_lesson(&file, None).unwrap();
        assert!(read_lesson(&file).is_none());
    }
}
