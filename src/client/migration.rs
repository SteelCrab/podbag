//! Transfer resources between two independent engine connections.
use super::{ApiError, Client, Kind, encode, wire};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};
use std::time::{SystemTime, UNIX_EPOCH};

const HELPER_IMAGE: &str = "docker.io/library/busybox:1.36.1";
const OWNER: &str = "io.github.steelcrab.podbag.migration";

pub fn transfer_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:x}-{nanos:x}-{:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Compare the contents and portable file metadata independently of tar record order.
pub fn file_manifest(reader: impl Read) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    let mut files = BTreeMap::new();
    let mut archive = tar::Archive::new(reader);
    let err = |e: std::io::Error| e.to_string();
    let mut buffer = [0u8; 32768];
    for entry in archive.entries().map_err(err)? {
        let mut entry = entry.map_err(err)?;
        let raw = entry.path_bytes();
        let path = raw.strip_prefix(b"./").unwrap_or(&raw);
        let path = path.strip_suffix(b"/").unwrap_or(path);
        if path.is_empty() || path == b"." {
            continue;
        }
        if path.starts_with(b"/") || path.split(|b| *b == b'/').any(|part| part == b"..") {
            return Err("Source returned a path outside the volume".into());
        }
        let path = path.to_vec();
        let header = entry.header();
        let mut hash = Sha256::new();
        hash.update([header.entry_type().as_byte()]);
        hash.update((header.mode().map_err(err)? & 0o7777).to_be_bytes());
        hash.update(header.uid().map_err(err)?.to_be_bytes());
        hash.update(header.gid().map_err(err)?.to_be_bytes());
        if let Some(link) = entry.link_name_bytes() {
            hash.update(&*link);
        }
        loop {
            let count = entry.read(&mut buffer).map_err(err)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        if files.insert(path, hash.finalize().to_vec()).is_some() {
            return Err("Duplicate file path in volume archive".into());
        }
    }
    Ok(files)
}

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Engine omitted {key}"))
}

fn missing(client: &Client, kind: Kind, name: &str) -> Result<bool, String> {
    match client.inspect(kind, name) {
        Ok(_) => Ok(false),
        Err(ApiError { status: 404, .. }) => Ok(true),
        Err(e) => Err(e.message),
    }
}
fn require_new(client: &Client, kind: Kind, name: &str) -> Result<(), String> {
    if missing(client, kind, name)? {
        Ok(())
    } else {
        Err(format!("Destination already contains {name}"))
    }
}

// Engines may report a registry manifest ID, a config ID or compressed layer digests.
// The exported config is the portable description of the bytes sent to the destination.
fn archive_config(file: &mut std::fs::File, name: &str) -> Result<(Value, String), String> {
    let read_entry = |file: &mut std::fs::File, path: &str| -> Result<Vec<u8>, String> {
        file.rewind().map_err(|e| e.to_string())?;
        let mut archive = tar::Archive::new(file);
        for entry in archive.entries_with_seek().map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.path_bytes().as_ref() != path.as_bytes() {
                continue;
            }
            if entry.size() > 16 << 20 {
                return Err("Image metadata is too large".into());
            }
            let mut data = Vec::new();
            entry
                .take(16 << 20)
                .read_to_end(&mut data)
                .map_err(|e| e.to_string())?;
            return Ok(data);
        }
        Err(format!("Image archive is missing {path}"))
    };
    let manifest: Vec<Value> =
        serde_json::from_slice(&read_entry(file, "manifest.json")?).map_err(|e| e.to_string())?;
    let [image] = manifest.as_slice() else {
        return Err("Select a single-platform image for migration".into());
    };
    let expected = crate::image::reference::Reference::parse(name).ok();
    for tag in image["RepoTags"].as_array().into_iter().flatten() {
        let tag = tag.as_str().ok_or("Invalid archive tag")?;
        if tag != name && crate::image::reference::Reference::parse(tag).ok() != expected {
            return Err(format!(
                "Select a single tag instead of an archive that would also load {tag}"
            ));
        }
    }
    let config = read_entry(file, field(image, "Config")?)?;
    let digest = format!("sha256:{}", crate::image::registry::sha256_hex(&config));
    Ok((
        serde_json::from_slice(&config).map_err(|e| e.to_string())?,
        digest,
    ))
}
fn archive_file(client: &Client, name: &str) -> Result<std::fs::File, String> {
    let mut f = tempfile::tempfile().map_err(|e| e.to_string())?;
    let mut archive = client.export_images(&[name]).map_err(|e| e.message)?;
    std::io::copy(&mut archive, &mut f).map_err(|e| e.to_string())?;
    f.rewind().map_err(|e| e.to_string())?;
    Ok(f)
}

