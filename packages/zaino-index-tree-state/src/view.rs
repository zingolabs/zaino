//! [`ReadView`]: tree states and subtree roots over every tier, pinned once per request

use incrementalmerkletree::{frontier::CommitmentTree, Hashable};
use orchard::tree::MerkleHashOrchard;
use zaino_persistence::{SequenceRead, TieredView, View};
use zaino_primitives::types::{
    BlockRef, CommitmentTreeBytes, Height, PerPool, ShieldedPool, SubtreeRoot, Treestate,
};
use zcash_primitives::merkle_tree::{write_commitment_tree, HashSer};

use crate::{
    fold::frontier_at,
    height_record,
    nodes::{NodeView, MERKLE_DEPTH},
    subtree_table, subtrees, ServeError,
};

/// Cumulative commitments in each pool after some height (genesis-minus-one = [`Default`])
pub(crate) type PoolSizes = PerPool<u64>;

/// Held blocks + committed store, one publication (one load per request: the seam cannot move
/// under it)
#[derive(Clone)]
pub struct ReadView<V> {
    view: TieredView<V>,
}

impl<V: View> std::fmt::Debug for ReadView<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadView").field("view", &self.view).finish()
    }
}

impl<V: View> ReadView<V> {
    pub(crate) fn new(view: TieredView<V>) -> Self {
        Self { view }
    }

    /// Last height either tier can answer, inclusive (`None` = nothing held)
    pub(crate) fn tip(&self) -> Option<Height> {
        self.view.tip().map(|tip| tip.height)
    }

    /// Last committed height, inclusive (`None` = nothing committed)
    pub(crate) fn finalized(&self) -> Option<Height> {
        self.view.durable().tip().map(|tip| tip.height)
    }

    /// `at` held above the committed files: reorg-able, and among the ~1000 heights every synced
    /// wallet asks about (bounded: a per-publication memo keyed on these stays small)
    pub fn is_non_finalized(&self, at: Height) -> bool {
        Some(at) <= self.tip() && Some(at) > self.finalized()
    }
}

impl<V: SequenceRead> ReadView<V> {
    /// Tree state after `at`: all three pools, always, as the real tree's serialization
    ///
    /// - `000000` when empty: both clients map an empty *field* onto `CommitmentTree::empty()`
    ///   (`zcash_client_backend/src/proto.rs:404,420-444`), so `""` post-activation is wrong
    /// - served through `TreeStateService::treestate_in` (Sapling floor, activation schedule)
    /// - reads mmapped nodes: run off async workers
    pub(crate) fn treestate(&self, at: Height) -> Result<Treestate, ServeError> {
        let record = height_record(&self.view, at).ok_or(ServeError::NotFound { height: at })?;
        let sizes = record.positions();
        let nodes = |pool| NodeView { view: &self.view, pool, size: *sizes.get(pool) };
        let inconsistent = ServeError::Inconsistent { height: at };

        Ok(Treestate {
            block_hash: record.hash,
            height: at,
            time: record.time,
            sapling: pool_tree::<sapling_crypto::Node, _>(nodes(ShieldedPool::Sapling))
                .ok_or(inconsistent.clone())?,
            orchard: pool_tree::<MerkleHashOrchard, _>(nodes(ShieldedPool::Orchard))
                .ok_or(inconsistent.clone())?,
            ironwood: pool_tree::<MerkleHashOrchard, _>(nodes(ShieldedPool::Ironwood))
                .ok_or(inconsistent)?,
        })
    }

    /// Completed subtree roots `start_index` inclusive to `start_index + max_entries` exclusive;
    /// `max_entries == 0` = to the last root; `start_index == count` = an empty list, never an
    /// error (pepper-sync's probe pass)
    ///
    /// - completing block's hash = its height's record (a subtree completes at a held height)
    pub fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: u16,
    ) -> Result<Vec<SubtreeRoot>, ServeError> {
        let limit = match max_entries {
            0 => u64::MAX,
            entries => u64::from(entries),
        };
        let count = self.view.len(subtree_table(pool));
        let start = u64::from(start_index);
        let end = count.min(start.saturating_add(limit));

        (start..end)
            .map(|index| {
                let entry = subtrees::entry(&self.view, pool, index).expect("below the count");
                let height = entry.end_height;
                let record =
                    height_record(&self.view, height).ok_or(ServeError::Inconsistent { height })?;
                Ok(SubtreeRoot {
                    root: entry.root,
                    completing: BlockRef { hash: record.hash, height },
                })
            })
            .collect()
    }
}

/// One pool's serialized tree, in the form both clients parse (`None` = stored nodes corrupt)
///
/// - `read_commitment_tree` on the other end (`pepper-sync/src/witness.rs:313-372`), so
///   `write_commitment_tree` here, not a frontier encoding
fn pool_tree<H: Hashable + HashSer + Clone, V: SequenceRead>(
    nodes: NodeView<'_, V>,
) -> Option<CommitmentTreeBytes> {
    let tree = CommitmentTree::<H, MERKLE_DEPTH>::from_frontier(&frontier_at::<H, V>(nodes)?);
    let mut serialized = Vec::with_capacity(1090);
    write_commitment_tree(&tree, &mut serialized).expect("writing into a Vec cannot fail");
    Some(CommitmentTreeBytes::new(serialized))
}
