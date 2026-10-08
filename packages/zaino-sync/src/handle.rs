//! [`IndexHandle`]: one index's committed view + whether it serves at the tip (`data-sink.md`)

use tokio::sync::watch;
use zaino_persistence::View;
use zaino_primitives::types::{BlockRef, Height};

/// From its writer (`XIndexWriter::handle`); cheap clone
///
/// - `requires` = indexes it folds on (compact-block: value-balance's fees): it serves only with them
#[derive(Clone)]
pub struct IndexHandle<V> {
    committed: watch::Receiver<V>,
    requires: Vec<IndexHandle<V>>,
}

impl<V: View> IndexHandle<V> {
    pub(crate) fn new(committed: watch::Receiver<V>) -> Self {
        Self { committed, requires: Vec::new() }
    }

    /// Serves only while `other` does too
    pub fn requiring(mut self, other: Self) -> Self {
        self.requires.push(other);
        self
    }

    /// Committed view (what snapshots read)
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

    /// Durable tip within `window` blocks of `best` (and every index it requires too)
    pub fn serving(&self, best: Height, window: u32) -> bool {
        let next = self.tip().map_or(0, |tip| u32::from(tip.height) + 1);
        let behind = (u32::from(best) + 1).saturating_sub(next);
        behind <= window && self.requires.iter().all(|other| other.serving(best, window))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence::{
        fs::SimFs, DiskEngine, IndexKind, PersistenceEngine, Schema, SequenceTable, Store, Tables,
        Width,
    };
    use zaino_primitives::testing::{h, MockChain};
    use zcash_protocol::consensus::NetworkType;

    const ROWS: SequenceTable = SequenceTable::new(0, "rows", Width::fixed(4));
    const SCHEMA: Schema =
        Schema::new(IndexKind::BlockHash, 1, NetworkType::Regtest, Tables::new(&[ROWS], &[]));

    /// Window 2: serving while ≤ 2 blocks behind best; fresh = behind by best + 1; a required
    /// index lagging holds it back
    #[test]
    fn an_index_serves_within_the_window_of_best_and_only_with_what_it_requires() {
        let chain = MockChain::regtest();
        let engine = DiskEngine::new(SimFs::new());
        let mut store = engine.open(std::path::Path::new("/a"), &SCHEMA).expect("open");
        let fresh = store.view();
        let mut changes = store.changes(chain.genesis());
        changes.sequence(ROWS).append(&[0; 4]);
        store.apply(changes);
        store.commit().expect("commit");
        let (_tx, rx) = watch::channel(store.view());
        let (_fresh_tx, fresh_rx) = watch::channel(fresh);
        let at_genesis = IndexHandle::new(rx);
        let empty = IndexHandle::new(fresh_rx);

        let serving = |handle: &IndexHandle<_>| [0, 1, 2, 3].map(|best| handle.serving(h(best), 2));
        assert_eq!(serving(&at_genesis), [true, true, true, false]);
        assert_eq!(serving(&empty), [true, true, false, false]);
        assert_eq!(serving(&at_genesis.clone().requiring(empty)), [true, true, false, false]);
    }
}
