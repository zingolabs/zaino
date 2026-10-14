//! Validator's chain state: its tip + its upgrade schedule

use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::local_consensus::LocalNetwork;

use zcash_protocol::BlockHash;

use crate::{ConsensusBranchIds, NetworkUpgradeInfo};

/// - `upgrades` = regtest's schedule ([`BlockchainInfo::schedule`])
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

impl BlockchainInfo {
    /// Activation heights as listed, keyed by branch id (unlisted = never)
    ///
    /// - panics on a listing out of protocol order (`getblockchaininfo` parse refuses one)
    pub fn schedule(&self) -> LocalNetwork {
        LocalNetwork::from_activation_heights(|upgrade| {
            let listed = self.upgrades.iter().find(|nu| nu.branch_id == upgrade.branch_id());
            listed.map(|nu| nu.activation_height)
        })
    }
}

#[cfg(test)]
mod tests {
    use zcash_protocol::consensus::{
        BranchId, NetworkType, NetworkUpgrade::Nu5, Parameters, UPGRADES_IN_ORDER,
    };

    use super::*;
    use crate::NetworkUpgradeStatus;
    use zaino_primitives::protocol::compiled_schedule;

    /// A validator's listing (in no particular order) reads back as the schedule it lists; an
    /// unlisted upgrade never activates
    #[test]
    fn a_listed_schedule_reads_back_and_an_unlisted_upgrade_never_activates() {
        let mainnet = compiled_schedule(NetworkType::Main).expect("mainnet compiled in");
        let listing = UPGRADES_IN_ORDER.iter().rev().filter_map(|&upgrade| {
            Some(NetworkUpgradeInfo {
                branch_id: upgrade.branch_id(),
                name: "label only".to_owned(),
                activation_height: mainnet.activation_height(upgrade)?,
                status: NetworkUpgradeStatus::Active,
            })
        });
        let mut info = BlockchainInfo {
            blocks: BlockHeight::from(3_500_000),
            estimated_height: BlockHeight::from(3_500_000),
            best_block_hash: BlockHash([0; 32]),
            sapling_activation: BlockHeight::from(419_200),
            upgrades: listing.collect(),
            consensus: ConsensusBranchIds {
                chain_tip: BranchId::Nu6_3,
                next_block: BranchId::Nu6_3,
            },
        };
        assert_eq!(info.schedule(), mainnet);

        info.upgrades.retain(|nu| nu.branch_id != Nu5.branch_id());
        assert_eq!(info.schedule().activation_height(Nu5), None, "unlisted = never");
    }
}
