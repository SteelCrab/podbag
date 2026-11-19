//! The Kubernetes Container Runtime Interface (CRI): `podbag cri serve`.
//!
//! A kubelet talks gRPC over HTTP/2 on a Unix socket. podbag answers it without an async
//! runtime: [`hpack`] and [`h2`] are a small HTTP/2 server, [`grpc`] frames the messages,
//! and [`pb`] holds the CRI messages (generated from `dev/cri/api.proto`). Pods are
//! namespaces held by bind mounts (`pod`) and networks made by CNI plugins ([`cni`]);
//! their containers are podbag containers ([`mapping`], `runtime`).

pub mod cni;
pub mod events;
pub mod grpc;
pub mod h2;
pub mod hpack;
mod hpack_tables;
pub mod images;
pub mod logs;
pub mod mapping;
#[allow(missing_docs)]
pub mod pb;
#[cfg(target_os = "linux")]
pub mod pod;
pub mod records;
#[cfg(target_os = "linux")]
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod server;
#[cfg(target_os = "linux")]
pub mod service;
pub mod spdy;
mod spdy_dict;
pub mod stats;
#[cfg(target_os = "linux")]
pub mod streaming;

#[cfg(test)]
mod hpack_examples;
