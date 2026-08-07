//! Query: fetch the chain tip *as seen by the mempool's own source*.

use std::future::Future;

use zaino_primitives::types::{BlockHash, Height};

use super::QueryError;

/// Domain error for [`GetMempoolSourceTip`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetMempoolSourceTipError {
    /// The validator is not ready to report a tip (e.g. still syncing).
    #[error("validator not ready")]
    NotReady,
}

/// Fetch the chain tip of the source that supplies mempool data.
///
/// # Why this is not `GetChainTip`
///
/// [`GetChainTip`](super::GetChainTip) asks "what is the best tip?", and an
/// implementation is free to answer it from whichever transport is fastest —
/// `ZebraValidator` prefers the state database.
///
/// A mempool consumer needs something narrower: the tip *the mempool listing
/// was read against*. It tags each published mempool set with this tip so a
/// later reader can decide whether that set is still coherent with the chain,
/// without re-reading the mempool. That comparison is only sound if the tag and
/// the set come from one source — a tip read from the state database while the
/// listing came from JSON-RPC can differ by a block for reasons that have
/// nothing to do with the mempool, and the consumer would read the difference as
/// a real tip change.
///
/// So an implementation must route this to the same source that serves
/// [`GetMempoolTxids`](super::GetMempoolTxids), even when a cheaper tip is
/// available elsewhere.
pub trait GetMempoolSourceTip: Send + Sync {
    /// Fetch the mempool source's tip.
    fn get_mempool_source_tip(
        &self,
    ) -> impl Future<Output = Result<(BlockHash, Height), QueryError<GetMempoolSourceTipError>>> + Send;
}
