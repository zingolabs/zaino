//! Validator's chain state: its tip + its upgrade schedule

use super::{BlockHash, ConsensusBranchIds, Height, NetworkUpgradeInfo};

/// - `upgrades` = regtest's schedule ([`UpgradeSchedule::reported`](super::UpgradeSchedule))
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
