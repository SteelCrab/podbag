//! Image manifests, indexes and configs, in both OCI and Docker formats.

use crate::image::ImageConfig;
use crate::image::reference::is_digest;
use crate::{Error, ErrorKind, Result};
use serde::Deserialize;

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// The Accept header for manifest requests.
pub const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

/// A pointer to a blob or manifest.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    #[serde(default)]
    pub variant: Option<String>,
}

impl Platform {
    fn label(&self) -> String {
        match &self.variant {
            Some(v) => format!("{}/{}/{v}", self.os, self.architecture),
            None => format!("{}/{}", self.os, self.architecture),
        }
    }
}

/// What a manifest request returned.
#[derive(Debug)]
pub enum Fetched {
    /// A multi-platform index: one manifest per platform.
    Index(Vec<Descriptor>),
    /// A single-platform image manifest.
    Manifest {
        config: Descriptor,
        layers: Vec<Descriptor>,
    },
}

#[derive(Deserialize)]
struct RawManifest {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    manifests: Option<Vec<Descriptor>>,
    config: Option<Descriptor>,
    layers: Option<Vec<Descriptor>>,
}

/// Parse a manifest or index. The format comes from the Content-Type header, then the
/// body's `mediaType`, then the presence of a `manifests` list.
pub fn parse_manifest(content_type: Option<&str>, body: &[u8]) -> Result<Fetched> {
    let bad = |why: String| {
        Error::new(
            ErrorKind::Runtime,
            "cannot read the image manifest",
            why,
            "try again; if it keeps failing, the registry sent a broken manifest",
        )
    };
    let raw: RawManifest = serde_json::from_slice(body).map_err(|e| bad(e.to_string()))?;

    let known = |t: &&str| matches!(*t, OCI_INDEX | DOCKER_LIST | OCI_MANIFEST | DOCKER_MANIFEST);
    let header = content_type.map(|t| t.split(';').next().unwrap_or("").trim());
    let kind = header
        .filter(known)
        .or(raw.media_type.as_deref().filter(known));
    if kind.is_none() {
        if let Some(t) = raw
            .media_type
            .as_deref()
            .or(header)
            .filter(|t| t.contains("manifest.v1"))
        {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "cannot read the image manifest",
                format!("the image uses the old format {t}"),
                "use an image published in OCI or Docker schema 2 format",
            ));
        }
    }

    let is_index = match kind {
        Some(OCI_INDEX | DOCKER_LIST) => true,
        Some(_) => false,
        None => raw.manifests.is_some(),
    };
    let unsafe_digest = |d: &str| {
        Error::new(
            ErrorKind::InvalidInput,
            "cannot read the image manifest",
            format!("invalid digest {d:?}"),
            "the image is malformed or malicious; do not use it",
        )
    };
    let check = |descs: &[Descriptor]| match descs.iter().find(|d| !is_digest(&d.digest)) {
        Some(d) => Err(unsafe_digest(&d.digest)),
        None => Ok(()),
    };
    if let Some(list) = &raw.manifests {
        check(list)?;
    }
    if let Some(c) = &raw.config {
        check(std::slice::from_ref(c))?;
    }
    if let Some(layers) = &raw.layers {
        check(layers)?;
    }
    if is_index {
        return raw
            .manifests
            .map(Fetched::Index)
            .ok_or_else(|| bad("the index has no manifests list".into()));
    }
    match (raw.config, raw.layers) {
        (Some(config), Some(layers)) => Ok(Fetched::Manifest { config, layers }),
        _ => Err(bad("the manifest has no config or layers".into())),
    }
}

/// This machine's platform in OCI terms: architecture and variant.
pub fn host_platform() -> (&'static str, Option<&'static str>) {
    match std::env::consts::ARCH {
        "aarch64" => ("arm64", Some("v8")),
        "x86_64" => ("amd64", None),
        "arm" => ("arm", Some("v7")),
        other => (other, None),
    }
}

