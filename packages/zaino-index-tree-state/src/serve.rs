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
use zcash_protocol::consensus::BlockHeight;
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
    /// - blank pools below their upgrade = the route's (`protocol::pool_activations`)
    /// - reads mmapped nodes: run off async workers
    pub fn treestate(&self, at: BlockHeight) -> Result<Treestate, ServeError> {
        let record = self.height_record(at).ok_or(ServeError::NotFound { height: at })?;
        let tree = |pool| {
            self.pool_tree(pool, record.sizes).unwrap_or_else(|| {
                panic!("tree_state: stored nodes rebuild the {pool:?} frontier at {at}")
            })
        };

        Ok(Treestate {
            block_hash: record.hash,
            height: at,
            time: record.time,
            trees: PerPool::from_fn(tree),
        })
    }

    /// Every completed subtree root of `pool`, index order
    ///
    /// - completing block's hash = its height's record (a subtree completes at a held height)
    pub fn subtree_roots(&self, pool: ShieldedPool) -> Vec<SubtreeRoot> {
        (0..self.len(subtree_table(pool)))
            .map(|index| {
                let entry = subtrees::entry(self.view(), pool, index).expect("below the count");
                let height = entry.end_height;
                let record = self.height_record(height).unwrap_or_else(|| {
                    panic!("tree_state: heights holds subtree {index}'s completing height {height}")
                });
                SubtreeRoot { root: entry.root, completing: BlockRef { hash: record.hash, height } }
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("no tree state at height {height}")]
    NotFound { height: BlockHeight },
}
