//! Query: fetch one raw transaction from the mempool.

use std::future::Future;

use zaino_primitives::types::TransactionId;

use super::QueryError;

/// Domain error for [`GetRawMempoolTransaction`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetRawMempoolTransactionError {
    /// The validator has no such transaction.
    ///
    /// On this port that is the normal listing/fetch race — the transaction was
    /// mined or evicted between being listed and being fetched — not a failure.
    /// A consumer skips the transaction and keeps the rest of its set.
    #[error("transaction {0} not in mempool")]
    NotFound(TransactionId),
}

/// Fetch the raw bytes of one mempool transaction.
///
/// Maps to `getrawtransaction(txid, 0)` over JSON-RPC.
///
/// # Why this is separate from `GetTransaction`
///
/// [`GetTransaction`](super::GetTransaction) answers "where is this
/// transaction?" and may be routed to a state database that has no mempool at
/// all. This port answers "give me these mempool bytes", and an implementation
/// must route it to the same source that serves
/// [`GetMempoolTxids`](super::GetMempoolTxids) — otherwise a consumer
/// reconstructing the mempool would be assembling bytes from one source against
/// a listing from another.
pub trait GetRawMempoolTransaction: Send + Sync {
    /// Fetch one mempool transaction's raw bytes.
    fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>>> + Send;
}
