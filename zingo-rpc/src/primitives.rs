//! Zingo-RPC primitives.

use std::sync::{atomic::AtomicBool, Arc};

/// Configuration data for gRPC server.
pub struct ProxyConfig {
    /// Lightwalletd uri.
    /// Used by grpc_passthrough to pass on unimplemented RPCs.
    pub lightwalletd_uri: http::Uri,
    /// Zebrad uri.
    pub zebrad_uri: http::Uri,
    /// Represents the Online status of the gRPC server.
    pub online: Arc<AtomicBool>,
}
