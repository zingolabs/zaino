//! Nonfinalised tier (the fold applied, not yet fsynced) and the [`ReadView`] over both tiers
//!
//! - `imbl` throughout: `view()` = a pointer copy, a reader pins a consistent instant
//! - split by *tree size*, not height (retention = pure function of size, [`retained_nodes`]):
//!   one predicate decides what `finalize` writes

use std::sync::Arc;

use imbl::OrdMap;
use incrementalmerkletree::{frontier::CommitmentTree, Hashable};
use orchard::tree::MerkleHashOrchard;
use zaino_primitives::types::{
    BlockRef, CommitmentTreeBytes, Extent, Height, PerPool, ShieldedPool, SubtreeRoot, Treestate,
};
use zcash_primitives::merkle_tree::{write_commitment_tree, HashSer};

use crate::{
    fold::{frontier_at, SUBTREE_LEVEL},
    heights::TreeStateHeight,
    nodes::{retained_nodes, NodeView, NonFinalizedNodes, MERKLE_DEPTH},
    subtrees::SubtreeEntry,
    ServeError, Snapshot,
};

/// Cumulative commitments in each pool after some height (genesis-minus-one = [`Default`])
pub(crate) type PoolSizes = PerPool<u64>;

/// One pool's applied-but-unwritten fold output
#[derive(Debug, Clone, Default)]
pub(crate) struct NonFinalizedPool {
    pub(crate) nodes: NonFinalizedNodes,
    pub(crate) subtrees: OrdMap<u64, SubtreeEntry>,
}

impl NonFinalizedPool {
    /// `(retained by a tree of `size`, retained only by later heights)`
    fn split(&self, size: u64) -> (Self, Self) {
        let mut below = Self::default();
        let mut above = Self::default();

        for (&(level, index), node) in &self.nodes {
            let half = if index < retained_nodes(level, size) { &mut below } else { &mut above };
            half.nodes.insert((level, index), *node);
        }

        for (&index, root) in &self.subtrees {
            let half = if index < size >> SUBTREE_LEVEL { &mut below } else { &mut above };
            half.subtrees.insert(index, *root);
        }

        (below, above)
    }
}

/// Nonfinalised tier, all three pools (read before the store)
///
/// - `end` = applied extent (= committed extent when nothing is buffered)
#[derive(Debug, Clone, Default)]
pub struct NonFinalizedTrees {
    pub(crate) heights: OrdMap<Height, TreeStateHeight>,
    pub(crate) pools: PerPool<NonFinalizedPool>,
    pub(crate) end: Extent,
}

impl NonFinalizedTrees {
    /// Empty, sitting on a durable `end`
    pub(crate) fn empty_at(end: Extent) -> Self {
        Self { end, ..Self::default() }
    }

    /// `pool`'s tree of `size` commitments, these nodes over `durable`'s
    pub(crate) fn nodes<'a>(
        &'a self,
        durable: &'a Snapshot,
        pool: ShieldedPool,
        size: u64,
    ) -> NodeView<'a> {
        NodeView {
            non_finalized: &self.pools.get(pool).nodes,
            durable: &durable.pools.get(pool).nodes,
            size,
        }
    }

    /// Splits at `cut` (`sizes` = trees after its last height)
    ///
    /// - left = the batch `finalize` writes
    /// - right = what stays buffered
    pub(crate) fn split(&self, cut: Extent, sizes: PoolSizes) -> (Self, Self) {
        let first_above = cut.next();
        let (heights_below, at_cut, mut heights_above) =
            self.heights.clone().split_lookup(&first_above);
        if let Some(record) = at_cut {
            heights_above.insert(first_above, record);
        }
        let halves = PerPool::from_fn(|pool| self.pools.get(pool).split(*sizes.get(pool)));

        (
            Self {
                heights: heights_below,
                pools: halves.clone().map(|(below, _)| below),
                end: cut,
            },
            Self { heights: heights_above, pools: halves.map(|(_, above)| above), end: self.end },
        )
    }
}

/// Nonfinalised + committed, one publication (one load per request: the seam cannot move under it)
#[derive(Debug, Clone)]
pub struct ReadView {
    non_finalized: NonFinalizedTrees,
    durable: Arc<Snapshot>,
}

impl ReadView {
    pub(crate) fn new(non_finalized: NonFinalizedTrees, durable: Arc<Snapshot>) -> Self {
        if let Some(&(first, _)) = non_finalized.heights.get_min() {
            assert_eq!(first, durable.end.next(), "nonfinalised must start where the files end");
        }
        Self { non_finalized, durable }
    }

