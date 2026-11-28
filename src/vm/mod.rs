//! The macOS VM's shared parts: the CLI protocol, the data disk template, the flows of
//! published UDP ports, and (on Linux) the guest's init, agent, and file synchronization.

#[cfg(target_os = "linux")]
pub mod agent;
pub mod dhcp;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod ext4;
#[cfg(target_os = "linux")]
pub mod guest;
pub mod proto;
#[cfg(unix)]
pub(crate) mod shares;
#[cfg(target_os = "linux")]
pub(crate) mod sync;
pub mod template;
pub mod udp;

/// The macOS VM disk as reported by `podbag vm storage`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskStorage {
    pub path: std::path::PathBuf,
    pub exists: bool,
    pub configured: bool,
    pub editable: bool,
    pub size_bytes: u64,
    pub allocated_bytes: u64,
    pub filesystem_bytes: Option<u64>,
    pub growth_pending: bool,
    pub max_size_gib: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<std::path::PathBuf>,
}
