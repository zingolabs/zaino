//! Tonic-generated lightwallet `CompactTxStreamer` service + compact formats, gRPC framing

#![forbid(unsafe_code)]

pub mod frame;
pub mod proto;

/// Vendored lightwallet-protocol release (`LightdInfo.lightwalletProtocolVersion`), e.g. `v0.5.0`
pub const LIGHTWALLET_PROTOCOL_VERSION: &str = env!("LIGHTWALLET_PROTOCOL_VERSION");
