//! `docker load`: an image archive into the store.
//!
//! The archive may hold `docker save`'s `manifest.json`, an OCI image layout (`index.json`
//! and `blobs/`), or both, as BuildKit and newer Docker write them. It is untrusted input:
//! its files go to a scratch directory under numbered names (no archive path is used on
//! disk), every digest it names is checked, and its layers pass the same checks as a
//! pull's (blob digest, diff ID, safe paths).

use super::{Source, Store, io_error, now, remove_tree};
use crate::image::manifest::{self, Compression, Descriptor, Fetched, Platform};
use crate::image::reference::{Reference, is_digest};
use crate::image::registry::sha256_hex;
use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

/// What a load stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadReport {
    /// The names it gave, in full (`docker.io/library/alpine:3`).
    pub names: Vec<String>,
    /// Images that came without a name: their digests.
    pub untagged: Vec<String>,
}

/// The largest JSON file read from an archive (index, manifests, configs).
const MAX_JSON: u64 = 16 * 1024 * 1024;
/// How many links one archive path may pass through.
const MAX_LINKS: usize = 40;
/// Space an unpacking archive leaves free on the store's disk.
const DISK_RESERVE: u64 = 512 * 1024 * 1024;

/// One image of the archive, as its index or manifest names it.
#[derive(Debug)]
struct Found {
    /// The manifest digest, when the archive has the manifest (OCI layout).
    manifest: Option<String>,
    /// The archive path of its config, and the digest the archive gives it.
    config: String,
    config_digest: Option<String>,
    layers: Vec<Blob>,
    names: Vec<String>,
}

#[derive(Debug)]
struct Blob {
    path: String,
    digest: Option<String>,
    size: Option<u64>,
    /// From the manifest's media type; otherwise read from the first bytes.
    compression: Option<Compression>,
}

impl Store {
    /// Store the images of an archive read from `input` (it may be gzip-compressed) and
    /// give them the names it holds; a name that pointed elsewhere moves.
    pub fn load(
        &self,
        input: &mut dyn Read,
        limit: u64,
        in_use: &dyn Fn() -> HashMap<String, String>,
    ) -> Result<LoadReport> {
        let scratch = self.scratch_dir(&format!("load.{}", std::process::id()));
        let stored = {
            // Shared for the whole load: an exclusive holder (garbage collection) empties
            // the scratch directory.
            let _shared = self.lock(false)?;
            let _ = remove_tree(&scratch);
            std::fs::create_dir_all(&scratch).map_err(|e| io_error(&scratch, e))?;
            let stored = Files::unpack(input, &scratch, limit).and_then(|files| {
                let images = find_images(&files)?;
                self.store_loaded(&files, &images)
            });
            let _ = remove_tree(&scratch);
            stored
        };
        match stored {
            Ok((report, digests, replaced)) => {
                let _exclusive = self.lock(true)?;
                let in_use = in_use();
                // An image whose last name moved to a loaded one goes, as with `tag`, unless
                // a container uses it.
                let unnamed = self.unnamed_images()?;
                let mut removed = false;
                for old in replaced {
                    if unnamed.contains(&old)
                        && !in_use.contains_key(&old)
                        && !digests.contains(&old)
                    {
                        self.delete_image(&old)?;
                        removed = true;
                    }
                }
                if removed {
                    self.collect_garbage()?;
                }
                if let Some(last) = digests.last() {
                    self.enforce_limit_locked(limit, &in_use, last)?;
                }
                Ok(report)
            }
            Err(e) => {
                // What this load left behind goes, as after a failed pull.
                if let Ok(_exclusive) = self.lock(true) {
                    let _ = self.collect_garbage();
                }
                Err(e)
            }
        }
    }

