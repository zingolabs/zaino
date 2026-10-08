//! [`TreeStateReader`]: typed reads over any view of the tables (serving's and a fold's parent)

use incrementalmerkletree::{
    frontier::{CommitmentTree, Frontier, NonEmptyFrontier},
    Address, Hashable, Position, Source,
};
use orchard::tree::MerkleHashOrchard;
use zaino_persistence::{OverlayView, SequenceRead, SequenceTable, View};
use zaino_primitives::types::{
    BlockRef, CommitmentTreeBytes, Height, ShieldedPool, SubtreeRoot, TreeSizes, Treestate,
};
use zcash_primitives::merkle_tree::{write_commitment_tree, HashSer};

use crate::{
    heights::{self, TreeStateHeight},
    level_table,
    nodes::{self, slot, MERKLE_DEPTH},
    subtree_table, subtrees, ServeError, HEIGHTS,
};

/// One state of the tables, never moving while held (one pin per request, one parent per fold)
#[derive(Clone)]
pub struct TreeStateReader<V> {
    view: V,
}

impl<V: View> std::fmt::Debug for TreeStateReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeStateReader").field("tip", &self.view.tip()).finish_non_exhaustive()
    }
}

impl<V: View> TreeStateReader<V> {
    /// `view` of a store opened with [`TABLES`](crate::TABLES)
    pub fn new(view: V) -> Self {
        Self { view }
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }

    /// Last height held, inclusive (`None` = nothing held)
    pub(crate) fn tip(&self) -> Option<Height> {
        self.view.tip().map(|tip| tip.height)
    }
}

/// Snapshot's seam: its layer above the committed files
impl<V: View> TreeStateReader<OverlayView<V>> {
    /// `at` in the layer above the committed files: among the ~1000 heights every synced wallet
    /// asks about (bounded: a per-snapshot memo keyed on these stays small)
    pub fn is_non_finalized(&self, at: Height) -> bool {
        let committed = self.view.durable().tip().map(|tip| tip.height);
        Some(at) <= self.tip() && Some(at) > committed
    }
}

impl<V: SequenceRead> TreeStateReader<V> {
    /// Tree state after `at`: all three pools, always, as the real tree's serialization
    ///
    /// - `000000` when empty: both clients map an empty *field* onto `CommitmentTree::empty()`
    ///   (`zcash_client_backend/src/proto.rs:404,420-444`), so `""` post-activation is wrong
    /// - Sapling floor and blank pools below their upgrade = the route's ([`PoolActivations`])
    /// - reads mmapped nodes: run off async workers
    ///
    /// [`PoolActivations`]: crate::PoolActivations
    pub fn treestate(&self, at: Height) -> Result<Treestate, ServeError> {
        let record = self.height_record(at).ok_or(ServeError::NotFound { height: at })?;
        let inconsistent = ServeError::Inconsistent { height: at };
        let tree = |pool| self.pool_tree(pool, record.sizes).ok_or(inconsistent.clone());

        Ok(Treestate {
            block_hash: record.hash,
            height: at,
            time: record.time,
            sapling: tree(ShieldedPool::Sapling)?,
            orchard: tree(ShieldedPool::Orchard)?,
            ironwood: tree(ShieldedPool::Ironwood)?,
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
        let count = self.len(subtree_table(pool));
        let start = u64::from(start_index);
        let end = count.min(start.saturating_add(limit));

        (start..end)
            .map(|index| {
                let entry = subtrees::entry(&self.view, pool, index).expect("below the count");
                let height = entry.end_height;
                let record =
                    self.height_record(height).ok_or(ServeError::Inconsistent { height })?;
                Ok(SubtreeRoot {
                    root: entry.root,
                    completing: BlockRef { hash: record.hash, height },
                })
            })
            .collect()
    }

    /// Tree sizes after the tip ([`TreeSizes::ZERO`] = nothing held)
    pub(crate) fn tip_sizes(&self) -> TreeSizes {
        let Some(tip) = self.tip() else {
            return TreeSizes::ZERO;
        };
        self.height_record(tip).expect("a view holds its tip's record").sizes
    }

    /// `pool`'s frontier at tree size `size` (`None` = a node missing or non-canonical: corrupt)
    ///
    /// - no hashing: every ommer = a retained left sibling, leaf = `l00[size - 1]`
    pub(crate) fn frontier<H: Hashable + HashSer + Clone>(
        &self,
        pool: ShieldedPool,
        size: u64,
    ) -> Option<Frontier<H, MERKLE_DEPTH>> {
        let Some(last) = size.checked_sub(1) else {
            return Some(Frontier::empty());
        };
        let position = Position::from(last);
        let node = |addr| {
            let (level, index) = slot(addr)?;
            let record = self.view.sequence(level_table(pool, level)).record(index)?;
            nodes::decode::<H>(record[..].try_into().expect("NODE bytes"))
        };

        // `Source::Past(i)` indexes `ommers[i]` in `witness_addrs`' ascending-level order (that
        // order = the contract)
        let ommers = position
            .witness_addrs(position.root_level())
            .filter(|(_, source)| matches!(source, Source::Past(_)))
            .map(|(addr, _)| node(addr))
            .collect::<Option<Vec<H>>>()?;
        let leaf = node(Address::from(position))?;

        NonEmptyFrontier::from_parts(position, leaf, ommers).and_then(Frontier::try_from).ok()
    }

    /// Records in `table` (= the slot of its next append)
    pub(crate) fn len(&self, table: SequenceTable) -> u64 {
        self.view.sequence(table).count()
    }

    /// `None` above the tip
    fn height_record(&self, at: Height) -> Option<TreeStateHeight> {
        let bytes = self.view.sequence(HEIGHTS).record(u64::from(at))?;
        Some(heights::decode(bytes[..].try_into().expect("RECORD bytes")))
    }

    /// `pool`'s serialized tree at `sizes`, the form both clients parse (`None` = corrupt)
    ///
    /// - `read_commitment_tree` on the other end (`pepper-sync/src/witness.rs:313-372`), so
    ///   `write_commitment_tree` here, not a frontier encoding
    fn pool_tree(&self, pool: ShieldedPool, sizes: TreeSizes) -> Option<CommitmentTreeBytes> {
        let size = u64::from(sizes.get(pool).get());
        Some(match pool {
            ShieldedPool::Sapling => serialize(&self.frontier::<sapling_crypto::Node>(pool, size)?),
            ShieldedPool::Orchard | ShieldedPool::Ironwood => {
                serialize(&self.frontier::<MerkleHashOrchard>(pool, size)?)
            }
        })
    }
}

fn serialize<H: Hashable + HashSer + Clone>(
    frontier: &Frontier<H, MERKLE_DEPTH>,
) -> CommitmentTreeBytes {
    let tree = CommitmentTree::<H, MERKLE_DEPTH>::from_frontier(frontier);
    let mut serialized = Vec::with_capacity(1090);
    write_commitment_tree(&tree, &mut serialized).expect("writing into a Vec cannot fail");
    CommitmentTreeBytes::new(serialized)
}
