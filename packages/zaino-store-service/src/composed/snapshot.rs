//! [`ComposedSnapshot`] — the pinned read surface, and the reads whose
//! placement never varies.
//!
//! The coherence marker, the served range, and compact-block serving delegate
//! to the composed [`ChainViewSnapshot`]; the nullifier projection and the
//! chain-info aggregate are derived from it. The raw-transaction read is the
//! validator's on every routing — no local index holds transaction bytes — so
//! it is implemented here unconditionally, through the live
//! [`RemoteChainView`]. Reads whose placement is the use case's decision live
//! in their own modules, one impl per placement.

use std::marker::PhantomData;

use futures::stream::BoxStream;

use zaino_chainview::ChainViewSnapshot;
use zaino_core::{
    BlockId, BlockRef, ChainInfo, CompactBlock, Height, HeightRange, RawTransaction,
    ServiceableRange, TransactionId,
};
use zaino_service::error::{BlockReadError, ReadError, TxReadError};
use zaino_service::routing::Routing;
use zaino_service::{
    ChainInfoRead, ChainSegment, CompactBlockRead, CompactNullifierRead, RawTransactionRead,
    Snapshot,
};
use zaino_source::GetTransaction;

use crate::remote::RemoteChainView;

/// A pinned view over the composed chain, plus the live passthrough handle,
/// under the routing `R`.
pub struct ComposedSnapshot<F, N, Src, R> {
    /// Pinned local reads — the composed FS⊕NFS view.
    local: ChainViewSnapshot<F, N>,
    /// Live passthrough reads — the validator through the source ports.
    /// Captured at snapshot time; its reads are live (not pinned), which is
    /// sound for the immutable data light clients query.
    remote: RemoteChainView<Src>,
    routing: PhantomData<R>,
}

impl<F, N, Src, R> ComposedSnapshot<F, N, Src, R> {
    /// Pair a pinned local view with the passthrough handle riding alongside
    /// it. Built only by the engine's [`snapshot`](crate::Composed), which
    /// captures the local sides in one shot so the pin stays coherent across
    /// the seam.
    pub(crate) fn new(local: ChainViewSnapshot<F, N>, remote: RemoteChainView<Src>) -> Self {
        Self {
            local,
            remote,
            routing: PhantomData,
        }
    }

    /// The composed local view: both tiers, as pinned.
    pub(crate) fn local(&self) -> &ChainViewSnapshot<F, N> {
        &self.local
    }

    /// The passthrough provider.
    pub(crate) fn remote(&self) -> &RemoteChainView<Src> {
        &self.remote
    }
}

/// Split a half-open `[start, end)` range at the seam: the part the finalised
/// store answers (heights `≤ w`) and the part the non-finalised head answers
/// (heights `> w`). Either half is `None` when empty.
///
/// ```text
/// fs  = [start, min(end, w + 1))     if start ≤ w
/// nfs = [max(start, w + 1), end)     if end > w + 1
/// ```
///
/// With no watermark the store holds nothing and the whole range is the
/// head's; a watermark at the height ceiling makes the whole range the
/// store's.
pub(crate) fn split_at_seam<F, N>(
    local: &ChainViewSnapshot<F, N>,
    range: HeightRange,
) -> (Option<HeightRange>, Option<HeightRange>)
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
{
    let Some(watermark) = local.watermark() else {
        return (None, non_empty(range));
    };
    let Some(seam) = watermark.checked_add(1) else {
        return (non_empty(range), None);
    };
    let fs = HeightRange {
        start: range.start,
        end: range.end.min(seam),
    };
    let nfs = HeightRange {
        start: range.start.max(seam),
        end: range.end,
    };
    (non_empty(fs), non_empty(nfs))
}

/// `None` for an empty half-open range.
fn non_empty(range: HeightRange) -> Option<HeightRange> {
    (range.start < range.end).then_some(range)
}

impl<F: Clone, N: Clone, Src: Clone, R> Clone for ComposedSnapshot<F, N, Src, R> {
    fn clone(&self) -> Self {
        Self {
            local: self.local.clone(),
            remote: self.remote.clone(),
            routing: PhantomData,
        }
    }
}

// --- coherence marker + served range -----------------------------------------
//
// Delegated to the composed view: the pin, the coverage, and the finalised/tip
// boundary are exactly what the composer already computes across the seam.

impl<F, N, Src, R> ChainSegment for ComposedSnapshot<F, N, Src, R>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    fn pinned_tip(&self) -> Option<BlockId> {
        self.local.pinned_tip()
    }

    fn coverage(&self) -> Option<HeightRange> {
        self.local.coverage()
    }
}

impl<F, N, Src, R> Snapshot for ComposedSnapshot<F, N, Src, R>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    fn serviceable_range(&self) -> ServiceableRange {
        self.local.serviceable_range()
    }
}

// --- reads whose placement never varies --------------------------------------

/// Always local: composing compact blocks across the seam is what the two
/// tiers exist for.
impl<F, N, Src, R> CompactBlockRead for ComposedSnapshot<F, N, Src, R>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    async fn compact_block(&self, at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
        self.local.compact_block(at).await
    }
    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        self.local.stream_compact(range)
    }
}

/// Always local: a projection of the compact block already served, reduced to
/// its spend markers. Not a separate index.
impl<F, N, Src, R> CompactNullifierRead for ComposedSnapshot<F, N, Src, R>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    async fn compact_block_nullifiers(
        &self,
        at: BlockRef,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        Ok(self
            .local
            .compact_block(at)
            .await?
            .map(crate::nullifiers::strip_to_nullifiers))
    }
}

/// Always local: the aggregate is read off the composed pinned tip.
impl<F, N, Src, R> ChainInfoRead for ComposedSnapshot<F, N, Src, R>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    async fn chain_info(&self) -> Result<ChainInfo, ReadError> {
        let tip = self.local.pinned_tip();
        let estimated_height = tip.map(|id| id.height).unwrap_or(Height::GENESIS);
        Ok(ChainInfo {
            tip,
            estimated_height,
        })
    }
}

/// Always remote: no local index holds transaction bytes, and the wallet parses
/// them itself, so the validator's `getrawtransaction` answer relays as is.
impl<F, N, Src, R> RawTransactionRead for ComposedSnapshot<F, N, Src, R>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: GetTransaction + Send + Sync + 'static,
    R: Routing,
{
    async fn raw_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        self.remote.raw_transaction(id).await
    }
}
