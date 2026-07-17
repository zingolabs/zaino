//! Capability: read a mined transaction by txid.

use std::future::Future;

use zaino_primitives::types::TransactionHash;

use crate::error::PortError;
use crate::mined_transaction::MinedTransaction;

/// Domain error for [`GetMinedTransaction`].
///
/// Empty: absence is an answer (`Ok(None)`), not a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetMinedTransactionError {}

/// Read a transaction as mined in the pinned best chain.
///
/// Answers `Some` exactly for transactions mined in the pinned view.
/// A transaction that is merely in the mempool is not served here —
/// it travels the port's mempool surface, which ADR 0001 keeps apart
/// from chain state.
pub trait GetMinedTransaction: Send + Sync {
    /// The mined transaction with `txid`, or `None` when the pinned
    /// best chain does not contain it.
    fn get_mined_transaction(
        &self,
        txid: TransactionHash,
    ) -> impl Future<Output = Result<Option<MinedTransaction>, PortError<GetMinedTransactionError>>> + Send;
}
