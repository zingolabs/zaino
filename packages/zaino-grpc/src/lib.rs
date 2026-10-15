//! lightwalletd-compatible `CompactTxStreamer` endpoint
//!
//! - [`GrpcService`] = every method in the one registry (`service.rs` `ROUTES`), dispatched by
//!   path over [`Routes`] (`GrpcSnapshots`, the chain view, the validators); compact blocks
//!   = stored bytes, never re-encoded
//! - one `GrpcSnapshot` per request or stream (`service.rs`)
//! - Domain → wire owned by the byte-producing crate (no domain crate depends on the schema)
#![forbid(unsafe_code)]

mod adaptive;
mod admission;
mod client;
mod coalesce;
mod connections;
mod deadline;
mod emit;
mod health;
mod lifetime;
mod limits;
mod memo;
mod network;
mod observe;
mod overload;
mod report;
mod routes;
mod service;
mod stall;
#[cfg(test)]
mod testing;
mod tls;
mod transport;
mod wire;

pub use admission::{AdmissionReport, AdmissionState};
pub use client::TrustedProxies;
pub use emit::{describe_metrics, sent_bytes_total, METRIC_BUCKETS};
pub use health::Health;
pub use lifetime::Retire;
pub use limits::GrpcLimits;

pub use service::{Routes, Routing};
pub use tls::{Tls, TlsError, TlsFiles};
pub use transport::{BoundGrpcService, GrpcServeError, GrpcService};
pub use zaino_proto::proto::service::LightdInfo;

/// `GetLightdInfo`'s answer from `grpc_snapshot` (`None` = the RPC answers `UNAVAILABLE`)
pub fn lightd_info<V>(
    grpc_snapshot: &zaino_grpc_snapshot::GrpcSnapshot<V>,
    network: zcash_protocol::consensus::NetworkType,
    version: &str,
) -> Option<LightdInfo> {
    routes::chain::lightd_info(grpc_snapshot, network, version).ok()
}
