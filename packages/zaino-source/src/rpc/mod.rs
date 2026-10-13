//! JSON-RPC 2.0 transport: HTTP, envelope, auth (one attempt per call)
//!
//! - Returns raw `serde_json::Value` or a `DeserializeOwned` (parsing = `parse.rs`)

mod client;
// `pub(crate)`: the indexer block transport reports into the same families
pub(crate) mod emit;
mod endpoint;
mod envelope;
mod error;

pub(crate) use client::{Call, RpcClient, RpcClientConfig};
pub use client::{LinkLimits, Timeouts};
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub use endpoint::EndpointError;
pub(crate) use endpoint::{auth_from_parts, validator_url};
pub use error::RpcError;
