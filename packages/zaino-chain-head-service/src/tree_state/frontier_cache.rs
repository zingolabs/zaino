//! Per-block commitment frontiers for the window, kept across snapshots.
//!
//! The frontier after a block is a function of that block and its ancestors
//! alone. It does not depend on where the watermark sits, which snapshot asked,
//! or which branch is currently best, so it is keyed by block hash and stays
//! correct for as long as it is kept:
//!
//! ```text
//! after(b)            = extract(b, after(parent(b)))     after(seam − 1) = seed
//! completions(b, p)   = subtrees of p that b completes on top of after(parent(b))
//! ```
//!
//! A snapshot published for the next block finds every earlier block already
//! folded, so a treestate read near the tip extends the cached run by the few
//! blocks it lacks instead of replaying the whole window. A block displaced by a
//! reorg keeps its entry; it is simply never asked for again, and is dropped once
//! the watermark passes its height.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use zaino_indexes::indexes::tree_state::serve::WindowSubtree;
use zaino_indexes::indexes::tree_state::TreeStateValue;
use zaino_primitives::types::{BlockHash, Height, ShieldedPool};

/// The cached fold results of one window block.
struct Entry {
    /// The block's height, for pruning once the watermark passes it.
    height: Height,
    /// The per-pool frontier after the block.
    after: Arc<TreeStateValue>,
    /// The subtrees the block completes, per pool, once a read has asked.
    completions: HashMap<ShieldedPool, Arc<[WindowSubtree]>>,
}

/// Fold results for window blocks, shared by every snapshot of one chain head.
///
/// Reads and inserts hold the map lock only for the lookup or insert itself;
/// folding happens outside it. [`fill`](Self::fill) is a separate async lock a
/// read holds while it folds, so concurrent reads that need the same blocks wait
/// for one fold instead of each repeating it.
#[derive(Default)]
pub(crate) struct WindowFrontiers {
    entries: Mutex<HashMap<BlockHash, Entry>>,
    fill: tokio::sync::Mutex<()>,
}

impl WindowFrontiers {
    /// Serialise folding: held by the one read currently extending the cache.
    pub(crate) async fn fill_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.fill.lock().await
    }

    /// The frontier after `block`, if folded.
    pub(crate) fn after(&self, block: &BlockHash) -> Option<Arc<TreeStateValue>> {
        self.entries()
            .get(block)
            .map(|entry| Arc::clone(&entry.after))
    }

    /// Record the frontier after `block` at `height`.
    pub(crate) fn insert_after(
        &self,
        block: BlockHash,
        height: Height,
        after: Arc<TreeStateValue>,
    ) {
        self.entries().entry(block).or_insert_with(|| Entry {
            height,
            after,
            completions: HashMap::new(),
        });
    }

    /// The subtrees `block` completes in `pool`, if computed.
    pub(crate) fn completions(
        &self,
        block: &BlockHash,
        pool: ShieldedPool,
    ) -> Option<Arc<[WindowSubtree]>> {
        self.entries()
            .get(block)
            .and_then(|entry| entry.completions.get(&pool).cloned())
    }

    /// Record the subtrees `block` completes in `pool`. A block whose frontier
    /// was never recorded has no entry to attach them to, and they are dropped.
    pub(crate) fn insert_completions(
        &self,
        block: &BlockHash,
        pool: ShieldedPool,
        completions: Arc<[WindowSubtree]>,
    ) {
        if let Some(entry) = self.entries().get_mut(block) {
            entry.completions.insert(pool, completions);
        }
    }

    /// Drop every block at or below `watermark`: the finalised store answers
    /// those heights, and the window no longer folds them.
    pub(crate) fn prune_through(&self, watermark: Height) {
        self.entries().retain(|_, entry| entry.height > watermark);
    }

    /// How many blocks are cached.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries().len()
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<BlockHash, Entry>> {
        // Every critical section is a single map operation that cannot panic
        // part-way, so a poisoned lock still guards a consistent map.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
