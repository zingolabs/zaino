//! JSON-RPC 2.0 transport: HTTP, envelope, auth (one attempt per call)
//!
//! - Returns raw `serde_json::Value` or a `DeserializeOwned` (parsing = `parse.rs`)

mod client;
mod emit;
mod endpoint;
mod envelope;
mod error;

pub(crate) use client::Call;
pub use client::{LinkLimits, RpcClient, RpcClientConfig, Timeouts};
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub use endpoint::EndpointError;
pub(crate) use endpoint::{auth_from_parts, validator_url};
pub use error::RpcError;
