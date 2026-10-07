//! `zaino-grpc` — the lightwalletd-compatible `CompactTxStreamer` endpoint.
//!
//! - [`GrpcService`] = every method, dispatched by path over [`Routes`] (the enabled indexes,
//!   the chain view, the validators); compact blocks = stored bytes, never re-encoded
//! - Domain → wire owned by the byte-producing crate (no domain crate depends on the schema)
#![forbid(unsafe_code)]

mod admission;
mod client;
mod connections;
mod emit;
mod limits;
mod memo;
mod observe;
mod report;
mod routes;
mod service;
mod stall;
#[cfg(test)]
mod testing;
mod tls;
mod transport;
mod wire;

pub use client::TrustedProxies;
pub use emit::{describe_metrics, sent_bytes_total, METRIC_BUCKETS};
pub use limits::GrpcLimits;
pub use service::Routes;
pub use tls::{Tls, TlsError, TlsFiles};
pub use transport::{BoundGrpcService, GrpcServeError, GrpcService};
