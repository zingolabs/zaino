//! Reads `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots` answer with, over one reader
//!
//! - per request: one 48 B record read, ≤ 33 node reads (32 B) per pool, ~1 KB serialized, no
//!   hashing

use incrementalmerkletree::frontier::{CommitmentTree, Frontier};
use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use zaino_persistence::SequenceRead;
use zaino_primitives::types::{
    BlockRef, CommitmentTreeBytes, PerPool, SubtreeRoot, TreeSizes, Treestate,
};
use zcash_primitives::merkle_tree::{write_commitment_tree, HashSer};
use zcash_protocol::consensus::{BlockHeight, Parameters};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::ShieldedPool;

use crate::nodes::MERKLE_DEPTH;
use crate::subtrees;
use crate::tables::subtree_table;
use crate::TreeStateReader;

impl<V: SequenceRead> TreeStateReader<V> {
    /// Tree state after `at`: all three pools, always, as the real tree's serialization
    ///
    /// - `000000` when empty (both clients map an empty *field* onto `CommitmentTree::empty()`:
    ///   `zcash_client_backend/src/proto.rs:404,420-444`, so `""` post-activation = wrong)
    /// - blank pools below their upgrade = the route's ([`pool_activations`])
    /// - reads mmapped nodes: run off async workers
    pub fn treestate(&self, at: BlockHeight) -> Result<Treestate, ServeError> {
        let record = self.height_record(at).ok_or(ServeError::NotFound { height: at })?;
        let inconsistent = ServeError::Inconsistent { height: at };
        let tree = |pool| self.pool_tree(pool, record.sizes).ok_or(inconsistent.clone());

        Ok(Treestate {
            block_hash: record.hash,
            height: at,
            time: record.time,
            trees: PerPool {
                sapling: tree(ShieldedPool::Sapling)?,
                orchard: tree(ShieldedPool::Orchard)?,
                ironwood: tree(ShieldedPool::Ironwood)?,
            },
        })
    }

    /// Completed subtree roots `start_index` inclusive to `start_index + max_entries` exclusive
    ///
    /// - `max_entries == 0` = to the last root
    /// - `start_index == count` = an empty list, never an error (pepper-sync's probe pass)
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
                let entry = subtrees::entry(self.view(), pool, index).expect("below the count");
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

/// Height each pool's tree begins (`None` = unscheduled)
///
/// - zebra's `z_gettreestate` omits a pool below its upgrade (lightwalletd then answers `""`)
pub fn pool_activations(schedule: &LocalNetwork) -> PerPool<Option<BlockHeight>> {
    PerPool::from_fn(|pool| schedule.activation_height(pool.activation_upgrade()))
}

/// `Inconsistent` = stored nodes rebuild no frontier (a fold bug, not a bad request)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("no tree state at height {height}")]
    NotFound { height: BlockHeight },

    #[error("stored tree state at height {height} is inconsistent")]
    Inconsistent { height: BlockHeight },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - Sapling, NU5 (orchard), NU6.3 (ironwood) from the schedule
    /// - unscheduled pool = `None`
    #[test]
    fn pool_activations_are_the_schedules_pool_upgrades() {
        use zaino_primitives::protocol::compiled_schedule;
        use zaino_source::testing::{all_at, h};
        use zcash_protocol::consensus::NetworkType;

        let mainnet = compiled_schedule(NetworkType::Main).expect("mainnet compiled in");
        let expected = PerPool {
            sapling: Some(h(419_200)),
            orchard: Some(h(1_687_104)),
            ironwood: Some(h(3_428_143)),
        };
        assert_eq!(pool_activations(&mainnet), expected, "mainnet: Sapling, NU5, NU6.3");

        let no_nu5 = LocalNetwork { nu5: None, ..all_at(h(1)) };
        let expected = PerPool { sapling: Some(h(1)), orchard: None, ironwood: Some(h(1)) };
        assert_eq!(pool_activations(&no_nu5), expected, "no NU5 = never orchard");
    }
}
