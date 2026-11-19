//! The build cache: what each step made, by a key of what it started from and what it did.
//!
//! A key is the hash of the step's starting state (its layers and config), the expanded
//! instruction, the build arguments it uses, and the content of what it copies. An entry
//! is the state after the step. It is valid while the image it names is stored; the
//! store removes entries whose image is gone.

use crate::image::ImageConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// The state after a step.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// An image with exactly these layers; None for no layers (`FROM scratch`).
    pub image: Option<String>,
    pub diff_ids: Vec<String>,
    pub config: ImageConfig,
    /// When the last step that changed files ran (seconds since the epoch).
    pub created: u64,
}

/// The hash of a state: its layers and config (not its time).
pub fn state_id(diff_ids: &[String], config: &ImageConfig) -> String {
    let json = serde_json::to_vec(&(diff_ids, config)).unwrap_or_default();
    hex(&Sha256::digest(&json))
}

/// The key of a step. `args` are the build arguments the step sees (name, value), in any
/// order; `sources` is the digest of what it copies, or empty.
pub fn key(parent: &str, instruction: &str, args: &[(String, String)], sources: &str) -> String {
    let mut args = args.to_vec();
    args.sort();
    let mut h = Sha256::new();
    h.update(b"podbag-build-1\0");
    for part in [parent, instruction, sources] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    for (k, v) in &args {
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(v.as_bytes());
        h.update([0]);
    }
    hex(&h.finalize())
}

pub fn get(dir: &Path, key: &str) -> Option<Entry> {
    let bytes = std::fs::read(dir.join(format!("{key}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write an entry; a failure only means the next build does the step again.
pub fn put(dir: &Path, key: &str, entry: &Entry) {
    let _ = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(entry).unwrap_or_default())?;
        std::fs::rename(&tmp, dir.join(format!("{key}.json")))
    })();
}

/// Remove entries whose image is gone (and unfinished writes).
pub fn sweep(dir: &Path, exists: &dyn Fn(&str) -> bool) {
    let Ok(list) = std::fs::read_dir(dir) else {
        return;
    };
    for f in list.flatten() {
        let path = f.path();
        let keep = path.extension().is_some_and(|e| e == "json")
            && std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Entry>(&b).ok())
                .is_some_and(|e| e.image.as_deref().is_none_or(exists));
        if !keep {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Digest of a list of copied items: each one's name, kind, mode, owner and content.
#[derive(Default)]
pub struct SourceHash(Sha256);

impl std::fmt::Debug for SourceHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceHash")
    }
}

impl SourceHash {
    pub fn add(&mut self, parts: &[&str]) {
        for p in parts {
            self.0.update(p.as_bytes());
            self.0.update([0]);
        }
        self.0.update([1]);
    }

    pub fn finish(self) -> String {
        hex(&self.0.finalize())
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_change_with_each_part_but_not_arg_order() {
        let args = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let base = key("p", "RUN make", &args(&[("A", "1"), ("B", "2")]), "");
        assert_eq!(
            base,
            key("p", "RUN make", &args(&[("B", "2"), ("A", "1")]), "")
        );
        assert_ne!(
            base,
            key("q", "RUN make", &args(&[("A", "1"), ("B", "2")]), "")
        );
        assert_ne!(
            base,
            key("p", "RUN make2", &args(&[("A", "1"), ("B", "2")]), "")
        );
        assert_ne!(
            base,
            key("p", "RUN make", &args(&[("A", "1"), ("B", "3")]), "")
        );
        assert_ne!(base, key("p", "RUN make", &args(&[("A", "1")]), ""));
        assert_ne!(
            base,
            key("p", "RUN make", &args(&[("A", "1"), ("B", "2")]), "s")
        );
        // Parts cannot run into each other.
        assert_ne!(key("ab", "c", &[], ""), key("a", "bc", &[], ""));
        assert_eq!(base.len(), 64);
    }

    #[test]
    fn state_ids_follow_layers_and_config() {
        let config = ImageConfig::default();
        let a = state_id(&["sha256:1".into()], &config);
        assert_eq!(a, state_id(&["sha256:1".into()], &config));
        assert_ne!(a, state_id(&["sha256:2".into()], &config));
        let other = ImageConfig {
            user: Some("app".into()),
            ..ImageConfig::default()
        };
        assert_ne!(a, state_id(&["sha256:1".into()], &other));
    }

    #[test]
    fn source_hashes_tell_names_from_contents() {
        let mut one = SourceHash::default();
        one.add(&["a", "file", "x"]);
        let mut two = SourceHash::default();
        two.add(&["a", "filex", ""]);
        assert_ne!(one.finish(), two.finish());
    }

    #[test]
    fn entries_round_trip_and_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = |image: Option<&str>| Entry {
            image: image.map(String::from),
            diff_ids: vec![],
            config: ImageConfig::default(),
            created: 7,
        };
        put(tmp.path(), "k1", &entry(Some("sha256:kept")));
        put(tmp.path(), "k2", &entry(Some("sha256:gone")));
        put(tmp.path(), "k3", &entry(None));
        std::fs::write(tmp.path().join("k4.1.tmp"), "").unwrap();
        assert_eq!(get(tmp.path(), "k1"), Some(entry(Some("sha256:kept"))));
        assert_eq!(get(tmp.path(), "nope"), None);
        sweep(tmp.path(), &|d| d == "sha256:kept");
        let mut left: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["k1.json", "k3.json"]);
    }
}
