//! Swarm mode on one node: services, tasks, secrets and configs behind the Docker API.
//!
//! There is no daemon. Each task is a podbag container; the state of the swarm is in
//! files under `<data>/swarm`; every API call that changes what is wanted, and every
//! read of services and tasks, compares the wanted tasks with the live containers and
//! fixes the difference.

pub mod body;
pub mod http;
pub mod net;
pub mod ops;
pub mod plan;
pub mod shapes;
pub mod store;
pub mod tasks;
pub mod update;

pub use ops::Engine;

/// Whether this process runs rootless: then there are no NAT rules on the host, and one
/// task publishes a service's ports.
pub fn rootless() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() != 0 }
}

/// Whether a rolling update runs in this process: the API service waits for it before
/// it exits when idle.
pub fn busy() -> bool {
    update::busy()
}
