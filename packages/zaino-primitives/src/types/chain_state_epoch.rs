//! A stable identifier for a published view of the chain.

use crate::types::BlockRef;

/// Names *which* chain state a published view represents.
///
/// A publisher hands out immutable snapshots; a consumer that holds one and
/// wants to know whether some other component's data was derived against the
/// same chain compares epochs. Equality means the two describe one chain state,
/// so data tagged with one is coherent with a view tagged with the other.
///
/// # Why the generation, and why it moves when it does
///
/// `generation` advances when the publisher's best tip *changes*, not on every
/// publication. A publisher republishes on its own cadence — trimming blocks
/// that have passed below its window, folding in a no-op reconcile — and
/// bumping the generation on those would churn the epoch every cycle and defeat
/// the comparison it exists to serve. Keyed to tip changes, a stable tip gives a
/// stable epoch while successive tips stay distinguishable.
///
/// `best_tip` rides along so the epoch is self-describing: a consumer can tell
/// not just that the chain moved but where it moved to. Carrying the pair also
/// makes the comparison stronger than either half alone — two publications can
/// share a tip and differ in content, and the generation separates them, while
/// a same-height reorg changes the hash and not necessarily the count.
///
/// # Why this is a primitive
///
/// Two subsystems need this vocabulary and neither may depend on the other: the
/// chain head publishes epochs, and the mempool's coherence layer freezes and
/// thaws against them. Defining it in either crate would either couple them or
/// duplicate the type — and a duplicate that starts field-identical is a
/// duplicate that drifts. It lives here, where both already look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChainStateEpoch {
    /// Advances on each change of [`Self::best_tip`].
    pub generation: u64,
    /// The canonical tip this epoch describes.
    pub best_tip: BlockRef,
}
