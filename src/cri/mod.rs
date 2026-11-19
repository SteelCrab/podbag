//! The Kubernetes Container Runtime Interface (CRI): `podbag cri serve`.
//!
//! A kubelet talks gRPC over HTTP/2 on a Unix socket. podbag answers it without an async
//! runtime: [`hpack`] and [`h2`] are a small HTTP/2 server, [`grpc`] frames the messages,
//! and [`pb`] holds the CRI messages (generated from `dev/cri/api.proto`).

pub mod grpc;
pub mod h2;
pub mod hpack;
mod hpack_tables;
pub mod images;
#[allow(missing_docs)]
pub mod pb;
#[cfg(target_os = "linux")]
pub mod server;
#[cfg(target_os = "linux")]
pub mod service;

#[cfg(test)]
mod hpack_examples;
