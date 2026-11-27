//! The macOS VM's shared parts: the CLI protocol, the data disk template, the flows of
//! published UDP ports, and (on Linux) the guest's init, agent, and file synchronization.

#[cfg(target_os = "linux")]
pub mod agent;
pub mod dhcp;
#[cfg(target_os = "linux")]
pub mod guest;
pub mod proto;
#[cfg(target_os = "linux")]
pub(crate) mod sync;
pub mod template;
pub mod udp;
