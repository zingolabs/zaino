//! [`EngineSnapshot`] — the pinned read surface, and the reads whose
//! placement never varies.
//!
//! The coherence marker, the served range, and compact-block serving delegate
//! to the composed [`ChainViewSnapshot`]; the nullifier projection and the
//! chain-info aggregate are derived from it. The raw-transaction read is the
//! validator's on every routing — no local index holds transaction bytes — so
//! it is implemented here unconditionally, through the live
//! [`PassthroughProvider`]. Reads whose placement is the use case's decision live
//! in their own modules, one impl per placement.

use std::marker::PhantomData;

use futures::stream::BoxStream;

use crate::chain_view::ChainTier;
use crate::chain_view::ChainViewSnapshot;
use crate::routing::Routing;
use zaino_primitives::types::{
    BlockRef, BlockSelector, BlockchainInfo, CompactBlock, HeightRange, RawTransaction,
    TransactionId,
};
use zaino_service::ServiceableRange;
use zaino_service::error::{BlockReadError, ReadError, TxReadError};
use zaino_service::{
    ChainInfoRead, ChainSegment, CompactBlockRead, CompactNullifierRead, RawTransactionRead,
    Snapshot,
};
use zaino_source::{GetBlockchainInfo, GetTransaction};

use crate::passthrough::PassthroughProvider;

/// A pinned view over the composed chain, plus the live passthrough handle,
/// under the routing `R`.
pub struct EngineSnapshot<F, N, Src, R> {
    /// Pinned local reads — the composed FS⊕NFS view.
    local: ChainViewSnapshot<F, N>,
    /// Live passthrough reads — the validator through the source ports.
    /// Captured at snapshot time; its reads are live (not pinned), which is
    /// sound for the immutable data light clients query.
    passthrough: PassthroughProvider<Src>,
    routing: PhantomData<R>,
}

impl<F, N, Src, R> EngineSnapshot<F, N, Src, R> {
    /// Pair a pinned local view with the passthrough handle riding alongside
    /// it. Built only by the engine's [`snapshot`](crate::Engine), which
    /// captures the local sides in one shot so the pin stays coherent across
    /// the seam.
    pub(crate) fn new(
        local: ChainViewSnapshot<F, N>,
        passthrough: PassthroughProvider<Src>,
    ) -> Self {
        Self {
            local,
            passthrough,
            routing: PhantomData,
        }
    }

    /// The composed local view: both tiers, as pinned.
    pub(crate) fn local(&self) -> &ChainViewSnapshot<F, N> {
        &self.local
    }

    /// The passthrough provider.
    pub(crate) fn passthrough(&self) -> &PassthroughProvider<Src> {
        &self.passthrough
    }
}

/// Split a half-open `[start, end)` range at the seam: the part the finalised
/// store answers (heights `≤ w`) and the part the non-finalised head answers
/// (heights `> w`). Either half is `None` when empty.
///
/// ```text
/// fs  = [start, min(end, w)]        if start ≤ min(end, w)
/// nfs = [max(start, w + 1), end]    if max(start, w + 1) ≤ end
/// ```
///
/// Both halves are inclusive, like the [`HeightRange`] they split: the
/// watermark height itself is the store's, and the height above it is the
/// head's.
///
/// With no watermark the store holds nothing and the whole range is the
/// head's; a watermark at the height ceiling makes the whole range the
/// store's.
pub(crate) fn split_at_seam<F, N>(
    local: &ChainViewSnapshot<F, N>,
    range: HeightRange,
) -> (Option<HeightRange>, Option<HeightRange>)
where
    F: ChainTier,
    N: ChainTier,
{
    let Some(watermark) = local.watermark() else {
        return (None, non_empty(range));
    };
    let Some(seam) = watermark.checked_add(1) else {
        // The watermark is at the height ceiling, so nothing is above it.
        return (non_empty(range), None);
    };
    let fs = HeightRange {
        start: range.start,
        end: range.end.min(watermark),
    };
    let nfs = HeightRange {
        start: range.start.max(seam),
        end: range.end,
    };
    (non_empty(fs), non_empty(nfs))
}

/// `None` when the range names no height.
///
/// [`HeightRange`] is inclusive, so a range is empty only when its start is
/// *above* its end; `[h, h]` names one height and is not empty.
fn non_empty(range: HeightRange) -> Option<HeightRange> {
    (range.start <= range.end).then_some(range)
}

impl<F: Clone, N: Clone, Src: Clone, R> Clone for EngineSnapshot<F, N, Src, R> {
    fn clone(&self) -> Self {
        Self {
            local: self.local.clone(),
            passthrough: self.passthrough.clone(),
            routing: PhantomData,
        }
    }
}

// --- coherence marker + served range -----------------------------------------
//
// Delegated to the composed view: the pin, the coverage, and the finalised/tip
// boundary are exactly what the composer already computes across the seam.

impl<F, N, Src, R> ChainSegment for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    fn pinned_tip(&self) -> Option<BlockRef> {
        self.local.pinned_tip()
    }

    fn coverage(&self) -> Option<HeightRange> {
        self.local.coverage()
    }
}

impl<F, N, Src, R> Snapshot for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    fn serviceable_range(&self) -> Option<ServiceableRange> {
        self.local.serviceable_range()
    }
}

// --- reads whose placement never varies --------------------------------------

/// Always local: composing compact blocks across the seam is what the two
/// tiers exist for.
impl<F, N, Src, R> CompactBlockRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    async fn compact_block(
        &self,
        at: BlockSelector,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        self.local.compact_block(at).await
    }
    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        self.local.stream_compact(range)
    }
}

/// Always local: a projection of the compact block already served, reduced to
/// its spend markers. Not a separate index.
impl<F, N, Src, R> CompactNullifierRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    async fn compact_block_nullifiers(
        &self,
        at: BlockSelector,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        Ok(self
            .local
            .compact_block(at)
            .await?
            .map(crate::nullifiers::strip_to_nullifiers))
    }
}

/// Always passthrough: the aggregate describes one chain position, so it is
/// relayed whole from the validator rather than assembled from a mix of local
/// and passthrough reads that could disagree about the height they describe. It
/// moves local in one piece once the value-pool cumulative bridge exists, or not
/// at all.
impl<F, N, Src, R> ChainInfoRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: GetBlockchainInfo + Send + Sync + 'static,
    R: Routing,
{
    async fn chain_info(&self) -> Result<BlockchainInfo, ReadError> {
        self.passthrough.chain_info().await
    }
}

/// Always passthrough: no local index holds transaction bytes, and the wallet parses
/// them itself, so the validator's `getrawtransaction` answer relays as is.
impl<F, N, Src, R> RawTransactionRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: GetTransaction + Send + Sync + 'static,
    R: Routing,
{
    async fn raw_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        self.passthrough.raw_transaction(id).await
    }
}
