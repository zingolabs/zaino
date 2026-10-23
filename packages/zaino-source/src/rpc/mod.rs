//! JSON-RPC 2.0 transport: HTTP, envelope, auth (one attempt per call)
//!
//! - Returns raw `serde_json::Value` or a `DeserializeOwned` (parsing = `json.rs`)

mod client;
mod envelope;
mod error;

pub use client::ResponseSizeCap;
#[cfg(any(test, feature = "testing"))]
pub(crate) use client::Served;
pub(crate) use client::{Call, RpcClient, RpcClientConfig, REPLY_ENVELOPE_BYTES};
pub use error::RpcError;
