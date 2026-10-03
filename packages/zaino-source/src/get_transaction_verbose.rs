//! Query: fetch a transaction by txid, decoded into its pool structure.

use std::future::Future;

use zaino_primitives::types::{Transaction, TransactionDetail, TransactionId, TransactionLocation};

use super::{QueryError, ValidatorSource};

/// A transaction decoded into its pool structure, with where it lives.
///
/// Paired because a caller needs both and two fetches could disagree: the
/// transaction could be mined between them.
#[derive(Debug, Clone)]
pub struct DecodedTransaction {
    /// The transaction, decomposed by pool.
    pub transaction: Transaction,
    /// The envelope, coinbase input, and Sprout values the indexing shape
    /// ([`transaction`](Self::transaction)) drops.
    pub detail: TransactionDetail,
    /// The transaction's raw consensus bytes, carried so the explorer's `hex`
    /// field renders from the same decode rather than a refetch.
    pub raw: Vec<u8>,
    /// Where the transaction was found.
    pub location: TransactionLocation,
}

/// Domain error for [`GetTransactionVerbose`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetTransactionVerboseError {
    /// No transaction with this txid exists.
    #[error("transaction not found: {0}")]
    NotFound(TransactionId),
}

/// Fetch a transaction by its txid, decoded into its pool structure.
///
/// Separate from [`OneShotGetTransaction`](crate::OneShotGetTransaction): that
/// port yields the raw consensus bytes, which is what a wallet wants because it
/// parses them itself. This port yields the transaction decomposed by pool
/// (transparent, sapling, orchard, ironwood) — the explorer surface, whose
/// decoding needs the validator's chain library and so lives in the adapter,
/// not in the validator-agnostic core.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetTransactionVerbose: ValidatorSource + Send + Sync {
    /// Fetch and decode a transaction.
    fn get_transaction_verbose(
        &self,
        txid: TransactionId,
    ) -> impl Future<
        Output = Result<
            DecodedTransaction,
            QueryError<GetTransactionVerboseError, Self::NonDomain>,
        >,
    > + Send;
}