fn matches_config(config: &Value, loaded: &Value) -> bool {
    fn nonempty(v: &Value) -> Option<&Value> {
        match v {
            Value::Null => None,
            Value::String(s) if s.is_empty() => None,
            Value::Object(o) if o.is_empty() => None,
            Value::Array(a) if a.is_empty() => None,
            _ => Some(v),
        }
    }
    config["rootfs"]["diff_ids"].is_array()
        && config["rootfs"]["diff_ids"] == loaded["RootFS"]["Layers"]
        && config["os"] == loaded["Os"]
        && config["architecture"] == loaded["Architecture"]
        && [
            "Cmd",
            "Env",
            "Entrypoint",
            "WorkingDir",
            "User",
            "Labels",
            "Volumes",
            "ExposedPorts",
            "Healthcheck",
            "StopSignal",
            "Shell",
            "OnBuild",
        ]
        .iter()
        .all(|k| nonempty(&config["config"][k]) == nonempty(&loaded["Config"][k]))
}

fn copy_image(from: &Client, to: &Client, name: &str) -> Result<(), String> {
    let source = from.inspect(Kind::Images, name).map_err(|e| e.message)?;
    let mut archive = archive_file(from, name)?;
    let (config, digest) = archive_config(&mut archive, name)?;
    let matches = |loaded: &Value| {
        loaded["Id"] == digest
            || loaded["Id"] == source["Id"] && loaded["Id"].is_string()
            || matches_config(&config, loaded)
    };
    if !missing(to, Kind::Images, name)? {
        return if matches(&to.inspect(Kind::Images, name).map_err(|e| e.message)?) {
            Ok(())
        } else {
            Err(format!("Destination tag {name} belongs to another image"))
        };
    }
    archive.rewind().map_err(|e| e.to_string())?;
    to.load_images(archive).map_err(|e| e.message)?;
    let loaded = to.inspect(Kind::Images, name).map_err(|e| e.message)?;
    if !matches(&loaded) {
        let (_, target_digest) = archive_config(&mut archive_file(to, name)?, name)?;
        if target_digest != digest {
            return Err(format!("Image {name} failed destination verification"));
        }
    }
    Ok(())
}

