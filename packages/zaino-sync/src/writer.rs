//! What every index writer does to its store, around its own fold (`usage.md`)
//!
//! - commits: the store's own on a full buffer, the writer's [`commit`] after a finalized run and at
//!   `Shutdown`
//! - [`IndexPublisher`] = the writer's end of its [`IndexHandle`]
//! - public precondition checks (panic naming the index): [`assert_next`], [`assert_dense`],
//!   [`assert_run`]

use std::time::{Duration, Instant};

use tokio::sync::watch;
use zaino_persistence::{BlockChanges, SequenceRead, SequenceTable, Store, View};
use zaino_primitives::types::{Block, BlockRef};

use crate::{metrics, IndexHandle};

/// Fold's preconditions, panic naming the index: `out` opened for `block`, `block` next above
/// `parent` (`None` = empty parent: genesis)
pub fn assert_next(out: &BlockChanges, parent: Option<BlockRef>, block: &Block) {
    let (name, at, opened) = (out.schema().kind.name(), block.block_ref(), out.block());
    assert!(opened == at, "{name}: changes opened for another block ({opened:?}), folding {at:?}");
    let extends = block.header().extends(parent);
    assert!(extends, "{name}: block {at:?} does not extend the parent tip {parent:?}");
}

/// Writer's open precondition, panic naming the index: `table` holds one record per durable height
pub fn assert_dense<V: View + SequenceRead>(view: &V, table: SequenceTable) {
    let name = view.schema().kind.name();
    let held = view.tip().map_or(0, |tip| u64::from(tip.height) + 1);
    assert_eq!(view.sequence(table).len(), held, "{name}: one record per durable height");
}

/// [`assert_next`] over a run: `out[i]` for `blocks[i]`, each block next above the one before it
/// (the first above `parent`)
pub fn assert_run(parent: Option<BlockRef>, blocks: &[&Block], out: &[BlockChanges]) {
    assert_eq!(blocks.len(), out.len(), "one delta per block of the run");
    let mut below = parent;
    for (block, out) in blocks.iter().zip(out) {
        assert_next(out, below, block);
        below = Some(block.block_ref());
    }
}

/// `block` at or below `store`'s applied tip (a restart resends from the lowest durable tip)
pub fn held<S: Store>(store: &S, block: &Block) -> bool {
    Some(block.header().height) <= store.applied().tip().map(|tip| tip.height)
}

/// `changes` into `store`, counted (`zaino_index_applied_{blocks,rows}_total`) → time it took
/// (auto-commit included)
pub fn apply<S: Store>(store: &mut S, changes: BlockChanges) -> Duration {
    metrics::applied_block(store.schema().kind.name(), changes.rows());
    let started = Instant::now();
    store.apply(changes);
    started.elapsed()
}

/// Everything buffered → disk now (nothing buffered = nothing written) → time it took
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
    metrics::index_run(store.schema().kind.name(), started.elapsed(), write);
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
        metrics::index_tips(store.schema().kind.name(), height(applied), height(durable));
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
    use zaino_source::testing::{h, MockChain};
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

    /// Each block checked against the one before it (gap / sibling / below = `Header::extends`
    /// table, zaino-primitives)
    #[test]
    fn a_run_of_linked_blocks_from_genesis_passes() {
        let mut chain = MockChain::regtest();
        let two = chain.mine_empty(2);
        let blocks = chain.blocks(two);
        let run: Vec<&Block> = blocks.iter().map(|block| &**block).collect();
        let out: Vec<BlockChanges> = run.iter().map(|b| SCHEMA.changes(b.block_ref())).collect();
        assert_run(None, &run, &out);
    }

    /// B2 at the height above A1, its parent B1 ≠ A1
    #[test]
    #[should_panic(expected = "does not extend the parent tip")]
    fn a_block_on_another_parent_panics() {
        let mut chain = MockChain::regtest();
        let a1 = chain.mine_empty(1);
        let b2 = chain.fork(h(0)).mine_empty(2).tip();
        assert_next(&SCHEMA.changes(b2), Some(a1), chain.block(b2.hash));
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
