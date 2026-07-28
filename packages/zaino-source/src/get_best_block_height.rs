//! Query: fetch the current best block height.

use std::future::Future;

use zaino_primitives::types::Height;

use super::QueryError;

/// Domain error for [`GetBestBlockHeight`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBestBlockHeightError {
    /// The validator is not ready (e.g. still syncing).
    #[error("validator not ready")]
    NotReady,
}

/// Fetch the current best block height.
///
/// Maps to `getblockcount` over JSON-RPC, or the equivalent ReadState
/// query. Lighter than [`super::GetChainTip`] when the hash isn't needed.
pub trait GetBestBlockHeight: Send + Sync {
    /// Fetch current tip height.
    fn get_best_block_height(
        &self,
    ) -> impl Future<Output = Result<Height, QueryError<GetBestBlockHeightError>>> + Send;
}
