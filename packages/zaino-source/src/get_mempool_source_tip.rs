//! Query: fetch the chain tip *as seen by the mempool's own source*.

use std::convert::Infallible;
use std::future::Future;

use zaino_primitives::types::{BlockHash, Height};

use super::QueryError;

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
///
/// # Why this has no domain error
///
/// The single-source rule above is also what makes this method's failure modes
/// purely transport. Because the tip must come from whichever transport serves
/// the mempool, and that is the JSON-RPC path, there is no second implementation
/// that could observe a *mempool-specific* reason for having no tip — and the
/// JSON-RPC answer (`getblockchaininfo`) either returns a tip or fails at the
/// transport level. Nothing is left to name.
///
/// Contrast [`GetChainTip`](super::GetChainTip), which *does* carry a `NotReady`:
/// it is free to be answered from the state database, and the ReadState adapter
/// genuinely observes "no tip yet" as an answer rather than a failure.
///
/// So this is typed `QueryError<Infallible>` rather than given an unproducible
/// variant. A domain error no implementation can return is worse than none: it
/// tells a consumer to handle a case that cannot arise, and reads as though the
/// condition were being reported when it is not.
pub trait GetMempoolSourceTip: Send + Sync {
    /// Fetch the mempool source's tip.
    fn get_mempool_source_tip(
        &self,
    ) -> impl Future<Output = Result<(BlockHash, Height), QueryError<Infallible>>> + Send;
}