/// Pick the linux manifest for `arch`: the exact variant first, then one without a variant.
pub fn select_platform(
    name: &str,
    manifests: &[Descriptor],
    arch: &str,
    variant: Option<&str>,
) -> Result<Descriptor> {
    let linux = |d: &&Descriptor| {
        d.platform
            .as_ref()
            .is_some_and(|p| p.os == "linux" && p.architecture == arch)
    };
    let variant_of = |d: &Descriptor| d.platform.as_ref().and_then(|p| p.variant.clone());
    let exact = manifests
        .iter()
        .filter(linux)
        .find(|d| variant_of(d).as_deref() == variant);
    let plain = manifests
        .iter()
        .filter(linux)
        .find(|d| variant_of(d).is_none());
    if let Some(d) = exact.or(plain) {
        return Ok(d.clone());
    }
    let offered: Vec<String> = manifests
        .iter()
        .filter_map(|d| d.platform.as_ref())
        .filter(|p| p.os != "unknown")
        .map(Platform::label)
        .collect();
    Err(Error::new(
        ErrorKind::NotFound,
        format!("no linux/{arch} image for {name}"),
        format!(
            "the image provides: {}",
            if offered.is_empty() {
                "nothing".to_string()
            } else {
                offered.join(", ")
            }
        ),
        format!("use an image built for linux/{arch}"),
    ))
}

/// How a layer blob is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

pub fn layer_compression(media_type: &str) -> Result<Compression> {
    match media_type {
        "application/vnd.oci.image.layer.v1.tar" => Ok(Compression::None),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Ok(Compression::Gzip),
        "application/vnd.oci.image.layer.v1.tar+zstd" => Ok(Compression::Zstd),
        other => Err(Error::new(
            ErrorKind::Unsupported,
            "cannot read an image layer",
            format!("unsupported layer type {other}"),
            "use an image whose layers are plain, gzip or zstd tar archives",
        )),
    }
}

