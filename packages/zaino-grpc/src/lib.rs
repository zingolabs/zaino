//! lightwalletd-compatible `CompactTxStreamer` endpoint
//!
//! - [`GrpcService`] = every method in the one registry (`service.rs` `Method`), dispatched by
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
mod guard;
mod health;
mod lifetime;
mod limits;
mod memo;
mod metrics;
mod overload;
mod pressure;
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

pub use crate::metrics::{sent_bytes_total, METRICS};
pub use admission::{AdmissionReport, AdmissionState};
pub use health::Health;
pub use lifetime::{Hold, Retire};
pub use limits::{GrpcLimits, OverloadLimits};
pub use overload::OverloadCause;
pub use proxy_protocol::TrustedProxies;

pub use service::{RouteDeps, Routing};
pub use tls::{Tls, TlsError, TlsFiles};
pub use transport::{BoundGrpcService, GrpcServeError, GrpcService};

/// RPCs unserved without `kind` (`Method` order; none = an index no RPC reads directly)
pub fn rpcs_gated_by(kind: zaino_persistence::IndexKind) -> impl Iterator<Item = &'static str> {
    use strum::VariantArray as _;
    let methods = service::Method::VARIANTS.iter();
    methods.filter(move |method| method.route().index == Some(kind)).map(|method| method.name())
}
