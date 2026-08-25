//! What ChainHead asks of a validator, and how a consumer reaches its
//! published views.
//!
//! Two ports. [`ChainHeadBlockSource`] is what ChainHead needs from a
//! validator; [`ChainHeadBlockService`] is the handle a consumer holds onto a
//! running ChainHead.
//!
//! The handle is deliberately thin. Everything answerable *about the chain*
//! lives on [`ChainHeadSnapshot`](crate::snapshot::ChainHeadSnapshot), because
//! that is what those questions are about; this port only produces snapshots
//! and reports when a new one exists. Restating each query here as a method
//! taking a snapshot would define every capability twice, and would make a
//! consumer that already holds a snapshot keep a service handle it does not
//! need.
//!
//! There is no lifecycle port either. Starting and stopping belong to the
//! runtime, not the domain: they are inherent methods on the concrete service,
//! and their absence here is what stops a read handle from shutting ChainHead
//! down. Status is a runtime property too — reported through
//! `zaino_status::Status` like every other Zaino subsystem — but it is readable
//! from *both* concrete handles, because observing how a runtime is faring is
//! not the same as sequencing it. A consumer holding only a read handle still
//! has to be able to say whether the tip it is being served is fresh.
//!
//! Nor is there a `sync`, `sync_to_height`, `reconcile` or `reconcile_once`.
//! ChainHead synchronises itself; a public method to drive it would let a
//! consumer sequence it against something else, which is exactly the coupling
//! this crate exists to remove.

use std::sync::Arc;

use tokio::sync::{broadcast, watch};

use zaino_primitives::types::ChainStateEpoch;

use crate::{block::ChainHeadBlock, snapshot::ChainHeadSnapshot};

/// Every question ChainHead asks a validator.
///
/// A bound over [`zaino_source`] ports rather than a vocabulary of its own:
/// each of these questions already exists there, and restating them would mean
/// an adapter that can already answer them still has to be taught to say so.
/// Nothing implements this directly — the blanket impl below applies it to any
/// type answering all of them, so production composites and test mocks earn the
/// bound the same way.
///
/// Five questions, and deliberately no `GetChainTips`. ChainHead learns of a
/// competing branch only by living through the reorg that created it — walking
/// back by hash from a block whose parent it does not hold — so it never asks a
/// validator to enumerate tips. A bound naming a question nothing asks would
/// oblige every source to answer it for nothing.
///
/// [`SubscribeBlocks`](zaino_source::SubscribeBlocks) is a latency hint only.
/// Its channel carries `()`, and ChainHead re-reads the source on every wake,
/// so a source offering no push path is served correctly by the poll interval
/// alone.
///
/// Deliberately not `Clone`: a source may own connections and a database handle
/// that must not be duplicated. The runtime shares one behind an `Arc` instead,
/// which is why `zaino_source_zebra::ZebraValidator` — which is not `Clone` —
/// satisfies this bound directly rather than through a wrapper.
pub trait ChainHeadBlockSource:
    zaino_source::GetChainTip
    + zaino_source::GetBlock
    + zaino_source::GetBlockByHash
    + zaino_source::GetCommitmentTreeRoots
    + zaino_source::GetCommitmentTreeRootsByHeight
    + zaino_source::SubscribeBlocks
    + Send
    + Sync
    + 'static
{
}

impl<T> ChainHeadBlockSource for T where
    T: zaino_source::GetChainTip
        + zaino_source::GetBlock
        + zaino_source::GetBlockByHash
        + zaino_source::GetCommitmentTreeRoots
        + zaino_source::GetCommitmentTreeRootsByHeight
        + zaino_source::SubscribeBlocks
        + Send
        + Sync
        + 'static
{
}

/// A handle onto a running ChainHead.
///
/// Produces snapshots and reports when the chain state changes. Everything
/// answerable about the chain is asked of the snapshot itself.
///
/// The associated [`Snapshot`](Self::Snapshot) type is what keeps the graph's
/// representation out of this port: a runtime storing its graph in persistent
/// structures rather than hash maps satisfies the same interface, and no
/// consumer changes.
///
/// Both methods are total. A ChainHead completes its initialisation before its
/// constructor returns, so there is no state in which one exists but has
/// nothing to answer with, and no "not ready yet" case for a consumer to
/// handle.
pub trait ChainHeadBlockService: Clone + Send + Sync + 'static {
    /// The view this handle produces.
    type Snapshot: ChainHeadSnapshot;

    /// The most recently published snapshot.
    ///
    /// A caller wanting several answers from one coherent view captures this
    /// once and queries the result, rather than calling it repeatedly.
    fn current(&self) -> Arc<Self::Snapshot>;

    /// Notifications of chain-state changes.
    ///
    /// The epoch's generation advances when the canonical tip changes, not on
    /// every republication, so a consumer pinned to an epoch is told the chain
    /// moved only when it actually did.
    fn subscribe_updates(&self) -> watch::Receiver<ChainStateEpoch>;
}

/// Blocks the chain head has finalised, for a store to ingest.
///
/// Optional and separate from [`ChainHeadBlockService`] so a consumer bounds on
/// it only when it wants the handoff. A chain head running in Independent Mode
/// — where the store builds from the source itself — never touches this.
///
/// A block is emitted once it falls below the consensus seam, past which no
/// reorg can reach it. The whole block travels, parsed and with its commitment
/// tree roots, because the point of the handoff is that the store does not
/// fetch it again.
///
/// **The stream is best-effort.** It is a `broadcast`, so a consumer that falls
/// behind receives `RecvError::Lagged(n)` and learns exactly how many it
/// missed; a chain head re-anchoring after a long outage moves its floor
/// discontinuously and simply never emits the blocks it skipped. Neither is an
/// error. The store's own build from source is the authority, and this only
/// spares it the fetch in steady state.
pub trait ChainHeadFreezeEvents: Clone + Send + Sync + 'static {
    /// Subscribe to blocks as they pass below the seam.
    fn subscribe_frozen(&self) -> broadcast::Receiver<ChainHeadBlock>;
}

#[cfg(test)]
mod tests {
    use super::ChainHeadBlockSource;

    /// The production composite must satisfy the driven port. A compile-time
    /// check: if a question is added to ChainHead's requirements that
    /// `ZebraValidator` cannot answer, this stops building.
    #[test]
    fn zebra_validator_satisfies_the_bound() {
        fn assert_satisfied<T: ChainHeadBlockSource>() {}
        assert_satisfied::<zaino_source_zebra::ZebraValidator>();
    }
}
