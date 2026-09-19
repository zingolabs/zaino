//! Aggregate obligations. Fine traits are for consumers/mocks; these name the
//! whole-engine handle. Per-use-case bundles live in [`crate::profiles`].

use zaino_core::{BlockId, ServiceableRange};

use crate::controls::{
    Broadcast, MempoolSubscribe, ReportedUpgrades, Serviceable, TakeSnapshot, TipSubscribe,
};

/// A pinned, reorg-coherent view (ADR-0003): a cheap-to-clone handle to the
/// chain as of the tip it was pinned to, coherent for as long as any clone
/// lives.
///
/// This marker is the *coherence* capability only — it carries no reads. Which
/// reads a view offers is named separately (the read-set bundles in
/// [`crate::profiles`]), so a consumer composes exactly the reads it needs over
/// one shared pin rather than inheriting the union of every read.
pub trait Snapshot: Clone + Send + Sync + 'static {
    /// The tip this view is pinned to — its coherence coordinate — or `None`
    /// when the chain has no tip yet. Readable without any read capability, so a
    /// consumer can reason about the pin without depending on `BlockRead`.
    fn pinned_tip(&self) -> Option<BlockId>;

    fn serviceable_range(&self) -> ServiceableRange;
}

/// The full inner driving surface — what `zaino-runtime` implements and what a
/// mock stands in for when testing outer clients. The runtime satisfies every
/// capability; outer clients depend on the narrower profile bundles instead
/// (see [`crate::profiles`]).
pub trait IndexerService:
    TakeSnapshot
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
