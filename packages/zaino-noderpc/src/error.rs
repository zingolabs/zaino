//! Adapter error type.
//!
//! Note the contrast with the light-serve adapter: there a broadcast rejection
//! rides out in the `SendResponse`; here `sendrawtransaction` surfaces it as an
//! RPC error, matching node-RPC semantics. Same domain answer, two wire shapes —
//! decided by the adapter, not the port.

use zaino_service::error::{BroadcastRejection, ReadError, SpendReadError, Transient};

/// A node-RPC handler failure.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// The chain has no tip yet — not built to any height.
    #[error("no blocks available yet")]
    NoBlocks,
    /// Could not acquire a coherent snapshot; likely resolves on retry.
    #[error(transparent)]
    Unavailable(#[from] Transient),
    /// A wire parameter failed validation (bad hex, wrong length, ...).
    #[error("invalid parameter: {0}")]
    InvalidParams(String),
    /// The engine rejected a broadcast.
    #[error(transparent)]
    Rejected(#[from] BroadcastRejection),
    /// A spend-status read failed.
    #[error(transparent)]
    SpendRead(#[from] SpendReadError),
    /// A generic read (e.g. the chain-info aggregate) failed.
    #[error(transparent)]
    Read(#[from] ReadError),
}
