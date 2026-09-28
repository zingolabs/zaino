//! Aggregate obligations. Fine traits are for consumers/mocks; these name the
//! whole-engine handle. Per-use-case bundles live in [`crate::use_cases`].

use zaino_primitives::types::{BlockRef, Height, HeightRange};

use crate::controls::{
    Broadcast, MempoolSubscribe, ReportedUpgrades, Serviceable, TakeSnapshot, TipSubscribe,
};

/// One side's contribution to a composed chain view: the height span it covers
/// and the tip it is pinned to.
///
/// Both the finalised store and the non-finalised head provide this — the same
/// neutral shape. Which side is the durable prefix and which the volatile suffix
/// is the *composer's* to assign, by the slot it places each in, not a fact a
/// segment carries about itself. That is what lets one composer route over two
/// sides without either having to describe its own durability.
///
/// A *coherence* marker only — it carries no reads. The compact blocks a segment
/// serves are named separately by [`CompactBlockRead`](crate::CompactBlockRead)
/// and composed over one shared pin, so a consumer composes exactly the reads it
/// needs rather than inheriting the union of every read.
pub trait ChainSegment: Clone + Send + Sync + 'static {
    /// The tip this segment is pinned to — its coherence coordinate — or `None`
    /// when it holds nothing.
    fn pinned_tip(&self) -> Option<BlockRef>;

    /// The inclusive height span this segment can serve, or `None` when it holds
    /// nothing.
    ///
    /// Neutral by design: a finalised store reports `[genesis, watermark]`, a
    /// non-finalised window `[floor, tip]`. The composer reads the boundary it
    /// needs from each — the FS's high is the seam watermark, the NFS's low and
    /// high are the volatile window — without either side describing what its
    /// bounds *mean*.
    fn coverage(&self) -> Option<HeightRange>;
}

/// The seam within a served view: the finalised store's watermark and the
/// served tip above it.
///
/// Two different kinds of height, deliberately not both called a tip. The
/// watermark is a *level* on the store — the highest height every index has
/// committed — and may be absent when the store holds nothing. The tip is a
/// *position* on the chain: the block the view is pinned to.
///
/// ```text
/// finalised    = [genesis, watermark]        answered by the finalised tier
/// window       = (watermark, tip]            answered by the non-finalised head
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceableRange {
    /// The finalised store's watermark, or `None` when it holds nothing; then
    /// the whole served span is the non-finalised window.
    pub watermark: Option<Height>,
    /// The served tip: the height this view is pinned to.
    pub tip: Height,
}

/// A pinned, reorg-coherent *served* view (ADR-0003): a cheap-to-clone handle to
/// the chain as of the tip it was pinned to, coherent for as long as any clone
/// lives.
///
/// The served framing on top of a [`ChainSegment`]: it adds the
/// finalised/non-finalised boundary a downstream client sees. A composed view
/// implements this (its `serviceable_range` reports the store's watermark and
/// the head's tip); a bare input segment reports only neutral
/// [`coverage`](ChainSegment::coverage).
pub trait Snapshot: ChainSegment {
    /// The seam within this view, or `None` when it holds nothing — the same
    /// condition under which [`coverage`](ChainSegment::coverage) is `None`.
    fn serviceable_range(&self) -> Option<ServiceableRange>;
}

/// The full inner driving surface — what `zaino-runtime` implements and what a
/// mock stands in for when testing outer clients. The runtime satisfies every
/// capability; outer clients depend on the narrower use-case services instead
/// (see [`crate::use_cases`]).
pub trait IndexerService:
    TakeSnapshot<Snapshot: Snapshot>
    + TipSubscribe
    + MempoolSubscribe
    + Broadcast
    + Serviceable
    + ReportedUpgrades
    + Send
    + Sync
    + 'static
{
}
