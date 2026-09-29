//! JSON-RPC 2.0 transport: HTTP, envelope, work-queue retry, auth
//!
//! - Returns raw `serde_json::Value` or a `DeserializeOwned` (parsing = `parse.rs`)

mod client;
mod emit;
mod envelope;
mod error;
mod probe;

pub use client::{RpcClient, RpcClientConfig};
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub use error::RpcError;
pub use probe::ProbeError;
pub(crate) use probe::{auth_from_parts, probe_node};
