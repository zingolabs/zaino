//! Adapter error type.
//!
//! Note the contrast with the light-serve adapter: there a broadcast rejection
//! rides out in the `SendResponse`; here `sendrawtransaction` surfaces it as an
//! RPC error, matching node-RPC semantics. Same domain answer, two wire shapes —
//! decided by the adapter, not the port.

use zaino_service::error::{
    AddressReadError, BlockDeltasError, BlockHashReadError, BroadcastRejection, MempoolReadError,
    ReadError, SpendReadError, TransactionViewError, Transient, TreestateReadError, TxReadError,
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
    /// A requested block height is beyond the chain — zcashd's `getblockhash`
    /// out-of-range error, which has its own code (`-8`) distinct from the
    /// not-found code.
    #[error("{0}")]
    OutOfRange(String),
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
    /// A treestate read failed (`z_gettreestate` / `z_getsubtreesbyindex`).
    #[error(transparent)]
    Treestate(#[from] TreestateReadError),
    /// A mempool listing read failed.
    #[error(transparent)]
    MempoolRead(#[from] MempoolReadError),
    /// The `getblockhashes` timestamp-range selection failed — a hole in the
    /// chain view or a tier read failure, never bad client input.
    #[error(transparent)]
    BlockHashRead(#[from] BlockHashReadError),
    /// Composing `getblockdeltas` failed — a resolution failure, a chain-view hole
    /// in the median-time window, or a corrupt amount, never bad client input.
    #[error(transparent)]
    BlockDeltas(#[from] BlockDeltasError),
    /// Locating an outpoint's spend for `getspentinfo` failed — a tier read
    /// failure, never bad client input (an unspent or unknown outpoint is the
    /// not-found error above, not this).
    #[error(transparent)]
    Spend(#[from] SpendReadError),
}
