//! Lightwallet service RPC implementations and Nym functionality.

use std::sync::{atomic::AtomicBool, Arc};

#[cfg(feature = "nym_poc")]
pub mod nymwalletservice;
#[cfg(not(feature = "nym_poc"))]
pub mod service;

pub mod nymservice;

#[derive(Debug, Clone)]
/// Configuration data for gRPC server.
pub struct GrpcClient {
    /// Lightwalletd uri.
    /// Used by grpc_passthrough to pass on unimplemented RPCs.
    pub lightwalletd_uri: http::Uri,
    /// Zebrad uri.
    pub zebrad_uri: http::Uri,
    /// Represents the Online status of the gRPC server.
    pub online: Arc<AtomicBool>,
}
