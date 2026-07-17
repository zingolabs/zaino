//! Capability: the pinned view's status of a transaction.

use std::future::Future;

use zaino_primitives::types::TransactionHash;

use crate::error::PortError;
use crate::transaction_status::TransactionStatus;

/// Domain error for [`GetTransactionStatus`].
///
/// Empty: an unrecognized txid is an answer
/// ([`TransactionStatus::Unknown`]), not a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetTransactionStatusError {}

/// The pinned view's status of a transaction: mined in the best chain,
/// orphaned onto a non-best branch, or unknown.
pub trait GetTransactionStatus: Send + Sync {
    /// Where the pinned view places `txid`.
    fn get_transaction_status(
        &self,
        txid: TransactionHash,
    ) -> impl Future<Output = Result<TransactionStatus, PortError<GetTransactionStatusError>>> + Send;
}
