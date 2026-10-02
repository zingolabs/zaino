//! Adapter error type.
//!
//! Note the contrast with the light-serve adapter: there a broadcast rejection
//! rides out in the `SendResponse`; here `sendrawtransaction` surfaces it as an
//! RPC error, matching node-RPC semantics. Same domain answer, two wire shapes —
//! decided by the adapter, not the port.

use zaino_service::error::{
    AddressReadError, BroadcastRejection, ReadError, TransactionViewError, Transient, TxReadError,
};
use zaino_service::NodeStatusError;

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
    /// A generic read (e.g. the chain-info aggregate) failed.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// A transparent-address read failed.
    #[error(transparent)]
    AddressRead(#[from] AddressReadError),
    /// The requested object is not known to this indexer.
    #[error("{0}")]
    NotFound(String),
    /// A transaction read failed.
    #[error(transparent)]
    TxRead(#[from] TxReadError),
    /// Resolving a transaction's inputs to the outputs they spend failed — a
    /// transport failure or a source inconsistency, never bad client input.
    #[error(transparent)]
    TransactionView(#[from] TransactionViewError),
    /// A node-status read failed.
    #[error(transparent)]
    NodeStatus(#[from] NodeStatusError),
}
