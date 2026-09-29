//! `zaino-grpc` — the lightwalletd-compatible `CompactTxStreamer` adapter.
//!
//! - [`Router`] dispatches by method path: enabled index / chain view claims its methods
//!   (compact blocks = stored bytes, never re-encoded)
//! - Unclaimed → [`GrpcService`]: `GetTransaction`, `GetLightdInfo` & `SendTransaction` (w/o
//!   chain view) off the validator, rest unimplemented
//! - Domain → wire owned by the byte-producing crate (no domain crate depends on the schema)
#![forbid(unsafe_code)]

mod admission;
mod client;
mod connections;
mod emit;
mod grpc;
mod limits;
mod memo;
mod observe;
mod router;
mod stall;
mod transport;
mod validator;

pub use client::TrustedProxies;
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub use grpc::GrpcService;
pub use limits::{GrpcLimits, ReadLanes};
pub use router::ChainViewHandles;
pub use router::Router;
pub use transport::{BoundGrpcServer, GrpcServeError, GrpcServer};
pub use validator::{ProjectCompact, Relay};
pub use validator::{ValidatorHandler, ValidatorPorts};
