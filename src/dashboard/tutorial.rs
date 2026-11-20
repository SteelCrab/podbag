//! Validate and store an installed module and its completion state.
use crate::client::choice::{update_json, update_json_if, with_json_lock};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::Path,
};

#[derive(Serialize, Deserialize, PartialEq)]
pub struct Words {
    pub title: String,
    pub goal: String,
    pub create: String,
    pub write: String,
    pub serve: String,
    pub request: String,
}
#[derive(Serialize, Deserialize, PartialEq)]
pub struct Pack {
    format: String,
    pub starter: String,
    translations: BTreeMap<String, Words>,
}
#[derive(Serialize, Deserialize, PartialEq)]
pub struct Installed {
    pub pack: Pack,
    pub complete: bool,
}

pub fn read_pack(path: &Path) -> std::result::Result<Pack, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(65537).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("tutorial.invalid".into());
    }
    let pack: Pack = serde_json::from_slice(&bytes).map_err(|_| "tutorial.invalid")?;
    if valid_pack(&pack) {
        Ok(pack)
    } else {
        Err("tutorial.invalid".into())
    }
}
impl Pack {
    pub fn words(&self, language: &str) -> &Words {
        self.translations
            .get(language)
            .unwrap_or(&self.translations["en"])
    }
}

fn valid_pack(pack: &Pack) -> bool {
    pack.format == "podbag-volume-api-lab-v1"
        && pack.translations.contains_key("en")
        && serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&pack.starter).is_ok()
        && pack.translations.values().all(|w| {
            [&w.title, &w.goal, &w.create, &w.write, &w.serve, &w.request]
                .iter()
                .all(|s| !s.trim().is_empty())
        })
}

impl Installed {
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice::<Self>(&bytes)
                .ok()
                .filter(|record| valid_pack(&record.pack))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn install(path: &Path, pack: Pack) -> Result<Self, String> {
        let installed = Self {
            pack,
            complete: false,
        };
        let value = serde_json::to_value(&installed).map_err(|e| e.to_string())?;
        update_json(path, |record| *record = value)?;
        Ok(installed)
    }

    pub fn record_completion(path: &Path, pack: &Pack) -> Result<(), String> {
        update_json_if(path, |record| {
            // A session can outlive a concurrent uninstall or replacement.
            if serde_json::from_value::<Self>(record.clone()).is_ok_and(|old| &old.pack == pack) {
                record["complete"] = true.into();
                true
            } else {
                false
            }
        })
    }

    pub fn remove(path: &Path) -> Result<(), String> {
        with_json_lock(path, || match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        })
    }
}
