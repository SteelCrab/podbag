//! The CRI services behind `podbag cri serve`: runtime information and images.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message;

use super::grpc::{Code, Service, Status};
use super::images;
use super::pb;
use crate::image::{self, Image};
use crate::{Error, ErrorKind};

/// How the kubelet places cgroups; podbag follows it for pods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupDriver {
    Cgroupfs,
    Systemd,
}

/// How `podbag cri serve` was started.
#[derive(Debug, Clone)]
pub struct Options {
    pub socket: PathBuf,
    pub cni_conf_dir: PathBuf,
    pub cni_bin_dirs: Vec<PathBuf>,
    pub cgroup_driver: CgroupDriver,
}

/// The CRI runtime and image services.
pub struct Cri {
    options: Options,
}

impl Cri {
    pub fn new(options: Options) -> Cri {
        Cri { options }
    }

    fn version(&self) -> pb::VersionResponse {
        pb::VersionResponse {
            version: "0.1.0".into(),
            runtime_name: "podbag".into(),
            runtime_version: env!("CARGO_PKG_VERSION").into(),
            runtime_api_version: "v1".into(),
        }
    }

    fn status(&self, verbose: bool) -> pb::StatusResponse {
        let network = self.network_config();
        let condition = |kind: &str, result: &Result<PathBuf, String>| pb::RuntimeCondition {
            r#type: kind.into(),
            status: result.is_ok(),
            reason: if result.is_ok() {
                String::new()
            } else {
                "NetworkPluginNotReady".into()
            },
            message: result.as_ref().err().cloned().unwrap_or_default(),
        };
        let mut info = HashMap::new();
        if verbose {
            info.insert(
                "config".to_string(),
                serde_json::json!({
                    "socket": self.options.socket,
                    "cniConfDir": self.options.cni_conf_dir,
                    "cniBinDirs": self.options.cni_bin_dirs,
                    "cgroupDriver": format!("{:?}", self.options.cgroup_driver).to_lowercase(),
                    "cniConfig": network.as_ref().ok(),
                })
                .to_string(),
            );
        }
        pb::StatusResponse {
            status: Some(pb::RuntimeStatus {
                conditions: vec![
                    condition("RuntimeReady", &Ok(PathBuf::new())),
                    condition("NetworkReady", &network),
                ],
            }),
            info,
            runtime_handlers: vec![pb::RuntimeHandler {
                name: String::new(),
                features: Some(pb::RuntimeHandlerFeatures::default()),
            }],
            features: Some(pb::RuntimeFeatures::default()),
        }
    }

    /// The CNI configuration the pods' networks come from: the first file in name order.
    fn network_config(&self) -> Result<PathBuf, String> {
        let dir = &self.options.cni_conf_dir;
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| format!("no CNI configuration in {}: {e}", dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("conf" | "conflist" | "json")
                )
            })
            .collect();
        files.sort();
        files
            .into_iter()
            .next()
            .ok_or_else(|| format!("no CNI configuration in {}", dir.display()))
    }

    fn runtime_config(&self) -> pb::RuntimeConfigResponse {
        let driver = match self.options.cgroup_driver {
            CgroupDriver::Systemd => pb::CgroupDriver::Systemd,
            CgroupDriver::Cgroupfs => pb::CgroupDriver::Cgroupfs,
        };
        pb::RuntimeConfigResponse {
            linux: Some(pb::LinuxRuntimeConfiguration {
                cgroup_driver: driver as i32,
            }),
        }
    }
}

impl Service for Cri {
    fn unary(&self, path: &str, request: &[u8]) -> Option<Result<Vec<u8>, Status>> {
        let method = path.strip_prefix("/runtime.v1.")?;
        Some(match method {
            "RuntimeService/Version" => call(request, |_: pb::VersionRequest| Ok(self.version())),
            "RuntimeService/Status" => {
                call(request, |r: pb::StatusRequest| Ok(self.status(r.verbose)))
            }
            "RuntimeService/RuntimeConfig" => call(request, |_: pb::RuntimeConfigRequest| {
                Ok(self.runtime_config())
            }),
            "RuntimeService/UpdateRuntimeConfig" => {
                call(request, |_: pb::UpdateRuntimeConfigRequest| {
                    Ok(pb::UpdateRuntimeConfigResponse {})
                })
            }
            "ImageService/ListImages" => call(request, list_images),
            "ImageService/ImageStatus" => call(request, image_status),
            "ImageService/PullImage" => call(request, pull_image),
            "ImageService/RemoveImage" => call(request, remove_image),
            "ImageService/ImageFsInfo" => {
                call(request, |_: pb::ImageFsInfoRequest| image_fs_info())
            }
            _ => return None,
        })
    }
}

