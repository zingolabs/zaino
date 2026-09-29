//! The validator's chain state: its tip and the upgrade schedule Zaino adopts

use super::{BlockHash, ConsensusBranchIds, Height, NetworkUpgradeInfo};

/// `upgrades` = a consensus input (Zaino's activation heights come from the validator, never a
/// compiled-in schedule that could disagree)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockchainInfo {
    pub blocks: Height,
    /// Network tip estimate (an estimate even when synced)
    pub estimated_height: Height,
    pub best_block_hash: BlockHash,
    /// Sapling's entry in `upgrades` (every network has one; required at parse)
    pub sapling_activation: Height,
    pub upgrades: Vec<NetworkUpgradeInfo>,
    pub consensus: ConsensusBranchIds,
}
