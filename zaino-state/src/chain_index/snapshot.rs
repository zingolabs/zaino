/// State that has not been confirmed by at least 100 blocks.
pub mod non_finalised_state;
use non_finalised_state::NonfinalizedBlockCacheSnapshot;

use crate::chain_index::snapshot::non_finalised_state::BestTip;
use crate::chain_index::types;
use crate::IndexedBlock;

use std::sync::Arc;

impl<T> NonFinalized for Arc<T>
where
    T: NonFinalized,
{
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock> {
        self.as_ref().get_chainblock_by_hash(target_hash)
    }

    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock> {
        self.as_ref().get_chainblock_by_height(target_height)
    }

    fn best_chaintip(&self) -> BestTip {
        self.as_ref().best_chaintip()
    }
}

/// A snapshot of the non-finalized state, for consistent queries
pub trait NonFinalized {
    /// Hash -> block
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock>;
    /// Height -> block
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock>;
    /// Get the tip of the best chain, according to the snapshot
    fn best_chaintip(&self) -> BestTip;
}

impl NonFinalized for NonfinalizedBlockCacheSnapshot {
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock> {
        self.blocks.iter().find_map(|(hash, chainblock)| {
            if hash == target_hash {
                Some(chainblock)
            } else {
                None
            }
        })
    }
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock> {
        self.heights_to_hashes.iter().find_map(|(height, hash)| {
            if height == target_height {
                self.get_chainblock_by_hash(hash)
            } else {
                None
            }
        })
    }

    fn best_chaintip(&self) -> BestTip {
        self.best_tip
    }
}
