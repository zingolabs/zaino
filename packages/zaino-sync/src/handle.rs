//! [`IndexHandle`]: one index's committed view, as the rest of the daemon reads it (`data-sink.md`)

use tokio::sync::watch;
use zaino_persistence::View;
use zaino_primitives::types::BlockRef;

/// From its writer (`XIndexWriter::handle`); cheap clone
#[derive(Clone)]
pub struct IndexHandle<V> {
    committed: watch::Receiver<V>,
}

impl<V: View> IndexHandle<V> {
    pub(crate) fn new(committed: watch::Receiver<V>) -> Self {
        Self { committed }
    }

    /// Committed view (what snapshots and folds read)
    pub fn view(&self) -> V {
        self.committed.borrow().clone()
    }

    /// Durable tip
    pub fn tip(&self) -> Option<BlockRef> {
        self.committed.borrow().tip()
    }

    /// Next commit; `false` = its writer gone
    pub async fn changed(&mut self) -> bool {
        self.committed.changed().await.is_ok()
    }
}
