//! The Training catalog on podbag.dev: the list of modules that Podbag Desktop and
//! `podbag lab list` sync, so a new module needs no new podbag.
use super::{LIMIT, Module, install_with, parse_package, read};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{fs, io::Read, path::Path, time::Duration};

pub const FORMAT: &str = "podbag-training-catalog-v1";
pub const URL: &str = "https://podbag.dev/training/catalog.json";
/// The synced catalog, kept in the tutorial library for its order.
const SAVED: &str = "catalog.json";
const CATALOG_LIMIT: u64 = 256 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub file: String,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Catalog {
    pub format: String,
    pub modules: Vec<Entry>,
}

/// What a sync did, by module id.
#[derive(Debug, Default, Serialize)]
pub struct Synced {
    /// Installed or replaced.
    pub updated: Vec<String>,
    /// A format this podbag does not know; a newer podbag shows it.
    pub skipped: Vec<String>,
    /// Not installed, and why.
    pub failed: Vec<(String, String)>,
}

/// The catalog's address: `PODBAG_TRAINING_CATALOG`, or podbag.dev.
pub fn catalog_url() -> String {
    std::env::var("PODBAG_TRAINING_CATALOG")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| URL.into())
}

/// Installs the catalog's new and changed modules. Offline, nothing changes and the error
/// says why.
pub fn sync(dir: &Path) -> Result<Synced, String> {
    sync_from(dir, &catalog_url())
}

/// HTTPS, or a local site on this computer.
fn allowed(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://127.0.0.1:")
}

fn get(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    if !allowed(url) {
        return Err(format!("refusing {url}: Training is read over HTTPS only"));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(format!("podbag/{}", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_resolve(Some(Duration::from_secs(5)))
        .timeout_per_read(Some(Duration::from_secs(5)))
        .build()
        .into();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("{url} answered {}", resp.status()));
    }
    let mut bytes = Vec::new();
    resp.into_body()
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {url}: {e}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{url} is too large"));
    }
    Ok(bytes)
}

/// The address of FILE next to the catalog; only plain names under its folder.
fn file_url(catalog: &str, file: &str) -> Option<String> {
    let plain = !file.is_empty()
        && file
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && file
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'/'));
    let base = &catalog[..catalog.rfind('/')? + 1];
    plain.then(|| format!("{base}{file}"))
}

fn sha256(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn sync_from(dir: &Path, url: &str) -> Result<Synced, String> {
    let catalog: Catalog = serde_json::from_slice(&get(url, CATALOG_LIMIT)?)
        .map_err(|e| format!("cannot read the Training catalog: {e}"))?;
    if catalog.format != FORMAT {
        return Err("this podbag cannot read the Training catalog; update podbag".into());
    }
    let mut synced = Synced::default();
    for entry in &catalog.modules {
        let id = entry.id.clone();
        let Ok(module) = Module::parse(&id) else {
            synced.failed.push((id, "bad module id".into()));
            continue;
        };
        let installed = read(dir, &module).ok().flatten();
        if installed
            .as_ref()
            .is_some_and(|r| r.sha256.as_deref() == Some(entry.sha256.as_str()))
        {
            continue;
        }
        let result = (|| {
            let file = file_url(url, &entry.file).ok_or("bad file name")?;
            let bytes = get(&file, LIMIT as u64)?;
            if sha256(&bytes) != entry.sha256.to_ascii_lowercase() {
                return Err("the file does not match the catalog".to_owned());
            }
            let text = String::from_utf8(bytes).map_err(|_| "not UTF-8")?;
            let format = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v["format"].as_str().map(String::from))
                .unwrap_or_default();
            if !Module::known_format(&format) {
                return Ok(false);
            }
            let package = parse_package(&text).map_err(|e| e.to_string())?;
            if !usable_here(&package, cfg!(windows)) {
                return Ok(false);
            }
            if Module::of(&package).map_err(|e| e.to_string())? != module {
                return Err("the file is another module".into());
            }
            install_with(dir, package, Some(entry.sha256.to_ascii_lowercase()))
                .map_err(|e| e.to_string())?;
            Ok(true)
        })();
        match result {
            Ok(true) => synced.updated.push(id),
            Ok(false) => synced.skipped.push(id),
            Err(why) => synced.failed.push((id, why)),
        }
    }
    let saved = serde_json::to_vec(&catalog).map_err(|e| e.to_string())?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    fs::write(dir.join(SAVED), saved).map_err(|e| e.to_string())?;
    Ok(synced)
}

/// Whether this computer can do every step of PACKAGE: the Windows practice session runs
/// podbag alone in WSL, without a Docker CLI.
pub fn usable_here(package: &super::Package, windows: bool) -> bool {
    !(windows && package.steps.iter().any(super::course::StepDef::is_docker))
}