struct Owned<'a> {
    client: &'a Client,
    objects: Vec<(Kind, String, bool)>,
}
impl<'a> Owned<'a> {
    fn new(client: &'a Client) -> Self {
        Self {
            client,
            objects: Vec::new(),
        }
    }
    fn add(&mut self, kind: Kind, name: &str) {
        self.objects.push((kind, name.to_owned(), false));
    }
    fn clean(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        for (kind, id, force) in self.objects.drain(..).rev() {
            if let Err(e) = self.client.remove(kind, &id, force)
                && e.status != 404
            {
                errors.push(format!("{id}: {e}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("Cleanup required: {}", errors.join(", ")))
        }
    }
}
fn with_cleanup<T>(result: Result<T, String>, cleanup: Result<(), String>) -> Result<T, String> {
    match (result, cleanup) {
        (Err(e), Err(c)) => Err(format!("{e}. {c}")),
        (r, Ok(())) => r,
        (Ok(_), Err(c)) => Err(c),
    }
}

fn helper(
    client: &Client,
    volume: &str,
    readonly: bool,
    owned: &mut Owned<'_>,
    kiri: bool,
) -> Result<String, String> {
    if missing(client, Kind::Images, HELPER_IMAGE)? {
        client
            .pull(HELPER_IMAGE, |_| {})
            .map_err(|e| format!("Volume helper image: {e}"))?;
    }
    let cfg = json!({"Image":HELPER_IMAGE,"Cmd":["sleep","86400"],"HostConfig":{"NetworkMode":"none","Mounts":[{
        "Type":"volume","Source":volume,"Target":"/data","ReadOnly":readonly,"VolumeOptions":{"NoCopy":true}
    }]}});
    let created = client
        .call("POST", "/containers/create", Some(&cfg))
        .map_err(|e| e.message)?;
    let id = field(&created, "Id")?.to_owned();
    owned.objects.push((Kind::Containers, id.clone(), true));
    // Kiri mounts volumes for the archive API only while the helper runs.
    if kiri {
        client
            .container_action(&id, "start")
            .map_err(|e| e.message)?;
    }
    Ok(id)
}
fn archive(client: &Client, id: &str, wrapper: bool) -> Result<impl Read, String> {
    // Docker, Podman and Podbag use /data/. for contents. Kiri always includes the directory.
    let path = if wrapper { "/data" } else { "/data/." };
    let (response, _) = client
        .request(
            "GET",
            &format!("/containers/{}/archive?path={}", encode(id), encode(path)),
            None,
            None,
        )
        .map_err(|e| e.message)?;
    Ok(response)
}
fn volume_manifest(reader: impl Read, wrapper: bool) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    let entries = file_manifest(reader)?;
    let mut result = BTreeMap::new();
    for (path, hash) in entries {
        let path = if wrapper {
            if path == b"data" {
                continue;
            }
            path.strip_prefix(b"data/")
                .ok_or("Unexpected volume archive root")?
                .to_vec()
        } else {
            path
        };
        result.insert(path, hash);
    }
    Ok(result)
}
fn copy_volume(
    from: &Client,
    to: &Client,
    name: &str,
    owner: &str,
    dest: &mut Owned<'_>,
) -> Result<(), String> {
    let spec = from.inspect(Kind::Volumes, name).map_err(|e| e.message)?;
    if spec["Driver"] != "local" || spec["Options"].as_object().is_some_and(|v| !v.is_empty()) {
        return Err(format!(
            "{name} uses a host-specific volume driver or options"
        ));
    }
    require_new(to, Kind::Volumes, name)?;
    for c in from
        .containers()
        .map_err(|e| e.message)?
        .into_iter()
        .filter(|c| {
            !matches!(
                c.state.as_str(),
                "created" | "exited" | "dead" | "configured" | "stopped"
            )
        })
    {
        let data = from
            .inspect(Kind::Containers, &c.id)
            .map_err(|e| e.message)?;
        if data["Mounts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|m| m["Name"] == name && m["RW"] != false)
        {
            return Err(format!(
                "Stop {} before copying its writable volume {name}",
                c.name()
            ));
        }
    }
    let mut source_helpers = Owned::new(from);
    let mut target_helpers = Owned::new(to);
    let result = (|| {
        let src_kiri = from.version().map_err(|e| e.message)?["Platform"]["Name"] == "Kiri";
        let dst_kiri = to.version().map_err(|e| e.message)?["Platform"]["Name"] == "Kiri";
        let src = helper(from, name, true, &mut source_helpers, src_kiri)?;
        let mut tar = tempfile::tempfile().map_err(|e| e.to_string())?;
        let bytes = std::io::copy(&mut archive(from, &src, src_kiri)?, &mut tar)
            .map_err(|e| e.to_string())?;
        if bytes < 1024 {
            return Err("Source returned an incomplete volume archive".into());
        }
        tar.rewind().map_err(|e| e.to_string())?;
        let expected = volume_manifest(&mut tar, src_kiri)?;
        let mut labels = spec["Labels"].as_object().cloned().unwrap_or_default();
        labels.insert(OWNER.into(), json!(owner));
        let made = to
            .call(
                "POST",
                "/volumes/create",
                Some(&json!({"Name":name,"Labels":labels})),
            )
            .map_err(|e| e.message)?;
        if made["Labels"][OWNER] != owner {
            return Err(format!(
                "Destination volume {name} was not reserved by this migration"
            ));
        }
        dest.add(Kind::Volumes, name);
        let dst = helper(to, name, false, &mut target_helpers, dst_kiri)?;
        tar.rewind().map_err(|e| e.to_string())?;
        let directory = if src_kiri { "/" } else { "/data" };
        let path = format!(
            "/containers/{}/archive?path={}&copyUIDGID=true",
            encode(&dst),
            encode(directory)
        );
        let (response, _) = to
            .send(
                "PUT",
                &path,
                &[],
                wire::Body::Stream("application/x-tar", &mut tar),
                None,
            )
            .map_err(|e| e.message)?;
        response.bytes(1 << 20).map_err(|e| e.to_string())?;
        if volume_manifest(archive(to, &dst, dst_kiri)?, dst_kiri)? != expected {
            return Err(format!("File verification failed for volume {name}"));
        }
        Ok(())
    })();
    with_cleanup(
        with_cleanup(result, target_helpers.clean()),
        source_helpers.clean(),
    )
}

fn verify_container(source: &Value, target: &Value) -> Result<(), String> {
    let original = &source["Config"];
    let copied = &target["Config"];
    for key in ["Cmd", "Entrypoint"] {
        if original[key]
            .as_array()
            .into_iter()
            .flatten()
            .ne(copied[key].as_array().into_iter().flatten())
        {
            return Err(format!("Container {key} verification failed"));
        }
    }
    let workdir = |config: &Value| {
        config["WorkingDir"]
            .as_str()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_owned()
    };
    if field(source, "Name")?.trim_start_matches('/')
        != field(target, "Name")?.trim_start_matches('/')
        || workdir(original) != workdir(copied)
        || original["User"].as_str().unwrap_or("") != copied["User"].as_str().unwrap_or("")
        || target["State"]["Running"] == true
    {
        return Err("Container identity or runtime configuration verification failed".into());
    }
    for entry in original["Env"].as_array().into_iter().flatten() {
        if !copied["Env"]
            .as_array()
            .is_some_and(|env| env.contains(entry))
        {
            return Err("Container environment verification failed".into());
        }
    }
    for mount in source["Mounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["Type"] == "volume")
    {
        if !target["Mounts"].as_array().into_iter().flatten().any(|m| {
            ["Type", "Name", "Destination", "RW"]
                .iter()
                .all(|key| m[key] == mount[key])
        }) {
            return Err("Container volume mount verification failed".into());
        }
    }
    Ok(())
}

fn migrate_container(
    from: &Client,
    to: &Client,
    id: &str,
    destination_defaults: bool,
    owner: &str,
    created: &mut Owned<'_>,
    progress: &impl Fn(&str),
) -> Result<(), String> {
    let spec = from.inspect(Kind::Containers, id).map_err(|e| e.message)?;
    let name = field(&spec, "Name")?.trim_start_matches('/');
    require_new(to, Kind::Containers, name)?;
    if spec["State"]["Running"] == true || spec["State"]["Paused"] == true {
        return Err(format!(
            "Stop {name} before migrating its filesystem and volumes"
        ));
    }
    let mut config = spec["Config"].clone();
    let mut host = spec["HostConfig"].clone();
    if !config.is_object() || !host.is_object() {
        return Err("Missing container configuration".into());
    }
    if host["VolumesFrom"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
    {
        return Err("VolumesFrom depends on another container".into());
    }
    let mut mounts = Vec::new();
    let mut volumes = BTreeSet::new();
    for m in spec["Mounts"].as_array().into_iter().flatten() {
        match m["Type"].as_str() {
            Some("volume") => {
                let source = field(m, "Name")?;
                volumes.insert(source);
                mounts.push(
                    json!({"Type":"volume","Source":source,"Target":field(m,"Destination")?,
                    "ReadOnly":m["RW"]==false,"VolumeOptions":{"NoCopy":true}}),
                );
            }
            Some("tmpfs") if host["Tmpfs"].get(field(m, "Destination")?).is_some() => {}
            _ => {
                return Err(format!(
                    "Container {name} uses a host-specific mount at {}",
                    m["Destination"]
                ));
            }
        }
    }
    for v in &volumes {
        require_new(to, Kind::Volumes, v)?;
    }
    if destination_defaults {
        let host = host.as_object_mut().unwrap();
        host.remove("LogConfig");
        host.remove("Ulimits");
    }
    let podbag = to.version().map_err(|e| e.message)?["Platform"]["Name"] == "podbag";
    if podbag
        && matches!(
            config["StopSignal"].as_str(),
            Some("15" | "SIGTERM" | "TERM")
        )
    {
        config.as_object_mut().unwrap().remove("StopSignal");
    }
    if podbag && host["ShmSize"].as_u64().is_some_and(|size| size > 0) {
        let size = host["ShmSize"].as_u64().unwrap();
        if host["Tmpfs"]["/dev/shm"].is_null() {
            host["Tmpfs"]["/dev/shm"] = json!(format!("size={size},mode=1777,nosuid,nodev,noexec"));
        }
        host.as_object_mut().unwrap().remove("ShmSize");
    }
    // Cgroup v2 has no per-container swappiness control. Podman serializes the absent value as zero.
    if host["MemorySwappiness"] == 0
        && from.call("GET", "/info", None).map_err(|e| e.message)?["CgroupVersion"] == "2"
    {
        host.as_object_mut().unwrap().remove("MemorySwappiness");
    }
    // Docker represents isolated PID and UTS namespaces with the empty mode.
    for key in ["PidMode", "UTSMode"] {
        if host[key] == "private" {
            host.as_object_mut().unwrap().remove(key);
        }
    }
    if host["Runtime"] == "oci" {
        host.as_object_mut().unwrap().remove("Runtime");
    }
    // Podman's inspect API uses libc names. Docker's create API takes lowercase names.
    for limit in host["Ulimits"].as_array_mut().into_iter().flatten() {
        if let Some(name) = limit["Name"]
            .as_str()
            .and_then(|s| s.strip_prefix("RLIMIT_"))
        {
            limit["Name"] = json!(name.to_ascii_lowercase());
        }
    }
    host["Binds"] = Value::Null;
    host["Mounts"] = json!(mounts);
    let mut endpoints = serde_json::Map::new();
    if let Some(networks) = spec["NetworkSettings"]["Networks"].as_object() {
        for (network, v) in networks {
            if !matches!(
                network.as_str(),
                "bridge" | "default" | "podman" | "none" | "host"
            ) {
                to.inspect(Kind::Networks, network)
                    .map_err(|_| format!("Create network {network} on the destination first"))?;
                endpoints.insert(
                    network.clone(),
                    json!({"Aliases":v["Aliases"],"IPAMConfig":v["IPAMConfig"]}),
                );
            }
        }
    }
    if host["NetworkMode"] == "podman" || host["NetworkMode"] == "bridge" {
        host["NetworkMode"] = json!("default");
    }
    if !endpoints.is_empty() {
        config["NetworkingConfig"] = json!({"EndpointsConfig":endpoints});
    }
    config["HostConfig"] = host;
    let repo = "localhost/podbag-migration";
    let image = format!("{repo}:{owner}");
    require_new(to, Kind::Images, &image)?;
    progress("snapshot");
    let path = format!(
        "/commit?container={}&repo={repo}&tag={owner}",
        encode(field(&spec, "Id")?)
    );
    let mut source = Owned::new(from);
    require_new(from, Kind::Images, &image)?;
    source.add(Kind::Images, &image);
    let result = (|| {
        let (res, _) = from
            .request("POST", &path, None, None)
            .map_err(|e| e.message)?;
        res.bytes(1 << 20).map_err(|e| e.to_string())?;
        copy_image(from, to, &image)?;
        created.add(Kind::Images, &image);
        for volume in volumes {
            progress("files");
            copy_volume(from, to, volume, owner, created)?;
        }
        config["Image"] = json!(image);
        let result = to
            .call(
                "POST",
                &format!("/containers/create?name={}", encode(name)),
                Some(&config),
            )
            .map_err(|e| e.message)?;
        let target = field(&result, "Id")?;
        created.add(Kind::Containers, target);
        if let Some(warnings) = result["Warnings"].as_array()
            && !warnings.is_empty()
        {
            return Err(format!(
                "Destination could not reproduce the configuration: {warnings:?}"
            ));
        }
        progress("verify");
        let checked = to
            .inspect(Kind::Containers, target)
            .map_err(|e| e.message)?;
        verify_container(&spec, &checked)
    })();
    with_cleanup(result, source.clean())
}

pub fn migrate(
    from: &Client,
    to: &Client,
    kind: Kind,
    id: &str,
    destination_defaults: bool,
    progress: impl Fn(&str),
) -> Result<(), String> {
    progress("prepare");
    if from.endpoint.describe() == to.endpoint.describe() {
        return Err("Choose two different engines".into());
    }
    from.ping().map_err(|e| format!("Source: {e}"))?;
    to.ping().map_err(|e| format!("Destination: {e}"))?;
    let owner = transfer_id();
    let mut dest = Owned::new(to);
    let result = match kind {
        Kind::Images => {
            progress("image");
            copy_image(from, to, id)
        }
        Kind::Volumes => {
            progress("files");
            copy_volume(from, to, id, &owner, &mut dest)
        }
        Kind::Containers => migrate_container(
            from,
            to,
            id,
            destination_defaults,
            &owner,
            &mut dest,
            &progress,
        ),
        Kind::Networks => Err("Select a container, image or volume".into()),
    };
    if result.is_err() {
        return with_cleanup(result, dest.clean());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_the_created_container_after_engine_normalization() {
        let source = json!({"Name":"/demo","Config":{"Cmd":["true"],"Env":["PORT=80"]},
            "Mounts":[{"Type":"volume","Name":"files","Destination":"/files","RW":false}]});
        let mut target = source.clone();
        target["Config"]["Env"] = json!(["PORT=80", "HOME=/root"]);
        target["Config"]["WorkingDir"] = json!("/");
        target["Config"]["Entrypoint"] = json!([]);
        verify_container(&source, &target).unwrap();
        target["Mounts"][0]["RW"] = json!(true);
        assert!(verify_container(&source, &target).is_err());
        target["Mounts"] = source["Mounts"].clone();
        target["Config"]["Env"] = json!(["PORT=81"]);
        assert!(verify_container(&source, &target).is_err());
    }
}
