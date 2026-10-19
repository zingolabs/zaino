//! JSON-RPC 2.0 transport: HTTP, envelope, auth (one attempt per call)
//!
//! - Returns raw `serde_json::Value` or a `DeserializeOwned` (parsing = `json.rs`)

mod auth;
mod client;
mod envelope;
mod error;

pub(crate) use auth::auth_from_parts;
pub use client::ResponseSizeCap;
pub(crate) use client::{Call, RpcClient, RpcClientConfig, REPLY_ENVELOPE_BYTES};
pub use error::RpcError;
