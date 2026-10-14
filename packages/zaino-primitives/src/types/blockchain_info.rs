//! Validator's chain state: its tip + its upgrade schedule

use super::{BlockHash, ConsensusBranchIds, NetworkUpgradeInfo};
use zcash_protocol::consensus::BlockHeight;

/// - `upgrades` = regtest's schedule ([`UpgradeSchedule::reported`](super::UpgradeSchedule))
/// - `estimated_height` = network tip estimate (even when synced)
/// - `sapling_activation` = Sapling's `upgrades` entry (every network has one; required at parse)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockchainInfo {
    pub blocks: BlockHeight,
    pub estimated_height: BlockHeight,
    pub best_block_hash: BlockHash,
    pub sapling_activation: BlockHeight,
    pub upgrades: Vec<NetworkUpgradeInfo>,
    pub consensus: ConsensusBranchIds,
}
