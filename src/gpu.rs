use crate::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The Docker DeviceRequest fields used to select Linux DRM render devices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Request {
    pub driver: String,
    pub count: i32,
    #[serde(rename = "DeviceIDs", deserialize_with = "default_if_null")]
    pub device_ids: Vec<String>,
    #[serde(deserialize_with = "default_if_null")]
    pub capabilities: Vec<Vec<String>>,
    #[serde(deserialize_with = "default_if_null")]
    pub options: BTreeMap<String, String>,
}

fn default_if_null<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.driver.as_str(), "" | "dri")
            || !self.options.is_empty()
            || !(self.capabilities.is_empty()
                || self.capabilities.len() == 1 && self.capabilities[0] == ["gpu"])
        {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "cannot request GPUs",
                "Podbag supports only Linux DRM render devices and the gpu capability",
                "omit the driver or select dri, without driver-specific options",
            ));
        }
        if self.device_ids.is_empty() && (self.count == 0 || self.count < -1)
            || !self.device_ids.is_empty()
                && (self.count != 0 || self.device_ids.iter().any(|id| id.is_empty()))
        {
            return Err(invalid("specify all, a positive count, or a device list"));
        }
        Ok(())
    }
}

impl std::str::FromStr for Request {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let text = text
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(text);
        let mut request = Self {
            capabilities: vec![vec!["gpu".into()]],
            ..Self::default()
        };
        if let Some(ids) = text.strip_prefix("device=") {
            request.device_ids = ids.split(',').map(str::trim).map(str::to_owned).collect();
        } else {
            request.count = match text {
                "all" => -1,
                value => value
                    .strip_prefix("count=")
                    .unwrap_or(value)
                    .parse()
                    .map_err(|_| invalid("invalid GPU count or device list"))?,
            };
        }
        request.validate()?;
        Ok(request)
    }
}

fn invalid(why: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "cannot request GPUs",
        why,
        "use --gpus all, --gpus COUNT, or --gpus device=renderD128",
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn devices(requests: &[Request]) -> Result<Vec<crate::spec::Device>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    if requests.is_empty() {
        return Ok(Vec::new());
    }
    for request in requests {
        request.validate()?;
    }
    let directory = "/dev/dri";
    let unavailable = || {
        Error::new(
            ErrorKind::NotFound,
            "cannot map GPUs",
            "the Linux host has no DRM render devices",
            "enable a host graphics driver that exposes /dev/dri/renderD*",
        )
    };
    let entries = std::fs::read_dir(directory).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            unavailable()
        } else {
            Error::io(
                "cannot list GPU devices",
                &error,
                "check access to /dev/dri",
            )
        }
    })?;
    let mut nodes = BTreeMap::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| Error::io("cannot list GPU devices", &error, "check /dev/dri"))?;
        let name = entry.file_name();
        let Some(number) = name
            .to_str()
            .and_then(|name| name.strip_prefix("renderD"))
            .and_then(|number| number.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let info = std::fs::metadata(&path)
            .map_err(|error| Error::io("cannot inspect a GPU device", &error, "check /dev/dri"))?;
        if info.file_type().is_char_device() && libc::major(info.rdev()) == 226 {
            nodes.insert(number, path.to_string_lossy().into_owned());
        }
    }
    if nodes.is_empty() {
        return Err(unavailable());
    }
    select(requests, &nodes.into_values().collect::<Vec<_>>())?
        .into_iter()
        .map(|path| crate::spec::Device::new(path, path, "rwm"))
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn select<'a>(requests: &[Request], nodes: &'a [String]) -> Result<Vec<&'a str>> {
    let mut selected = Vec::new();
    for request in requests {
        request.validate()?;
        if request.device_ids.is_empty() {
            let count = if request.count == -1 {
                nodes.len()
            } else {
                request.count as usize
            };
            if count > nodes.len() {
                return Err(invalid("the requested count exceeds the available GPUs"));
            }
            selected.extend(nodes[..count].iter().map(String::as_str));
            continue;
        }
        for id in &request.device_ids {
            let node = id
                .parse::<usize>()
                .ok()
                .and_then(|index| nodes.get(index))
                .or_else(|| {
                    nodes
                        .iter()
                        .find(|path| *path == id || path.strip_prefix("/dev/dri/") == Some(id))
                })
                .ok_or_else(|| invalid("a requested GPU is not present in /dev/dri"))?;
            selected.push(node.as_str());
        }
    }
    let mut seen = std::collections::HashSet::new();
    selected.retain(|path| seen.insert(*path));
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_drm_render_nodes_by_count_index_or_name() {
        let nodes = [
            "/dev/dri/renderD128".to_string(),
            "/dev/dri/renderD131".to_string(),
        ];
        for (selection, expected) in [
            ("all", vec![nodes[0].as_str(), nodes[1].as_str()]),
            ("count=1", vec![nodes[0].as_str()]),
            ("device=1,renderD131", vec![nodes[1].as_str()]),
            ("device=/dev/dri/renderD128", vec![nodes[0].as_str()]),
        ] {
            assert_eq!(
                select(&[selection.parse().unwrap()], &nodes).unwrap(),
                expected
            );
        }
        assert!(select(&["3".parse().unwrap()], &nodes).is_err());
        assert!(select(&["device=2".parse().unwrap()], &nodes).is_err());
        assert!("device=".parse::<Request>().is_err());
    }

    #[test]
    fn unsupported_gpu_drivers_report_an_error() {
        let request = Request {
            driver: "nvidia".into(),
            count: -1,
            ..Request::default()
        };
        assert_eq!(request.validate().unwrap_err().kind, ErrorKind::Unsupported);
    }

    #[test]
    fn accepts_docker_requests_with_null_optional_fields() {
        let request: Request = serde_json::from_str(
            r#"{"Count":-1,"DeviceIDs":null,"Options":null,"Capabilities":[["gpu"]]}"#,
        )
        .unwrap();
        request.validate().unwrap();
        assert_eq!(request.count, -1);
        assert!(request.device_ids.is_empty());
        assert!(request.options.is_empty());
    }
}
