//! Query: fetch current mempool transaction ids.

use std::future::Future;

use zaino_primitives::types::TransactionId;

use super::QueryError;

/// Domain error for [`GetMempoolTxids`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetMempoolTxidsError {
    /// This validator does not expose a mempool.
    ///
    /// The validator answering "I do not implement this method" rather than
    /// failing to answer: a statement about the *node*, not about this request,
    /// so retrying it or a different mempool method will not help. A consumer
    /// should stop asking rather than treat it as a transient fetch failure.
    ///
    /// Distinct from having no tip to report — see
    /// [`GetMempoolSourceTip`](super::GetMempoolSourceTip), which carries no
    /// domain error at all. This one is about the mempool subsystem's
    /// availability, and sits on the methods that actually read the mempool.
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
    ) -> impl Future<Output = Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError>>> + Send;
}
