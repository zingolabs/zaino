//! [`IndexHandle`]: one index's durable view + applied height, as the rest of the daemon reads them
//! (`usage.md`)

use tokio::sync::watch;
use zaino_persistence::View;
use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::BlockHeight;

/// From its writer (`XIndexWriter::handle`); cheap clone
#[derive(Clone)]
pub struct IndexHandle<V> {
    durable: watch::Receiver<V>,
    applied: watch::Receiver<Option<BlockRef>>,
}

impl<V: View> IndexHandle<V> {
    pub(crate) fn new(
        durable: watch::Receiver<V>,
        applied: watch::Receiver<Option<BlockRef>>,
    ) -> Self {
        Self { durable, applied }
    }

    /// Last block folded + applied to the store (durable or not)
    pub fn applied_height(&self) -> Option<BlockHeight> {
        self.applied.borrow().map(|tip| tip.height)
    }

    /// Durable view (what NFS publications and folds read)
    pub fn durable_view(&self) -> V {
        self.durable.borrow().clone()
    }

    pub fn durable_height(&self) -> Option<BlockHeight> {
        self.durable.borrow().tip().map(|tip| tip.height)
    }

    /// Next durable view; `false` = its writer gone
    pub async fn changed(&mut self) -> bool {
        self.durable.changed().await.is_ok()
    }
}
