//! Query: enumerate the known tips of the block tree.

use std::future::Future;

use zaino_primitives::types::rpc::ChainTip;

use super::QueryError;

/// Domain error for [`GetChainTips`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetChainTipsError {
    /// The validator is not ready to enumerate tips (e.g. still syncing).
    #[error("validator not ready")]
    NotReady,
}

/// Enumerate every tip of the block tree the validator knows: the active tip
/// plus any competing branches it has retained.
///
/// Distinct from [`GetChainTip`](super::GetChainTip), which answers only "what
/// is the best chain tip?". A validator that tracks no side chains answers this
/// with a single active tip, so the result is never empty on a synced node.
///
/// Maps to `getchaintips` over JSON-RPC.
pub trait GetChainTips: Send + Sync {
    /// Fetch all known chain tips.
    fn get_chain_tips(
        &self,
    ) -> impl Future<Output = Result<Vec<ChainTip>, QueryError<GetChainTipsError>>> + Send;
}