    /// Store the images; returns the report, their digests, and the digests the names
    /// pointed to before.
    fn store_loaded(
        &self,
        files: &Files,
        images: &[Found],
    ) -> Result<(LoadReport, Vec<String>, Vec<String>)> {
        let mut report = LoadReport::default();
        let mut digests = Vec::new();
        let mut replaced = Vec::new();
        for image in images {
            let config_bytes = files.read_small(&image.config)?;
            let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
            if let Some(want) = &image.config_digest {
                if *want != config_digest {
                    return Err(Source::Archive.mismatch(want));
                }
            }
            let config = manifest::parse_config(&config_bytes)?;
            if config.diff_ids.len() != image.layers.len() {
                return Err(bad(format!(
                    "the image config names {} layers, its manifest {}",
                    config.diff_ids.len(),
                    image.layers.len()
                )));
            }
            for (blob, diff_id) in image.layers.iter().zip(&config.diff_ids) {
                if self.layer_path(diff_id).exists() {
                    continue;
                }
                let path = files.find(&blob.path)?;
                let (digest, size) = file_digest(path)?;
                if let Some(want) = &blob.digest {
                    if *want != digest {
                        return Err(Source::Archive.mismatch(want));
                    }
                }
                if blob.size.is_some_and(|want| want != size) {
                    return Err(bad(format!(
                        "layer {} has {size} bytes, its manifest says {}",
                        blob.path,
                        blob.size.unwrap_or_default()
                    )));
                }
                let compression = match blob.compression {
                    Some(c) => c,
                    None => sniff(path)?,
                };
                let file = File::open(path).map_err(|e| io_error(path, e))?;
                self.ingest_layer(
                    Source::Archive,
                    Box::new(file),
                    compression,
                    &digest,
                    size,
                    diff_id,
                )?;
            }
            // Its ID: the manifest's digest when the archive has the manifest, as a pull
            // would give it; else the config's, as Docker shows image IDs.
            let digest = image
                .manifest
                .clone()
                .unwrap_or_else(|| config_digest.clone());
            if self.image_dir(&digest).join("image.json").exists() {
                self.ensure_artifact(&digest)?;
            } else {
                self.assemble(
                    &digest,
                    config.config,
                    config.diff_ids,
                    Some(config_digest.clone()),
                )?;
            }
            for name in &image.names {
                let full = Reference::parse(name)?.name();
                let before = self.read_refs()?.get(&full).map(|r| r.digest.clone());
                if let Some(old) = before.filter(|old| *old != digest) {
                    replaced.push(old);
                }
                self.set_ref(&full, &digest, now())?;
                report.names.push(full);
            }
            if image.names.is_empty() {
                report.untagged.push(digest.clone());
            }
            digests.push(digest);
        }
        Ok((report, digests, replaced))
    }
}

/// The images an archive holds: from its OCI index, then from `manifest.json` (whose names
/// join the image with the same config).
fn find_images(files: &Files) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    if files.find("index.json").is_ok() {
        found = from_index(files)?;
    }
    if files.find("manifest.json").is_ok() {
        let entries: Vec<DockerEntry> = json(&files.read_small("manifest.json")?, "manifest.json")?;
        for e in entries {
            let config_digest = digest_in_name(&e.config);
            let names = e.repo_tags.unwrap_or_default();
            if let Some(same) = found
                .iter_mut()
                .find(|f| config_digest.is_some() && f.config_digest == config_digest)
            {
                for n in names {
                    if !same.names.contains(&n) {
                        same.names.push(n);
                    }
                }
                continue;
            }
            found.push(Found {
                manifest: None,
                config: e.config,
                config_digest,
                layers: e
                    .layers
                    .into_iter()
                    .map(|path| Blob {
                        digest: digest_in_name(&path),
                        path,
                        size: None,
                        compression: None,
                    })
                    .collect(),
                names,
            });
        }
    }
    if found.is_empty() {
        return Err(bad(
            "it has no images: neither manifest.json nor index.json lists one".to_string(),
        ));
    }
    Ok(found)
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerEntry {
    config: String,
    #[serde(default)]
    repo_tags: Option<Vec<String>>,
    layers: Vec<String>,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    platform: Option<Platform>,
    #[serde(default)]
    annotations: HashMap<String, String>,
}

