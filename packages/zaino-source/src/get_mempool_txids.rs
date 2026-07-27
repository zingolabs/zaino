//! Query: fetch current mempool transaction ids.

use std::future::Future;

use zaino_primitives::types::TransactionHash;

use super::QueryError;

/// Domain error for [`GetMempoolTxids`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetMempoolTxidsError {
    /// Mempool is not available (e.g. validator doesn't expose it).
    #[error("mempool unavailable")]
    Unavailable,
}

/// Fetch the txids of all transactions currently in the mempool.
///
/// Maps to `getrawmempool` over JSON-RPC.
pub trait GetMempoolTxids: Send + Sync {
    /// Fetch mempool txids.
    fn get_mempool_txids(
        &self,
    ) -> impl Future<Output = Result<Vec<TransactionHash>, QueryError<GetMempoolTxidsError>>> + Send;
}
