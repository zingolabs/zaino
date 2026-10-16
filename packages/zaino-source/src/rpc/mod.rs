//! JSON-RPC 2.0 transport: HTTP, envelope, auth (one attempt per call)
//!
//! - Returns raw `serde_json::Value` or a `DeserializeOwned` (parsing = `parse.rs`)

mod client;
// `pub(crate)`: the indexer block transport reports into the same families
pub(crate) mod emit;
mod endpoint;
mod envelope;
mod error;

#[cfg(any(test, feature = "testing"))]
pub(crate) use client::Served;
pub(crate) use client::{Call, RpcClient, RpcClientConfig, REPLY_ENVELOPE_BYTES};
pub use client::{LinkLimits, ResponseSizeCap, Timeouts};
pub use emit::METRICS;
pub use endpoint::EndpointError;
pub(crate) use endpoint::{auth_from_parts, validator_url};
pub use error::RpcError;
