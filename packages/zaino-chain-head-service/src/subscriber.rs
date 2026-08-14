//! The read-only handle onto a running ChainHead.

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::{broadcast, watch};
use zaino_chain_head::{ChainHeadBlock, ChainHeadBlockService, ChainHeadFreezeEvents};
use zaino_primitives::types::ChainStateEpoch;
use zaino_status::{NamedAtomicStatus, Status, StatusType};

use crate::snapshot::MapBackedSnapshot;

/// A cheap-to-clone reader for a running ChainHead.
///
/// Holds no ability to drive or stop synchronisation — that stays on
/// `ChainHeadService`. Handing this out rather than the service is what makes
/// "consumers cannot sequence ChainHead against something else" a property of
/// the types rather than a convention.
///
/// It answers no questions about the chain either. Those belong to the
/// snapshot: take one with [`current`](ChainHeadBlockService::current) and ask
/// it, which is also what gives several answers from a single coherent view.
#[derive(Clone)]
pub struct ChainHeadSubscriber {
    /// The *cell* the runtime publishes into, not a snapshot taken from it.
    ///
    /// Holding a snapshot here instead would freeze the handle at the view it
    /// was created with, and a consumer that keeps one subscriber for the
    /// process lifetime would never see the chain move again.
    current: Arc<ArcSwap<MapBackedSnapshot>>,
    updates: watch::Receiver<ChainStateEpoch>,
    frozen: broadcast::Sender<ChainHeadBlock>,
    /// A clone of the runtime's own cell, not a copy of its value.
    ///
    /// Reading how the runtime is faring is not driving it: a status read
    /// cannot advance the graph, stop the writer task, or sequence ChainHead
    /// against anything. What it does buy is the ability for a consumer holding
    /// only this handle to say whether the tip it is being served is fresh —
    /// a snapshot looks identical whether the writer is keeping up or has given
    /// up, so without this the answer is unobtainable from the read side.
    status: NamedAtomicStatus,
}

impl std::fmt::Debug for ChainHeadSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainHeadSubscriber")
            .field("epoch", &*self.updates.borrow())
            .field("status", &self.status.load())
            .finish_non_exhaustive()
    }
}

impl ChainHeadSubscriber {
    pub(crate) fn new(
        current: Arc<ArcSwap<MapBackedSnapshot>>,
        updates: watch::Receiver<ChainStateEpoch>,
        frozen: broadcast::Sender<ChainHeadBlock>,
        status: NamedAtomicStatus,
    ) -> Self {
        Self {
            current,
            updates,
            frozen,
            status,
        }
    }

    /// The epoch of the most recently published snapshot.
    pub fn epoch(&self) -> ChainStateEpoch {
        *self.updates.borrow()
    }
}

impl Status for ChainHeadSubscriber {
    /// The status of the runtime this handle reads from.
    ///
    /// The same value `ChainHeadService::status` returns — both read one cell,
    /// so a transition cannot be visible on one handle and not the other.
    fn status(&self) -> StatusType {
        self.status.load()
    }
}

impl ChainHeadBlockService for ChainHeadSubscriber {
    type Snapshot = MapBackedSnapshot;

    fn current(&self) -> Arc<Self::Snapshot> {
        self.current.load_full()
    }

    fn subscribe_updates(&self) -> watch::Receiver<ChainStateEpoch> {
        self.updates.clone()
    }
}

impl ChainHeadFreezeEvents for ChainHeadSubscriber {
    /// Subscribing is what makes the runtime start emitting.
    ///
    /// With no receivers the runtime skips the work of collecting frozen blocks
    /// entirely, so a consumer running in Independent Mode pays nothing for a
    /// capability it never uses.
    fn subscribe_frozen(&self) -> broadcast::Receiver<ChainHeadBlock> {
        self.frozen.subscribe()
    }
}