fn from_index(files: &Files) -> Result<Vec<Found>> {
    let index: Index = json(&files.read_small("index.json")?, "index.json")?;
    let (arch, variant) = manifest::host_platform();
    let mut found = Vec::new();
    for entry in index.manifests {
        let names = names_of(&entry.annotations);
        let mut desc = Descriptor {
            media_type: entry.media_type,
            digest: entry.digest,
            size: entry.size,
            platform: entry.platform,
        };
        let mut fetched = read_manifest(files, &desc)?;
        if let Fetched::Index(list) = &fetched {
            // An image for several platforms: this machine's.
            desc = manifest::select_platform("the archive's image", list, arch, variant)?;
            fetched = read_manifest(files, &desc)?;
        }
        let Fetched::Manifest { config, layers } = fetched else {
            return Err(bad("an index lists another index".to_string()));
        };
        let layers = layers
            .into_iter()
            .map(|l| {
                Ok(Blob {
                    path: blob_path(&l.digest)?,
                    compression: Some(manifest::layer_compression(&l.media_type)?),
                    digest: Some(l.digest),
                    size: Some(l.size),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        found.push(Found {
            manifest: Some(desc.digest),
            config: blob_path(&config.digest)?,
            config_digest: Some(config.digest),
            layers,
            names,
        });
    }
    Ok(found)
}

/// A manifest or index blob, checked against its digest.
fn read_manifest(files: &Files, desc: &Descriptor) -> Result<Fetched> {
    let body = files.read_small(&blob_path(&desc.digest)?)?;
    if format!("sha256:{}", sha256_hex(&body)) != desc.digest {
        return Err(Source::Archive.mismatch(&desc.digest));
    }
    let media_type = Some(desc.media_type.as_str()).filter(|t| !t.is_empty());
    manifest::parse_manifest(media_type, &body)
}

/// The names an index entry gives its image: containerd's full name, or an OCI ref name
/// that is a whole reference (one that is only a tag names nothing).
fn names_of(annotations: &HashMap<String, String>) -> Vec<String> {
    let full = annotations.get("io.containerd.image.name").or_else(|| {
        annotations
            .get("org.opencontainers.image.ref.name")
            .filter(|n| n.contains('/') || n.contains(':'))
    });
    full.into_iter().cloned().collect()
}

fn blob_path(digest: &str) -> Result<String> {
    if !is_digest(digest) {
        return Err(bad(format!("invalid digest {digest:?}")));
    }
    Ok(format!("blobs/sha256/{}", &digest["sha256:".len()..]))
}

/// The digest an archive path stands for: `blobs/sha256/<hex>`, or `<hex>.json` (the
/// config in Docker's older format). Layer directories of that format are not digests.
fn digest_in_name(path: &str) -> Option<String> {
    let path = path.trim_start_matches("./");
    let hex = path
        .strip_prefix("blobs/sha256/")
        .or_else(|| path.strip_suffix(".json"))?;
    let digest = format!("sha256:{hex}");
    is_digest(&digest).then_some(digest)
}

fn json<T: serde::de::DeserializeOwned>(data: &[u8], name: &str) -> Result<T> {
    serde_json::from_slice(data).map_err(|e| bad(format!("{name} is not valid: {e}")))
}

/// How a layer file is compressed, from its first bytes.
fn sniff(path: &Path) -> Result<Compression> {
    let mut magic = [0u8; 4];
    let n = File::open(path)
        .and_then(|mut f| f.read(&mut magic))
        .map_err(|e| io_error(path, e))?;
    Ok(match &magic[..n] {
        [0x1f, 0x8b, ..] => Compression::Gzip,
        [0x28, 0xb5, 0x2f, 0xfd] => Compression::Zstd,
        _ => Compression::None,
    })
}

fn file_digest(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| io_error(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((format!("sha256:{hex}"), size))
}

fn bad(why: String) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "cannot load the image archive",
        why,
        "load an archive made by `docker save` or an OCI image layout",
    )
}

/// The archive's regular files, by their cleaned path, and its links.
struct Files {
    paths: HashMap<String, PathBuf>,
    links: HashMap<String, String>,
}

impl Files {
    /// Unpack the archive's files into `scratch`, at most `limit` bytes of them (the store
    /// limit: an archive larger than it could not be kept), and stop before the disk is
    /// nearly full (a small gzip stream can unpack to a great deal).
    fn unpack(input: &mut dyn Read, scratch: &Path, limit: u64) -> Result<Files> {
        let mut input = BufReader::new(input);
        let start = input.fill_buf().map_err(|e| bad(e.to_string()))?;
        let reader: Box<dyn Read + '_> = if start.starts_with(&[0x1f, 0x8b]) {
            Box::new(flate2::read::MultiGzDecoder::new(input))
        } else {
            Box::new(input)
        };
        let mut archive = tar::Archive::new(reader);
        let mut files = Files {
            paths: HashMap::new(),
            links: HashMap::new(),
        };
        let entries = archive.entries().map_err(|e| bad(e.to_string()))?;
        let mut total = 0u64;
        let mut checked = 0u64;
        for (n, entry) in entries.enumerate() {
            let mut entry = entry.map_err(|e| bad(e.to_string()))?;
            let raw = entry.path_bytes().into_owned();
            let Some(path) = clean(&raw, "", false)? else {
                continue;
            };
            let kind = entry.header().entry_type();
            if kind.is_file() || kind == tar::EntryType::Continuous {
                let size = entry.header().size().map_err(|e| bad(e.to_string()))?;
                total = total.saturating_add(size);
                if total > limit {
                    return Err(bad(format!(
                        "it unpacks to more than the store limit ({limit} bytes)"
                    )));
                }
                if total - checked >= 64 * 1024 * 1024 || checked == 0 {
                    checked = total.max(1);
                    if free_space(scratch).is_some_and(|free| free < size + DISK_RESERVE) {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            "cannot load the image archive",
                            "the store's disk is nearly full",
                            "free some space (`openpod system prune`), then try again",
                        ));
                    }
                }
                let dest = scratch.join(n.to_string());
                let mut out = File::create(&dest).map_err(|e| io_error(&dest, e))?;
                // Never more than the header says, whatever the stream holds.
                std::io::copy(&mut (&mut entry).take(size), &mut out)
                    .map_err(|e| bad(e.to_string()))?;
                files.paths.insert(path, dest);
            } else if kind.is_symlink() || kind.is_hard_link() {
                let target = entry
                    .link_name_bytes()
                    .map(|t| t.into_owned())
                    .unwrap_or_default();
                // A symbolic link is relative to its directory; a hard link to the root.
                let base = if kind.is_symlink() {
                    path.rsplit_once('/').map_or("", |(dir, _)| dir).to_string()
                } else {
                    String::new()
                };
                if let Some(target) = clean(&target, &base, true)? {
                    files.links.insert(path, target);
                }
            }
            // Directories, and anything else: nothing an image archive needs.
        }
        Ok(files)
    }

    /// The scratch file of an archive path, through its links.
    fn find(&self, path: &str) -> Result<&Path> {
        let mut at = clean(path.as_bytes(), "", false)?.unwrap_or_default();
        for _ in 0..MAX_LINKS {
            if let Some(file) = self.paths.get(&at) {
                return Ok(file);
            }
            match self.links.get(&at) {
                Some(next) => at = next.clone(),
                None => break,
            }
        }
        Err(bad(format!("it names {path}, which it does not hold")))
    }

    fn read_small(&self, path: &str) -> Result<Vec<u8>> {
        let file = self.find(path)?;
        let mut data = Vec::new();
        File::open(file)
            .and_then(|f| f.take(MAX_JSON + 1).read_to_end(&mut data))
            .map_err(|e| io_error(file, e))?;
        if data.len() as u64 > MAX_JSON {
            return Err(bad(format!("{path} is larger than 16 MiB")));
        }
        Ok(data)
    }
}

