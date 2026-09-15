//! Adapter error type.

use zaino_service::error::{BroadcastRejection, Transient};

/// A wallet-adapter failure.
///
/// The inner surface's error types are not `std::error::Error` yet, so they are
/// carried by value and Debug-formatted rather than chained as an error
/// `source`. Once the inner errors derive `Error`, these become `#[source]`.
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    /// Could not acquire a coherent snapshot — the one reorg race ADR-0003 admits.
    #[error("could not acquire a snapshot: {0:?}")]
    Snapshot(Transient),
    /// The engine rejected the broadcast (a domain answer, not a backend failure).
    #[error("broadcast rejected: {0:?}")]
    Broadcast(BroadcastRejection),
}