    /// Every height either tier can answer
    pub(crate) fn extent(&self) -> Extent {
        self.non_finalized.end.max(self.durable.end)
    }

    /// Committed heights alone
    pub(crate) fn finalized(&self) -> Extent {
        self.durable.end
    }

    /// `at` held above the committed files: reorg-able, and among the ~1000 heights every synced
    /// wallet asks about (bounded: a per-publication memo keyed on these stays small)
    pub fn is_nonfinalized(&self, at: Height) -> bool {
        self.extent().contains(at) && !self.finalized().contains(at)
    }

    /// Tree state after `at`: all three pools, always, as the real tree's serialization
    ///
    /// - `000000` when empty: both clients map an empty *field* onto `CommitmentTree::empty()`
    ///   (`zcash_client_backend/src/proto.rs:404,420-444`), so `""` post-activation is wrong
    /// - reads mmapped nodes: run off async workers
    pub fn treestate(&self, at: Height) -> Result<Treestate, ServeError> {
        let record = self.record(at)?;
        let sizes = record.positions();
        let nodes = |pool| self.non_finalized.nodes(&self.durable, pool, *sizes.get(pool));
        let inconsistent = ServeError::Inconsistent { height: at };

        Ok(Treestate {
            block_hash: record.hash,
            height: at,
            time: record.time,
            sapling: pool_tree::<sapling_crypto::Node>(nodes(ShieldedPool::Sapling))
                .ok_or(inconsistent.clone())?,
            orchard: pool_tree::<MerkleHashOrchard>(nodes(ShieldedPool::Orchard))
                .ok_or(inconsistent.clone())?,
            ironwood: pool_tree::<MerkleHashOrchard>(nodes(ShieldedPool::Ironwood))
                .ok_or(inconsistent)?,
        })
    }

    /// Tree state at the highest height either tier holds
    pub fn latest(&self) -> Result<Treestate, ServeError> {
        self.treestate(self.extent().last().ok_or(ServeError::Empty)?)
    }

    /// `at`'s height record, nonfinalised first
    fn record(&self, at: Height) -> Result<TreeStateHeight, ServeError> {
        match self.non_finalized.heights.get(&at) {
            Some(record) => Ok(*record),
            None => self.durable.height(at).ok_or(ServeError::NotFound { height: at }),
        }
    }

    /// Completed subtree roots `[start, start + max_entries)`, `max_entries == 0` = to the end;
    /// `start == count` = an empty list, never an error (pepper-sync's probe pass)
    ///
    /// - completing block's hash = its height's record (a subtree completes at a held height)
    pub fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: u16,
    ) -> Result<Vec<SubtreeRoot>, ServeError> {
        let durable = &self.durable.pools.get(pool).subtrees;
        let non_finalized = &self.non_finalized.pools.get(pool).subtrees;

        let limit = match max_entries {
            0 => u64::MAX,
            entries => u64::from(entries),
        };
        let count = durable.count().max(non_finalized.get_max().map_or(0, |(index, _)| index + 1));
        let start = u64::from(start_index);
        let end = count.min(start.saturating_add(limit));

        (start..end)
            .map(|index| {
                let entry = match non_finalized.get(&index) {
                    Some(entry) => *entry,
                    None => durable.get(index),
                };
                let record = self
                    .record(entry.end_height)
                    .map_err(|_| ServeError::Inconsistent { height: entry.end_height })?;
                Ok(SubtreeRoot {
                    root: entry.root,
                    completing: BlockRef { hash: record.hash, height: entry.end_height },
                })
            })
            .collect()
    }
}

/// One pool's serialized tree, in the form both clients parse (`None` = stored nodes corrupt)
///
/// - `read_commitment_tree` on the other end (`pepper-sync/src/witness.rs:313-372`), so
///   `write_commitment_tree` here, not a frontier encoding
fn pool_tree<H: Hashable + HashSer + Clone>(nodes: NodeView<'_>) -> Option<CommitmentTreeBytes> {
    let tree = CommitmentTree::<H, MERKLE_DEPTH>::from_frontier(&frontier_at::<H>(nodes)?);
    let mut serialized = Vec::with_capacity(1090);
    write_commitment_tree(&tree, &mut serialized).expect("writing into a Vec cannot fail");
    Some(CommitmentTreeBytes::new(serialized))
}
