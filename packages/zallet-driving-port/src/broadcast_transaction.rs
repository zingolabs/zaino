//! Capability: broadcast a transaction.

use std::future::Future;

use zaino_primitives::types::TransactionHash;

use crate::error::PortError;
use crate::raw::RawTransaction;

/// Domain error for [`BroadcastTransaction`].
///
/// Two rejection shapes for v1: bytes that are not a transaction, and
/// a validation rejection with the engine's reason. Finer taxonomy is
/// added when a driver demonstrably needs to branch on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BroadcastTransactionError {
    /// The bytes do not deserialize as a transaction.
    #[error("the bytes do not deserialize as a transaction")]
    Malformed,
    /// The engine's validation rejected the transaction.
    #[error("the engine rejected the transaction: {reason}")]
    Rejected {
        /// The engine's stated rejection reason.
        reason: String,
    },
}

/// Broadcast a transaction to the network through the port.
///
/// This absorbs the side-channel Zallet holds today (a separate
/// JSON-RPC connector for `sendrawtransaction`; decision 3 of the
/// design review). Acceptance returns the transaction's txid and
/// means the engine admitted it to its mempool — an accepted
/// transaction is subsequently observable on
/// [`crate::SubscribeToMempool`]'s stream.
pub trait BroadcastTransaction: Send + Sync {
    /// Submit `transaction`; on acceptance, its txid.
    fn broadcast_transaction(
        &self,
        transaction: RawTransaction,
    ) -> impl Future<Output = Result<TransactionHash, PortError<BroadcastTransactionError>>> + Send;
}
