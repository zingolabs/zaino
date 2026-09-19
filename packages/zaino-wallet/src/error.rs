//! Adapter error type.

use zaino_service::error::{BroadcastRejection, Transient};

/// A wallet-adapter failure. Chains the inner per-capability error as its
/// `source`.
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    /// Could not acquire a coherent snapshot — the one reorg race ADR-0003 admits.
    #[error(transparent)]
    Snapshot(#[from] Transient),
    /// The engine rejected the broadcast (a domain answer, not a backend failure).
    #[error(transparent)]
    Broadcast(#[from] BroadcastRejection),
}