/// Decodes the request, runs `f`, encodes the answer.
fn call<Q: Message + Default, A: Message>(
    bytes: &[u8],
    f: impl FnOnce(Q) -> Result<A, Status>,
) -> Result<Vec<u8>, Status> {
    let request = Q::decode(bytes)
        .map_err(|e| Status::new(Code::InvalidArgument, format!("bad request: {e}")))?;
    f(request).map(|answer| answer.encode_to_vec())
}

/// A podbag error as a gRPC status.
pub fn status_of(e: &Error) -> Status {
    let code = match e.kind {
        ErrorKind::NotFound => Code::NotFound,
        ErrorKind::InvalidInput => Code::InvalidArgument,
        ErrorKind::Conflict => Code::FailedPrecondition,
        ErrorKind::Unsupported => Code::Unimplemented,
        _ => Code::Internal,
    };
    Status::new(code, e.to_string())
}

fn stored() -> Result<Vec<pb::Image>, Status> {
    Ok(images::group(&Image::list().map_err(|e| status_of(&e))?))
}

fn spec_image(spec: Option<pb::ImageSpec>) -> String {
    spec.map(|s| s.image).unwrap_or_default()
}

fn list_images(r: pb::ListImagesRequest) -> Result<pb::ListImagesResponse, Status> {
    let all = stored()?;
    let wanted = r
        .filter
        .and_then(|f| f.image)
        .map(|s| s.image)
        .filter(|s| !s.is_empty());
    let images = match wanted {
        Some(w) => images::find(&all, &w).cloned().into_iter().collect(),
        None => all,
    };
    Ok(pb::ListImagesResponse { images })
}

fn image_status(r: pb::ImageStatusRequest) -> Result<pb::ImageStatusResponse, Status> {
    let wanted = spec_image(r.image);
    let all = stored()?;
    // An image that is not stored is not an error: the kubelet then pulls it.
    Ok(pb::ImageStatusResponse {
        image: images::find(&all, &wanted).cloned(),
        info: HashMap::new(),
    })
}

fn pull_image(r: pb::PullImageRequest) -> Result<pb::PullImageResponse, Status> {
    let name = spec_image(r.image);
    if name.is_empty() {
        return Err(Status::new(Code::InvalidArgument, "no image to pull"));
    }
    let image = Image::pull(&name).map_err(|e| status_of(&e))?;
    Ok(pb::PullImageResponse {
        image_ref: image.digest,
    })
}

fn remove_image(r: pb::RemoveImageRequest) -> Result<pb::RemoveImageResponse, Status> {
    let wanted = spec_image(r.image);
    let all = stored()?;
    // Removing an image that is gone already succeeds.
    let Some(found) = images::find(&all, &wanted) else {
        return Ok(pb::RemoveImageResponse {});
    };
    // By ID or digest, every name goes; by name, that name.
    let names = match images::full_tag(&wanted) {
        Some(tag) => vec![tag],
        None => found.repo_tags.clone(),
    };
    for name in &names {
        match Image::remove(name) {
            Ok(_) => {}
            Err(e) if e.kind == ErrorKind::NotFound => {}
            Err(e) => return Err(status_of(&e)),
        }
    }
    Ok(pb::RemoveImageResponse {})
}

fn image_fs_info() -> Result<pb::ImageFsInfoResponse, Status> {
    let dirs = crate::paths::dirs().map_err(|e| status_of(&e))?;
    let usage = image::disk_usage().map_err(|e| status_of(&e))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let fs = pb::FilesystemUsage {
        timestamp: now,
        fs_id: Some(pb::FilesystemIdentifier {
            mountpoint: dirs.data.display().to_string(),
        }),
        used_bytes: Some(pb::UInt64Value {
            value: usage.object_bytes,
        }),
        inodes_used: Some(pb::UInt64Value {
            value: usage.objects,
        }),
    };
    Ok(pb::ImageFsInfoResponse {
        image_filesystems: vec![fs.clone()],
        container_filesystems: vec![fs],
    })
}