/// Free bytes on the disk that holds `dir`, when the system says.
fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs fills a plain struct; zero is a valid starting value.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: a NUL-terminated path and a struct of the right type.
        let done = unsafe { libc::statvfs(path.as_ptr(), &mut st) } == 0;
        #[allow(clippy::unnecessary_cast)] // the field types differ between systems
        done.then(|| (st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// An archive path made relative and plain, below `base`. Absolute paths are refused, and
/// so is `..`, except in link targets where it may not climb above the archive's root.
/// None for the root itself.
fn clean(raw: &[u8], base: &str, links: bool) -> Result<Option<String>> {
    let unsafe_path = || bad(format!("unsafe path {:?}", String::from_utf8_lossy(raw)));
    let text = std::str::from_utf8(raw).map_err(|_| unsafe_path())?;
    if text.starts_with('/') || text.contains('\0') {
        return Err(unsafe_path());
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." if links => {
                parts.pop().ok_or_else(unsafe_path)?;
            }
            ".." => return Err(unsafe_path()),
            p => parts.push(p),
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn open(tmp: &tempfile::TempDir) -> Store {
        Store::open_at(tmp.path().join("store"), false, false).unwrap()
    }

    fn none() -> HashMap<String, String> {
        HashMap::new()
    }

    /// A tar of `(path, data)` files; a path ending in `@target` becomes a symbolic link.
    fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_mode(0o644);
            h.set_uid(0);
            h.set_gid(0);
            h.set_mtime(0);
            match path.split_once('@') {
                Some((link, target)) => {
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_size(0);
                    b.append_link(&mut h, link, target).unwrap();
                }
                None => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(data.len() as u64);
                    // Raw name bytes: set_path would refuse a hostile one.
                    h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                    h.set_cksum();
                    b.append(&h, *data).unwrap();
                }
            }
        }
        b.into_inner().unwrap()
    }

    fn digest(data: &[u8]) -> String {
        format!("sha256:{}", sha256_hex(data))
    }

    fn hex(data: &[u8]) -> String {
        sha256_hex(data)
    }

    fn config(diff_ids: &[String], cmd: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "architecture": manifest::host_platform().0,
            "os": "linux",
            "config": {"Cmd": [cmd]},
            "rootfs": {"type": "layers", "diff_ids": diff_ids},
        }))
        .unwrap()
    }

    /// `docker save`'s older format: `<hex>.json` config, `<id>/layer.tar` layers.
    fn docker_archive(layer: &[u8], cmd: &str, tags: serde_json::Value) -> Vec<u8> {
        let cfg = config(&[digest(layer)], cmd);
        let manifest = serde_json::json!([{
            "Config": format!("{}.json", hex(&cfg)),
            "RepoTags": tags,
            "Layers": ["0123/layer.tar"],
        }]);
        tar_of(&[
            (&format!("{}.json", hex(&cfg)), &cfg),
            ("0123/layer.tar", layer),
            ("manifest.json", &serde_json::to_vec(&manifest).unwrap()),
        ])
    }

    /// An OCI layout with a gzip layer, as BuildKit writes it.
    fn oci_layout(layer: &[u8], name: &str) -> (Vec<u8>, String) {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(layer).unwrap();
        let gz = gz.finish().unwrap();
        let cfg = config(&[digest(layer)], "/oci");
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": digest(&cfg), "size": cfg.len()},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "digest": digest(&gz), "size": gz.len()}],
        }))
        .unwrap();
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                           "digest": digest(&manifest), "size": manifest.len(),
                           "annotations": {"io.containerd.image.name": name,
                                           "org.opencontainers.image.ref.name": "latest"}}],
        }))
        .unwrap();
        let tar = tar_of(&[
            ("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#),
            (&format!("blobs/sha256/{}", hex(&cfg)), &cfg),
            (&format!("blobs/sha256/{}", hex(&gz)), &gz),
            (&format!("blobs/sha256/{}", hex(&manifest)), &manifest),
            ("index.json", &index),
        ]);
        (tar, digest(&manifest))
    }

    fn layer(files: &[(&str, &[u8])]) -> Vec<u8> {
        tar_of(files)
    }

    #[test]
    fn loads_a_docker_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(&tmp);
        let l = layer(&[("hello", b"hi")]);
        let archive = docker_archive(&l, "/hello", serde_json::json!(["local/hello:latest"]));
        let report = store
            .load(&mut archive.as_slice(), u64::MAX, &none)
            .unwrap();
        assert_eq!(report.names, ["docker.io/local/hello:latest"]);
        let (name, record) = store.find("local/hello:latest").unwrap();
        assert_eq!(name, "docker.io/local/hello:latest");
        assert_eq!(record.config.cmd, ["/hello"]);
        assert_eq!(record.diff_ids, [digest(&l)]);
        let root = store.rootfs_path(&record.digest);
        assert_eq!(std::fs::read(root.join("hello")).unwrap(), b"hi");
        // The scratch directory is gone.
        assert!(
            !store
                .scratch_dir(&format!("load.{}", std::process::id()))
                .exists()
        );
    }

    #[test]
    fn loads_an_oci_layout_with_gzip_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(&tmp);
        let l = layer(&[("etc/os", b"oci")]);
        let (archive, manifest_digest) = oci_layout(&l, "docker.io/local/oci:v1");
        // Compressed as a whole, too.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&archive).unwrap();
        let report = store
            .load(&mut gz.finish().unwrap().as_slice(), u64::MAX, &none)
            .unwrap();
        assert_eq!(report.names, ["docker.io/local/oci:v1"]);
        let (_, record) = store.find("local/oci:v1").unwrap();
        assert_eq!(record.digest, manifest_digest);
        assert_eq!(record.config.cmd, ["/oci"]);
        // buildx reports the config's digest as the image ID (`-q`, `--iidfile`).
        let config_digest = digest(&config(&[digest(&l)], "/oci"));
        let (name, by_config) = store.find(&config_digest).unwrap();
        assert_eq!(by_config.digest, manifest_digest);
        assert_eq!(name, "docker.io/local/oci:v1");
        let (_, by_prefix) = store.find(&config_digest["sha256:".len()..][..12]).unwrap();
        assert_eq!(by_prefix.digest, manifest_digest);
    }

    #[test]
    fn loading_again_moves_the_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(&tmp);
        let tag = serde_json::json!(["x:1"]);
        let first = docker_archive(&layer(&[("f", b"1")]), "/one", tag.clone());
        let second = docker_archive(&layer(&[("f", b"2")]), "/two", tag);
        store.load(&mut first.as_slice(), u64::MAX, &none).unwrap();
        // The same archive again: one image, one copy of each file.
        store.load(&mut first.as_slice(), u64::MAX, &none).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        let (_, old) = store.find("x:1").unwrap();
        let objects_of_one = store.objects.total_size().unwrap().0;
        store.load(&mut second.as_slice(), u64::MAX, &none).unwrap();
        let (_, record) = store.find("x:1").unwrap();
        assert_eq!(record.config.cmd, ["/two"]);
        // The image that lost its only name goes, files and all (as with `tag`).
        assert!(
            !store.image_dir(&old.digest).exists(),
            "the old image stayed"
        );
        assert_eq!(store.objects.total_size().unwrap().0, objects_of_one);
        // Unless a container uses it.
        let used = docker_archive(&layer(&[("f", b"3")]), "/three", serde_json::json!(["x:1"]));
        let in_use = || HashMap::from([(record.digest.clone(), "c1".to_string())]);
        store.load(&mut used.as_slice(), u64::MAX, &in_use).unwrap();
        assert!(
            store.image_dir(&record.digest).exists(),
            "an image in use went"
        );
        // Without a name it is kept by its ID.
        let untagged = docker_archive(&layer(&[("u", b"3")]), "/none", serde_json::Value::Null);
        let report = store
            .load(&mut untagged.as_slice(), u64::MAX, &none)
            .unwrap();
        assert!(report.names.is_empty());
        let (_, by_id) = store.find(&report.untagged[0]).unwrap();
        assert_eq!(by_id.config.cmd, ["/none"]);
        // `image prune` (dangling) removes images without a name, and only those.
        let pruned = store.prune_dangling(&none).unwrap();
        assert!(
            pruned.removed_ids.contains(&report.untagged[0]),
            "{pruned:?}"
        );
        assert!(store.find(&report.untagged[0]).is_err());
        assert!(store.find("x:1").is_ok());
    }

    #[test]
    fn an_archive_larger_than_the_store_limit_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(&tmp);
        let big = docker_archive(
            &layer(&[("big", &[7u8; 64 * 1024])]),
            "/big",
            serde_json::json!(["big:1"]),
        );
        // Compressed it is small; unpacked it passes the limit.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&big).unwrap();
        let gz = gz.finish().unwrap();
        assert!(gz.len() < 16 * 1024);
        let err = store
            .load(&mut gz.as_slice(), 32 * 1024, &none)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidInput, "{err}");
        assert!(err.why.contains("limit"), "{err}");
        assert!(store.find("big:1").is_err());
        assert!(
            !store
                .scratch_dir(&format!("load.{}", std::process::id()))
                .exists()
        );
        // Within the limit it loads.
        store.load(&mut big.as_slice(), u64::MAX, &none).unwrap();
    }

    #[test]
    fn the_store_limit_removes_unnamed_images_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(&tmp);
        let unnamed = docker_archive(&layer(&[("a", b"aaaa")]), "/a", serde_json::Value::Null);
        let named = docker_archive(&layer(&[("b", b"bbbb")]), "/b", serde_json::json!(["b:1"]));
        let newest = docker_archive(&layer(&[("c", b"cccc")]), "/c", serde_json::json!(["c:1"]));
        let a = store
            .load(&mut unnamed.as_slice(), u64::MAX, &none)
            .unwrap()
            .untagged[0]
            .clone();
        store.load(&mut named.as_slice(), u64::MAX, &none).unwrap();
        store.load(&mut newest.as_slice(), u64::MAX, &none).unwrap();
        let (_, c) = store.find("c:1").unwrap();
        let total = store.objects.total_size().unwrap().1;
        let _lock = store.lock(true).unwrap();
        store
            .enforce_limit_locked(total - 1, &HashMap::new(), &c.digest)
            .unwrap();
        assert!(store.find(&a).is_err(), "the unnamed image stayed");
        assert!(store.find("b:1").is_ok() && store.find("c:1").is_ok());
    }

    #[test]
    fn a_hostile_archive_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(&tmp);
        let l = layer(&[("f", b"x")]);
        let cfg = config(&[digest(&l)], "/x");
        let manifest = |config: &str, layers: &[&str]| {
            serde_json::to_vec(&serde_json::json!([{
                "Config": config, "RepoTags": ["evil:1"], "Layers": layers}]))
            .unwrap()
        };
        let cfg_name = format!("{}.json", hex(&cfg));
        // Bytes that are not a tar, named by their own digest: refused for what they are.
        let junk = vec![b'j'; 2048];
        let junk_cfg = config(&[digest(&junk)], "/x");
        let wrong_cfg_name = format!("{}.json", hex(b"other"));
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("parent path", tar_of(&[("../escaped", b"x")])),
            ("absolute path", tar_of(&[("/etc/escaped", b"x")])),
            (
                "missing file",
                tar_of(&[
                    (&cfg_name, &cfg),
                    (
                        "manifest.json",
                        &manifest(&cfg_name, &["nothing/layer.tar"]),
                    ),
                ]),
            ),
            (
                "config digest",
                tar_of(&[
                    (&wrong_cfg_name, &cfg),
                    ("l.tar", &l),
                    ("manifest.json", &manifest(&wrong_cfg_name, &["l.tar"])),
                ]),
            ),
            (
                "not a tar",
                tar_of(&[
                    (&format!("{}.json", hex(&junk_cfg)), &junk_cfg),
                    ("l.tar", &junk),
                    (
                        "manifest.json",
                        &manifest(&format!("{}.json", hex(&junk_cfg)), &["l.tar"]),
                    ),
                ]),
            ),
            (
                "diff id",
                tar_of(&[
                    (&cfg_name, &cfg),
                    ("l.tar", &layer(&[("other", b"y")])),
                    ("manifest.json", &manifest(&cfg_name, &["l.tar"])),
                ]),
            ),
            (
                "link out",
                tar_of(&[("manifest.json@../../../../etc/passwd", b"")]),
            ),
            ("no images", tar_of(&[("readme", b"hello")])),
        ];
        for (what, archive) in cases {
            let err = store
                .load(&mut archive.as_slice(), u64::MAX, &none)
                .unwrap_err();
            assert!(
                matches!(err.kind, ErrorKind::InvalidInput | ErrorKind::Runtime),
                "{what}: {err}"
            );
            assert!(store.find("evil:1").is_err(), "{what}: a name was given");
        }
        assert!(store.list().unwrap().is_empty());
        assert!(!tmp.path().join("escaped").exists());
        assert!(!tmp.path().join("store/escaped").exists());
    }
}