/// The module ids of the last synced catalog, in its order; empty before the first sync.
pub fn order(dir: &Path) -> Vec<String> {
    fs::read(dir.join(SAVED))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Catalog>(&bytes).ok())
        .map(|catalog| catalog.modules.into_iter().map(|e| e.id).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume_lesson::{Module, install, list, read};
    use sha2::Digest;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const WEB: &str = include_str!("../../tests/fixtures/training/web-basics.podbag");
    const NETWORK: &str = include_str!("../../tests/fixtures/training/container-network.podbag");

    type Files = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// A small web server on 127.0.0.1 for FILES (path, bytes); returns its base URL.
    fn serve(files: Files) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let found = files
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(p, _)| *p == path)
                    .cloned();
                let (status, body) = match found {
                    Some((_, body)) => ("200 OK", body),
                    None => ("404 Not Found", Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        base
    }

    fn sha(bytes: &[u8]) -> String {
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A site with the catalog of MODULES (id, file name, bytes, sha256 given in the catalog).
    fn site(modules: &[(&str, &str, &[u8], Option<&str>)]) -> (String, Files) {
        let files: Files = Arc::default();
        let base = serve(files.clone());
        publish(&files, modules);
        (format!("{base}/training/catalog.json"), files)
    }

    fn publish(files: &Files, modules: &[(&str, &str, &[u8], Option<&str>)]) {
        let entries: Vec<_> = modules
            .iter()
            .map(|(id, file, bytes, hash)| Entry {
                id: (*id).into(),
                file: format!("modules/{file}"),
                sha256: hash.map(String::from).unwrap_or_else(|| sha(bytes)),
            })
            .collect();
        let catalog = Catalog {
            format: FORMAT.into(),
            modules: entries,
        };
        let mut served = vec![(
            "/training/catalog.json".to_owned(),
            serde_json::to_vec(&catalog).unwrap(),
        )];
        for (_, file, bytes, _) in modules {
            served.push((format!("/training/modules/{file}"), bytes.to_vec()));
        }
        *files.lock().unwrap() = served;
    }

    fn ids(dir: &Path) -> Vec<String> {
        list(dir)
            .unwrap()
            .iter()
            .map(|r| Module::of(&r.package).unwrap().id().to_owned())
            .collect()
    }

    #[test]
    fn sync_installs_new_and_changed_modules() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let (url, files) = site(&[
            (
                "container-network",
                "container-network.podbag",
                NETWORK.as_bytes(),
                None,
            ),
            ("web-basics", "web-basics.podbag", WEB.as_bytes(), None),
        ]);
        let synced = sync_from(dir, &url).unwrap();
        assert_eq!(synced.updated, ["container-network", "web-basics"]);
        assert_eq!(
            ids(dir),
            ["container-network", "web-basics"],
            "the catalog's order"
        );
        assert!(sync_from(dir, &url).unwrap().updated.is_empty());

        let changed = NETWORK.replace("Connect two containers by name", "Connect by name");
        publish(
            &files,
            &[
                (
                    "container-network",
                    "container-network.podbag",
                    changed.as_bytes(),
                    None,
                ),
                ("web-basics", "web-basics.podbag", WEB.as_bytes(), None),
            ],
        );
        assert_eq!(sync_from(dir, &url).unwrap().updated, ["container-network"]);
        let record = read(dir, &Module::Course("container-network".into()))
            .unwrap()
            .unwrap();
        assert_eq!(
            record.words("en").title,
            "Podbag Training · Connect by name"
        );
    }

    #[test]
    fn sync_skips_unknown_formats() {
        let dir = tempfile::tempdir().unwrap();
        let future = br#"{"format":"podbag-hologram-v9","translations":{"en":{"title":"Next","guide":"Later."}}}"#;
        let (url, _) = site(&[("hologram", "hologram.podbag", future, None)]);
        let synced = sync_from(dir.path(), &url).unwrap();
        assert_eq!(synced.skipped, ["hologram"]);
        assert!(ids(dir.path()).is_empty());
    }

    #[test]
    fn sync_refuses_a_wrong_hash_and_paths_outside_the_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let (url, files) = site(&[(
            "web-basics",
            "web-basics.podbag",
            WEB.as_bytes(),
            Some(&"0".repeat(64)),
        )]);
        let synced = sync_from(dir.path(), &url).unwrap();
        assert_eq!(synced.failed.len(), 1, "{:?}", synced.failed);
        assert!(ids(dir.path()).is_empty());

        publish(
            &files,
            &[("web-basics", "../web-basics.podbag", WEB.as_bytes(), None)],
        );
        let synced = sync_from(dir.path(), &url).unwrap();
        assert_eq!(synced.failed.len(), 1, "{:?}", synced.failed);
        assert!(ids(dir.path()).is_empty());
    }

    #[test]
    fn sync_keeps_completed_and_replaces_old_formats() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let old = r#"{"format":"podbag-web-basics-v1","translations":{"en":{"title":"Web","guide":"Start."}}}"#;
        install(dir, old).unwrap();
        let web = Module::Course("web-basics".into());
        read(dir, &web).unwrap().unwrap().finish(dir).unwrap();
        let (url, _) = site(&[("web-basics", "web-basics.podbag", WEB.as_bytes(), None)]);
        assert_eq!(sync_from(dir, &url).unwrap().updated, ["web-basics"]);
        let record = read(dir, &web).unwrap().unwrap();
        assert!(record.completed);
        assert!(record.course().is_ok());
    }

    #[test]
    fn offline_sync_keeps_the_library() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), WEB).unwrap();
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/training/catalog.json",
            closed.local_addr().unwrap()
        );
        drop(closed);
        assert!(sync_from(dir.path(), &url).is_err());
        assert_eq!(ids(dir.path()), ["web-basics"]);
    }
}
