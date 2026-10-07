//! Validator's chain state: its tip + the upgrade schedule Zaino adopts

use super::{BlockHash, ConsensusBranchIds, Height, NetworkUpgradeInfo};

/// - `upgrades` = consensus input (activation heights from the validator, never compiled in)
/// - `estimated_height` = network tip estimate (even when synced)
/// - `sapling_activation` = Sapling's `upgrades` entry (every network has one; required at parse)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockchainInfo {
    pub blocks: Height,
    pub estimated_height: Height,
    pub best_block_hash: BlockHash,
    pub sapling_activation: Height,
    pub upgrades: Vec<NetworkUpgradeInfo>,
    pub consensus: ConsensusBranchIds,
}
