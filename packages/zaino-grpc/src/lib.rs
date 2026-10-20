//! lightwalletd-compatible `CompactTxStreamer` endpoint
//!
//! - [`GrpcService`] = every method in the one registry (`service.rs` `ROUTES`), dispatched by
//!   path over [`RouteDeps`] (`GrpcSnapshots`, the chain view, the validators)
//! - compact blocks = stored bytes, never re-encoded
//! - one `GrpcSnapshot` per request or stream (`service.rs`)
//! - Domain → wire owned by the byte-producing crate (no domain crate depends on the schema)
#![forbid(unsafe_code)]

mod adaptive;
mod admission;
mod coalesce;
mod connections;
mod deadline;
mod health;
mod labels;
mod layer;
mod lifetime;
mod limits;
mod memo;
mod metrics;
mod observe;
mod overload;
mod proxy_protocol;
mod read_lanes;
mod routes;
mod service;
mod stall;
mod summary;
#[cfg(test)]
mod testing;
mod tls;
mod transport;
mod wire;

pub use crate::metrics::{describe_metrics, sent_bytes_total};
pub use admission::{AdmissionReport, AdmissionState};
pub use health::Health;
pub use lifetime::{Hold, Retire};
pub use limits::GrpcLimits;
pub use proxy_protocol::TrustedProxies;

pub use service::{RouteDeps, Routing};
pub use tls::{Tls, TlsError, TlsFiles};
pub use transport::{BoundGrpcService, GrpcServeError, GrpcService};
pub use zaino_proto::proto::service::LightdInfo;

/// `GetLightdInfo`'s answer from `grpc_snapshot` (`None` = the RPC answers `UNAVAILABLE`)
pub fn lightd_info<V>(
    grpc_snapshot: &zaino_grpc_snapshot::GrpcSnapshot<V>,
    network: zcash_protocol::consensus::NetworkType,
    version: &str,
) -> Option<LightdInfo> {
    routes::info::lightd_info(grpc_snapshot, network, version).ok()
}
