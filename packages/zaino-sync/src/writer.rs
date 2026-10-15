//! What every index writer does to its store, around its own fold (`usage.md`)
//!
//! - commits: the store's own on a full buffer, the writer's [`commit`] after a finalized run and at
//!   `Shutdown`
//! - [`IndexPublisher`] = the writer's end of its [`IndexHandle`]

use std::time::{Duration, Instant};

use tokio::sync::watch;
use zaino_persistence::{BlockChanges, Store, View};
use zaino_primitives::types::{Block, BlockRef};
use zcash_protocol::consensus::BlockHeight;

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
pub fn held<S: Store>(store: &S, height: BlockHeight) -> bool {
    Some(height) <= store.applied().tip().map(|tip| tip.height)
}

/// `changes` into `store`, counted (`zaino_index_applied_{blocks,rows}_total`); time it took (an
/// auto-commit included)
pub fn apply<S: Store>(store: &mut S, changes: BlockChanges) -> Duration {
    emit::applied_block(store.schema().kind.name(), changes.rows());
    let started = Instant::now();
    store.apply(changes);
    started.elapsed()
}

/// Everything buffered → disk now (nothing buffered = nothing written); time it took
///
/// - failure = panic naming the index and its directory (store poisoned)
pub fn commit<S: Store>(store: &mut S) -> Duration {
    let started = Instant::now();
    if let Err(failed) = store.commit() {
        failed.crash();
    }
    started.elapsed()
}

/// One run done: `started` (taken off the queue) → now, `write` of it in [`apply`] + [`commit`]
/// (`zaino_index_{run,write}_seconds`)
pub fn ran<S: Store>(store: &S, started: Instant, write: Duration) {
    emit::index_run(store.schema().kind.name(), started.elapsed(), write);
}

/// Writer's end of its [`IndexHandle`]: applied tip after every hop, durable view on each commit
pub struct IndexPublisher<V> {
    durable: watch::Sender<V>,
    applied: watch::Sender<Option<BlockRef>>,
}

impl<V: View> IndexPublisher<V> {
    pub fn new<S: Store<View = V>>(store: &S) -> Self {
        let publisher = Self {
            durable: watch::Sender::new(store.durable()),
            applied: watch::Sender::new(store.durable().tip()),
        };
        publisher.publish(store);
        publisher
    }

    pub fn handle(&self) -> IndexHandle<V> {
        IndexHandle::new(self.durable.subscribe(), self.applied.subscribe())
    }

    /// `store`'s applied tip out, its durable view too once its tip moved (+ both as metrics)
    pub fn publish<S: Store<View = V>>(&self, store: &S) {
        let (applied, durable) = (store.applied().tip(), store.durable().tip());
        let height = |tip: Option<BlockRef>| tip.map(|tip| tip.height);
        emit::index_tips(store.schema().kind.name(), height(applied), height(durable));
        self.applied.send_replace(applied);
        if self.durable.borrow().tip() != durable {
            self.durable.send_replace(store.durable());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use zaino_persistence::{fs::SimFs, IndexKind, Schema, Tables};
    use zaino_primitives::testing::{h, MockChain};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::testing::open;

    const SCHEMA: Schema =
        Schema::new(IndexKind::BlockHash, 1, NetworkType::Regtest, Tables::new(&[], &[]));

    #[test]
    #[should_panic(expected = "block_hash: changes opened for another block")]
    fn changes_opened_for_another_block_panic() {
        let mut chain = MockChain::regtest();
        let one = chain.mine_empty(1);
        assert_next(&SCHEMA.changes(chain.genesis()), Some(chain.genesis()), chain.block(one.hash));
    }

    #[test]
    #[should_panic(expected = "does not extend the parent tip")]
    fn a_run_past_a_gap_panics() {
        let mut chain = MockChain::regtest();
        let two = chain.mine_empty(2);
        let run = [&**chain.block(chain.genesis().hash), &**chain.block(two.hash)];
        assert_run(None, &run, &run.map(|block| SCHEMA.changes(block.at())));
    }

    #[test]
    #[should_panic(expected = "does not extend the parent tip")]
    fn a_sibling_at_the_tips_height_panics() {
        let mut chain = MockChain::regtest();
        let two = chain.mine_empty(2);
        let sibling = chain.fork(h(1)).mine_empty(1).tip();
        assert_next(&SCHEMA.changes(sibling), Some(two), chain.block(sibling.hash));
    }

    #[test]
    #[should_panic(expected = "does not extend the parent tip")]
    fn a_block_below_the_tip_panics() {
        let mut chain = MockChain::regtest();
        let three = chain.mine_empty(3);
        let two = chain.at(h(2));
        assert_next(&SCHEMA.changes(two), Some(three), chain.block(two.hash));
    }

    /// Commit I/O failure = panic naming the index and its directory (never an `Err` to drain)
    #[test]
    #[should_panic(
        expected = "block_hash index commit failed at /block_hash: index io: sim: injected EIO"
    )]
    fn a_failed_commit_panics_naming_the_index_and_its_directory() {
        let fs = SimFs::new();
        let mut store = open(&fs, &SCHEMA, NonZeroUsize::MAX);
        apply(&mut store, SCHEMA.changes(MockChain::regtest().genesis()));
        fs.fail_from(fs.mutations());
        commit(&mut store);
    }
}
