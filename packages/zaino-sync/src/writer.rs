//! What every index writer does to its store, around its own fold (`data-sink.md`)
//!
//! - commits: the store's own on a full buffer, the writer's [`commit`] after a finalized run and at
//!   `Shutdown`
//! - [`IndexPublisher`] = the writer's end of its [`IndexHandle`]

use tokio::sync::watch;
use zaino_persistence::{BlockChanges, Store, View};
use zaino_primitives::types::{Block, BlockRef, Height};

use crate::{emit, IndexHandle};

/// Fold's preconditions, panic naming the index: `out` opened for `block`, `block` next above
/// `parent` (`None` = empty parent: genesis)
pub fn assert_next(out: &BlockChanges, parent: Option<BlockRef>, block: &Block) {
    let (name, at, opened) = (out.schema().kind.name(), block.at(), out.block());
    assert!(opened == at, "{name}: changes opened for another block ({opened:?}), folding {at:?}");
    let extends = block.header().extends(parent);
    assert!(extends, "{name}: block {at:?} does not extend the parent tip {parent:?}");
}

/// [`assert_next`] over a run: `out[i]` for `blocks[i]`, each block next above the one before it
/// (the first above `parent`)
pub fn assert_run(parent: Option<BlockRef>, blocks: &[&Block], out: &[BlockChanges]) {
    assert_eq!(blocks.len(), out.len(), "one delta per block of the run");
    let mut below = parent;
    for (block, out) in blocks.iter().zip(out) {
        assert_next(out, below, block);
        below = Some(block.at());
    }
}

/// `height` at or below `store`'s staged tip (a restart resends from the lowest durable tip)
pub fn held<S: Store>(store: &S, height: Height) -> bool {
    Some(height) <= store.staged().tip().map(|tip| tip.height)
}

/// `changes` into `store`, counted (`zaino_index_applied_{blocks,rows}_total`)
pub fn apply<S: Store>(store: &mut S, changes: BlockChanges) {
    emit::applied_block(store.schema().kind.name(), changes.rows());
    store.apply(changes);
}

/// Everything buffered → disk now (nothing buffered = nothing written)
///
/// - failure = panic naming the index and its directory (store poisoned)
pub fn commit<S: Store>(store: &mut S) {
    if let Err(error) = store.commit() {
        error.commit_failed(store.schema().kind.name(), store.path());
    }
}

/// Writer's end of its [`IndexHandle`]: applied tip after every hop, committed view on each commit
pub struct IndexPublisher<V> {
    committed: watch::Sender<V>,
    applied: watch::Sender<Option<BlockRef>>,
}

impl<V: View> IndexPublisher<V> {
    pub fn new<S: Store<View = V>>(store: &S) -> Self {
        let publisher = Self {
            committed: watch::Sender::new(store.committed()),
            applied: watch::Sender::new(store.committed().tip()),
        };
        publisher.publish(store);
        publisher
    }

    pub fn handle(&self) -> IndexHandle<V> {
        IndexHandle::new(self.committed.subscribe(), self.applied.subscribe())
    }

    /// `store`'s applied tip out, its committed view too once its tip moved (+ both as metrics)
    pub fn publish<S: Store<View = V>>(&self, store: &S) {
        let (applied, durable) = (store.staged().tip(), store.committed().tip());
        let height = |tip: Option<BlockRef>| tip.map(|tip| tip.height);
        emit::index_tips(store.schema().kind.name(), height(applied), height(durable));
        self.applied.send_replace(applied);
        if self.committed.borrow().tip() != durable {
            self.committed.send_replace(store.committed());
        }
    }
}