/// The parts of an image config file that openpod uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub architecture: String,
    pub os: String,
    pub diff_ids: Vec<String>,
    pub config: ImageConfig,
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    config: Option<RawRun>,
    rootfs: RawRootfs,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct RawRun {
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    env: Option<Vec<String>>,
    working_dir: Option<String>,
    user: Option<String>,
    volumes: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    healthcheck: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawRootfs {
    #[serde(default)]
    diff_ids: Vec<String>,
}

/// An image config file for `config` and the layers `diff_ids`, as [`parse_config`]
/// reads it back: what `docker save` writes for an image of the store.
#[cfg(unix)]
pub fn config_json(config: &ImageConfig, diff_ids: &[String], created: u64) -> Vec<u8> {
    let env: Vec<String> = config.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let volumes: serde_json::Map<String, serde_json::Value> = config
        .volumes
        .iter()
        .map(|v| (v.clone(), serde_json::json!({})))
        .collect();
    let mut run = serde_json::json!({
        "Env": env,
        "Cmd": config.cmd,
        "WorkingDir": config.workdir.clone().unwrap_or_default(),
        "User": config.user.clone().unwrap_or_default(),
    });
    if !config.entrypoint.is_empty() {
        run["Entrypoint"] = serde_json::json!(config.entrypoint);
    }
    if !volumes.is_empty() {
        run["Volumes"] = serde_json::Value::Object(volumes);
    }
    if let Some(h) = &config.healthcheck {
        run["Healthcheck"] = h.to_docker();
    }
    serde_json::to_vec(&serde_json::json!({
        "architecture": host_platform().0,
        "os": "linux",
        "created": crate::shim::logbook::rfc3339(created, 0),
        "config": run,
        "rootfs": {"type": "layers", "diff_ids": diff_ids},
    }))
    .unwrap_or_default()
}

pub fn parse_config(body: &[u8]) -> Result<ConfigFile> {
    let raw: RawConfig = serde_json::from_slice(body).map_err(|e| {
        Error::new(
            ErrorKind::Runtime,
            "cannot read the image config",
            e.to_string(),
            "try again; if it keeps failing, the registry sent a broken config",
        )
    })?;
    if let Some(bad) = raw.rootfs.diff_ids.iter().find(|d| !is_digest(d)) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot read the image config",
            format!("invalid diff_id {bad:?}"),
            "the image is malformed or malicious; do not use it",
        ));
    }
    let run = raw.config.unwrap_or_default();
    let env = run
        .env
        .unwrap_or_default()
        .into_iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv, String::new()),
        })
        .collect();
    let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());
    Ok(ConfigFile {
        architecture: raw.architecture,
        os: raw.os,
        diff_ids: raw.rootfs.diff_ids,
        config: ImageConfig {
            entrypoint: run.entrypoint.unwrap_or_default(),
            cmd: run.cmd.unwrap_or_default(),
            env,
            workdir: non_empty(run.working_dir),
            user: non_empty(run.user),
            volumes: run.volumes.unwrap_or_default().into_keys().collect(),
            // A check that cannot be read is left out, as the image would run without it.
            healthcheck: run.healthcheck.and_then(|h| {
                crate::health::HealthSettings::from_docker(&h)
                    .ok()
                    .flatten()
            }),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn desc(arch: &str, variant: Option<&str>, digest_char: char) -> String {
        let variant = variant
            .map(|v| format!(r#","variant":"{v}""#))
            .unwrap_or_default();
        format!(
            r#"{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:{}","size":1,"platform":{{"os":"linux","architecture":"{arch}"{variant}}}}}"#,
            digest_char.to_string().repeat(64)
        )
    }

    fn index(descs: &[String]) -> Vec<Descriptor> {
        let body = format!(r#"{{"schemaVersion":2,"manifests":[{}]}}"#, descs.join(","));
        match parse_manifest(None, body.as_bytes()).unwrap() {
            Fetched::Index(m) => m,
            Fetched::Manifest { .. } => panic!("expected an index"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_written_config_reads_back_the_same() {
        let config = ImageConfig {
            entrypoint: vec!["/entry".into()],
            cmd: vec!["run".into(), "it".into()],
            env: vec![
                ("PATH".into(), "/bin".into()),
                ("EMPTY".into(), String::new()),
            ],
            workdir: Some("/work".into()),
            user: Some("1000:1000".into()),
            volumes: vec!["/data".into()],
            healthcheck: Some(crate::health::HealthSettings {
                test: vec!["CMD".into(), "true".into()],
                interval_ms: Some(5000),
                timeout_ms: None,
                retries: Some(3),
                start_period_ms: Some(1000),
                start_interval_ms: None,
            }),
        };
        let ids = vec![format!("sha256:{}", "a".repeat(64))];
        let back = parse_config(&config_json(&config, &ids, 1_700_000_000)).unwrap();
        assert_eq!(back.config, config);
        assert_eq!(back.diff_ids, ids);
        assert_eq!(back.os, "linux");
        let bare = ImageConfig::default();
        assert_eq!(
            parse_config(&config_json(&bare, &[], 0)).unwrap().config,
            bare
        );
    }

    #[test]
    fn parses_both_manifest_formats() {
        for mt in [
            "application/vnd.oci.image.manifest.v1+json",
            "application/vnd.docker.distribution.manifest.v2+json",
        ] {
            let body = format!(
                r#"{{"schemaVersion":2,"mediaType":"{mt}","config":{{"mediaType":"x","digest":"sha256:{c}","size":2}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:{l}","size":3}}]}}"#,
                c = "c".repeat(64),
                l = "d".repeat(64)
            );
            match parse_manifest(Some(mt), body.as_bytes()).unwrap() {
                Fetched::Manifest { config, layers } => {
                    assert_eq!(config.size, 2);
                    assert_eq!(layers.len(), 1);
                    assert_eq!(layers[0].digest, format!("sha256:{}", "d".repeat(64)));
                }
                Fetched::Index(_) => panic!("expected a manifest for {mt}"),
            }
        }
    }

    #[test]
    fn detects_an_index_without_content_type() {
        assert_eq!(index(&[desc("amd64", None, 'a')]).len(), 1);
    }

    #[test]
    fn selects_arm64_v8_and_skips_attestations() {
        let m = index(&[
            desc("amd64", None, 'a'),
            r#"{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":1,"platform":{"os":"unknown","architecture":"unknown"}}"#.to_string(),
            desc("arm64", Some("v7"), 'c'),
            desc("arm64", Some("v8"), 'e'),
        ]);
        let d = select_platform("x", &m, "arm64", Some("v8")).unwrap();
        assert_eq!(d.digest, format!("sha256:{}", "e".repeat(64)));
    }

    #[test]
    fn falls_back_to_a_platform_without_variant() {
        let m = index(&[desc("arm64", Some("v7"), 'c'), desc("arm64", None, 'f')]);
        let d = select_platform("x", &m, "arm64", Some("v8")).unwrap();
        assert_eq!(d.digest, format!("sha256:{}", "f".repeat(64)));
    }

    #[test]
    fn missing_platform_lists_what_exists() {
        let m = index(&[desc("amd64", None, 'a'), desc("s390x", None, 'b')]);
        let e = select_platform("docker.io/library/x:latest", &m, "arm64", Some("v8")).unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(
            e.what,
            "no linux/arm64 image for docker.io/library/x:latest"
        );
        assert!(
            e.why.contains("linux/amd64") && e.why.contains("linux/s390x"),
            "{}",
            e.why
        );
    }

    #[test]
    fn layer_media_types() {
        use Compression::*;
        for (mt, want) in [
            ("application/vnd.oci.image.layer.v1.tar", None),
            ("application/vnd.oci.image.layer.v1.tar+gzip", Gzip),
            ("application/vnd.oci.image.layer.v1.tar+zstd", Zstd),
            ("application/vnd.docker.image.rootfs.diff.tar.gzip", Gzip),
        ] {
            assert_eq!(layer_compression(mt).unwrap(), want, "{mt}");
        }
        let e = layer_compression("application/vnd.docker.image.rootfs.foreign.diff.tar.gzip")
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unsupported);
    }

    #[test]
    fn config_extracts_runtime_settings() {
        let body = br#"{"architecture":"arm64","os":"linux",
            "config":{"Entrypoint":null,"Cmd":["sh"],"Env":["PATH=/bin","A=b=c"],"WorkingDir":"","User":"app"},
            "rootfs":{"type":"layers","diff_ids":["sha256:1111111111111111111111111111111111111111111111111111111111111111"]}}"#;
        let c = parse_config(body).unwrap();
        assert_eq!(c.architecture, "arm64");
        assert_eq!(c.diff_ids.len(), 1);
        assert!(c.config.entrypoint.is_empty());
        assert_eq!(c.config.cmd, ["sh"]);
        assert_eq!(
            c.config.env,
            [
                ("PATH".to_string(), "/bin".to_string()),
                ("A".to_string(), "b=c".to_string())
            ]
        );
        assert_eq!(c.config.workdir, None);
        assert_eq!(c.config.user.as_deref(), Some("app"));
        assert!(c.config.volumes.is_empty());
    }

    #[test]
    fn image_volumes_are_read() {
        let body = br#"{"architecture":"arm64","os":"linux",
            "config":{"Cmd":["sh"],"Volumes":{"/logs":{},"/data":{}}},
            "rootfs":{"type":"layers","diff_ids":["sha256:1111111111111111111111111111111111111111111111111111111111111111"]}}"#;
        assert_eq!(
            parse_config(body).unwrap().config.volumes,
            ["/data", "/logs"]
        );
    }

    #[test]
    fn rejects_unsafe_digests() {
        let bad_config = br#"{"architecture":"arm64","os":"linux","rootfs":{"type":"layers","diff_ids":["sha256:../images/x/rootfs/evil"]}}"#;
        assert_eq!(
            parse_config(bad_config).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        let good = "a".repeat(64);
        for (config, layer) in [
            ("sha256:../x".to_string(), format!("sha256:{good}")),
            (format!("sha256:{good}"), "sha256:..".to_string()),
        ] {
            let body = format!(
                r#"{{"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"x","digest":"{config}","size":1}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer}","size":1}}]}}"#
            );
            assert_eq!(
                parse_manifest(None, body.as_bytes()).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{body}"
            );
        }
        let index = r#"{"manifests":[{"mediaType":"x","digest":"sha256:../../y","size":1,"platform":{"os":"linux","architecture":"arm64"}}]}"#;
        assert_eq!(
            parse_manifest(None, index.as_bytes()).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn config_without_a_config_section_is_empty() {
        let c = parse_config(
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#,
        )
        .unwrap();
        assert_eq!(c.config, crate::image::ImageConfig::default());
    }
}
