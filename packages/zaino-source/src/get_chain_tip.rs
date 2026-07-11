//! Query: fetch the current best chain tip.

use std::future::Future;

use zaino_primitives::types::{BlockHash, Height};

use super::QueryError;

/// Domain error for [`GetChainTip`].
///
/// There are no domain-level failure modes for "what is the tip?" beyond
/// transport failure — the validator always has a tip. This enum exists
/// for forward compatibility (e.g. a validator that reports "still syncing").
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetChainTipError {
    /// The validator is not ready to report a tip (e.g. still syncing).
    #[error("validator not ready")]
    NotReady,
}

/// Fetch the current best chain tip (hash + height).
///
/// Maps to `getbestblockhash()` + `getblock(hash, 0)` over JSON-RPC,
/// or the equivalent ReadState query.
pub trait GetChainTip: Send + Sync {
    /// Fetch current tip.
    fn get_chain_tip(
        &self,
    ) -> impl Future<Output = Result<(BlockHash, Height), QueryError<GetChainTipError>>> + Send;
}
