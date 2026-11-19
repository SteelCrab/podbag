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

pub use super::mapping::CgroupDriver;

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
    runtime: super::runtime::Runtime,
}

impl Cri {
    pub fn new(options: Options) -> crate::Result<Cri> {
        Ok(Cri {
            runtime: super::runtime::Runtime::new(options.clone())?,
            options,
        })
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
            "RuntimeService/RunPodSandbox" => call(request, |r: pb::RunPodSandboxRequest| {
                let config = r.config.unwrap_or_default();
                let pod_sandbox_id = self.runtime.run_pod_sandbox(config)?;
                Ok(pb::RunPodSandboxResponse { pod_sandbox_id })
            }),
            "RuntimeService/StopPodSandbox" => call(request, |r: pb::StopPodSandboxRequest| {
                self.runtime.stop_pod_sandbox(&r.pod_sandbox_id)?;
                Ok(pb::StopPodSandboxResponse {})
            }),
            "RuntimeService/RemovePodSandbox" => call(request, |r: pb::RemovePodSandboxRequest| {
                self.runtime.remove_pod_sandbox(&r.pod_sandbox_id)?;
                Ok(pb::RemovePodSandboxResponse {})
            }),
            "RuntimeService/PodSandboxStatus" => call(request, |r: pb::PodSandboxStatusRequest| {
                self.runtime
                    .pod_sandbox_status(&r.pod_sandbox_id, r.verbose)
            }),
            "RuntimeService/ListPodSandbox" => call(request, |r: pb::ListPodSandboxRequest| {
                Ok(pb::ListPodSandboxResponse {
                    items: self.runtime.list_pod_sandbox(r.filter)?,
                })
            }),
            "RuntimeService/CreateContainer" => call(request, |r: pb::CreateContainerRequest| {
                let config = r.config.unwrap_or_default();
                let container_id = self.runtime.create_container(&r.pod_sandbox_id, config)?;
                Ok(pb::CreateContainerResponse { container_id })
            }),
            "RuntimeService/StartContainer" => call(request, |r: pb::StartContainerRequest| {
                self.runtime.start_container(&r.container_id)?;
                Ok(pb::StartContainerResponse {})
            }),
            "RuntimeService/StopContainer" => call(request, |r: pb::StopContainerRequest| {
                self.runtime.stop_container(&r.container_id, r.timeout)?;
                Ok(pb::StopContainerResponse {})
            }),
            "RuntimeService/RemoveContainer" => call(request, |r: pb::RemoveContainerRequest| {
                self.runtime.remove_container(&r.container_id)?;
                Ok(pb::RemoveContainerResponse {})
            }),
            "RuntimeService/ListContainers" => call(request, |r: pb::ListContainersRequest| {
                Ok(pb::ListContainersResponse {
                    containers: self.runtime.list_containers(r.filter)?,
                })
            }),
            "RuntimeService/ContainerStatus" => call(request, |r: pb::ContainerStatusRequest| {
                self.runtime.container_status(&r.container_id, r.verbose)
            }),
            "RuntimeService/UpdateContainerResources" => {
                call(request, |r: pb::UpdateContainerResourcesRequest| {
                    self.runtime
                        .update_container_resources(&r.container_id, r.linux)?;
                    Ok(pb::UpdateContainerResourcesResponse {})
                })
            }
            "RuntimeService/UpdatePodSandboxResources" => {
                call(request, |_: pb::UpdatePodSandboxResourcesRequest| {
                    Ok(pb::UpdatePodSandboxResourcesResponse {})
                })
            }
            "RuntimeService/ReopenContainerLog" => {
                call(request, |r: pb::ReopenContainerLogRequest| {
                    self.runtime.reopen_container_log(&r.container_id)?;
                    Ok(pb::ReopenContainerLogResponse {})
                })
            }
            "RuntimeService/ExecSync" => call(request, |r: pb::ExecSyncRequest| {
                self.runtime.exec_sync(&r.container_id, r.cmd, r.timeout)
            }),
            "RuntimeService/ContainerStats" => call(request, |r: pb::ContainerStatsRequest| {
                Ok(pb::ContainerStatsResponse {
                    stats: Some(self.runtime.container_stats(&r.container_id)?),
                })
            }),
            "RuntimeService/ListContainerStats" => {
                call(request, |r: pb::ListContainerStatsRequest| {
                    Ok(pb::ListContainerStatsResponse {
                        stats: self.runtime.list_container_stats(r.filter)?,
                    })
                })
            }
            "RuntimeService/PodSandboxStats" => call(request, |r: pb::PodSandboxStatsRequest| {
                Ok(pb::PodSandboxStatsResponse {
                    stats: Some(self.runtime.pod_sandbox_stats(&r.pod_sandbox_id)?),
                })
            }),
            "RuntimeService/ListPodSandboxStats" => {
                call(request, |r: pb::ListPodSandboxStatsRequest| {
                    Ok(pb::ListPodSandboxStatsResponse {
                        stats: self.runtime.list_pod_sandbox_stats(r.filter)?,
                    })
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

    fn stream(
        &self,
        path: &str,
        request: &[u8],
        out: &mut super::grpc::Stream<'_>,
    ) -> Option<Result<(), Status>> {
        let method = path.strip_prefix("/runtime.v1.")?;
        // The lists come whole, in one message (none for an empty list).
        let mut send_one = |bytes: Vec<u8>, empty: bool| {
            if empty { Ok(()) } else { out.send(&bytes) }
        };
        Some(match method {
            "RuntimeService/GetContainerEvents" => self.container_events(request, out),
            "RuntimeService/StreamPodSandboxes" => {
                decode(request).and_then(|r: pb::StreamPodSandboxesRequest| {
                    let items = self.runtime.list_pod_sandbox(r.filter)?;
                    let empty = items.is_empty();
                    send_one(
                        pb::StreamPodSandboxesResponse {
                            pod_sandboxes: items,
                        }
                        .encode_to_vec(),
                        empty,
                    )
                })
            }
            "RuntimeService/StreamContainers" => {
                decode(request).and_then(|r: pb::StreamContainersRequest| {
                    let items = self.runtime.list_containers(r.filter)?;
                    let empty = items.is_empty();
                    send_one(
                        pb::StreamContainersResponse { containers: items }.encode_to_vec(),
                        empty,
                    )
                })
            }
            "RuntimeService/StreamContainerStats" => {
                decode(request).and_then(|r: pb::StreamContainerStatsRequest| {
                    let items = self.runtime.list_container_stats(r.filter)?;
                    let empty = items.is_empty();
                    send_one(
                        pb::StreamContainerStatsResponse {
                            container_stats: items,
                        }
                        .encode_to_vec(),
                        empty,
                    )
                })
            }
            "RuntimeService/StreamPodSandboxStats" => {
                decode(request).and_then(|r: pb::StreamPodSandboxStatsRequest| {
                    let items = self.runtime.list_pod_sandbox_stats(r.filter)?;
                    let empty = items.is_empty();
                    send_one(
                        pb::StreamPodSandboxStatsResponse {
                            pod_sandbox_stats: items,
                        }
                        .encode_to_vec(),
                        empty,
                    )
                })
            }
            "ImageService/StreamImages" => {
                decode(request).and_then(|r: pb::StreamImagesRequest| {
                    let items = list_images(pb::ListImagesRequest { filter: r.filter })?.images;
                    let empty = items.is_empty();
                    send_one(
                        pb::StreamImagesResponse { images: items }.encode_to_vec(),
                        empty,
                    )
                })
            }
            _ => return None,
        })
    }
}

impl Cri {
    /// Container events until the caller goes away.
    fn container_events(
        &self,
        request: &[u8],
        out: &mut super::grpc::Stream<'_>,
    ) -> Result<(), Status> {
        let _: pb::GetEventsRequest = decode(request)?;
        let listener = self.runtime.events.listen();
        // The headers go out now: the kubelet waits for them before it lists the pods.
        out.start()?;
        loop {
            if out.cancelled() {
                return Ok(());
            }
            let Some((id, kind, at)) = listener.next(std::time::Duration::from_secs(1)) else {
                continue;
            };
            let status = self.runtime.status_of(&id).ok();
            let sandbox = status
                .as_ref()
                .and_then(|_| self.runtime.sandbox_of(&id))
                .and_then(|s| self.runtime.pod_sandbox_status(&s, false).ok())
                .and_then(|r| r.status);
            let event = pb::ContainerEventResponse {
                container_id: id,
                container_event_type: kind as i32,
                created_at: at,
                pod_sandbox_status: sandbox,
                containers_statuses: status.into_iter().collect(),
            };
            out.send(&event.encode_to_vec())?;
        }
    }
}

fn decode<Q: Message + Default>(bytes: &[u8]) -> Result<Q, Status> {
    Q::decode(bytes).map_err(|e| Status::new(Code::InvalidArgument, format!("bad request: {e}")))
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

/// The stored image the kubelet means: an ID, a name or a name with a digest.
pub fn find_image(wanted: &str) -> Result<Image, Status> {
    let all = Image::list().map_err(|e| status_of(&e))?;
    let grouped = images::group(&all);
    let found = images::find(&grouped, wanted).ok_or_else(|| {
        Status::new(
            Code::NotFound,
            format!("image {wanted:?} not found: pull it first"),
        )
    })?;
    // The name asked for when it is one of the image's; any of its names otherwise.
    let tag = images::full_tag(wanted).filter(|t| found.repo_tags.contains(t));
    all.into_iter()
        .filter(|i| i.digest == found.id)
        .find(|i| tag.as_ref().is_none_or(|t| &i.name == t))
        .ok_or_else(|| Status::new(Code::NotFound, format!("image {wanted:?} not found")))
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
