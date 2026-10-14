//! Network upgrade schedule as the validator reports it (regtest's `LocalNetwork` source)

use zcash_protocol::consensus::{BlockHeight, BranchId};

/// At the validator's current tip (zebrad never reports a disabled upgrade)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkUpgradeStatus {
    Active,
    Pending,
}

/// One schedule entry
///
/// - `branch_id` = the identity to key on
/// - `name`: display only, never matched
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkUpgradeInfo {
    pub branch_id: BranchId,
    pub name: String,
    pub activation_height: BlockHeight,
    pub status: NetworkUpgradeStatus,
}

/// Branch at the tip + for the next block (differ exactly when the next block activates one)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsensusBranchIds {
    pub chain_tip: BranchId,
    pub next_block: BranchId,
}
