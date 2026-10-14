//! Validator's chain state: its tip, its network-tip estimate, its upgrade schedule

use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::{BlockHeight, BranchId, NetworkUpgrade, UPGRADES_IN_ORDER};
use zcash_protocol::local_consensus::LocalNetwork;

/// `getblockchaininfo` as Zaino reads it
///
/// - `activations` = upgrades this build knows (an unknown branch id skipped, never an error)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockchainInfo {
    pub tip: BlockRef,
    pub estimated_height: BlockHeight,
    pub activations: Vec<(BranchId, BlockHeight)>,
}

impl BlockchainInfo {
    /// Listed heights as a schedule (unlisted = never); `None` = listed out of protocol order
    pub fn schedule(&self) -> Option<LocalNetwork> {
        let listed = |upgrade: NetworkUpgrade| {
            let entry = self.activations.iter().find(|(id, _)| *id == upgrade.branch_id());
            entry.map(|(_, height)| *height)
        };
        let heights: Vec<BlockHeight> =
            UPGRADES_IN_ORDER.iter().filter_map(|&upgrade| listed(upgrade)).collect();
        heights.is_sorted().then(|| LocalNetwork::from_activation_heights(listed))
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::protocol::compiled_schedule;
    use zcash_protocol::consensus::{NetworkType, NetworkUpgrade::Nu5, Parameters};
    use zcash_protocol::BlockHash;

    use super::*;

    /// A listing in any order reads back as the schedule it lists; unlisted = never; out of
    /// protocol order = no schedule
    #[test]
    fn a_listed_schedule_reads_back_unlisted_never_activates_and_disorder_is_refused() {
        let mainnet = compiled_schedule(NetworkType::Main).expect("mainnet compiled in");
        let listing = UPGRADES_IN_ORDER.iter().rev().filter_map(|&upgrade| {
            Some((upgrade.branch_id(), mainnet.activation_height(upgrade)?))
        });
        let tip = BlockRef { hash: BlockHash([0; 32]), height: BlockHeight::from(3_500_000) };
        let mut info =
            BlockchainInfo { tip, estimated_height: tip.height, activations: listing.collect() };
        assert_eq!(info.schedule(), Some(mainnet));

        info.activations.retain(|(id, _)| *id != Nu5.branch_id());
        assert_eq!(info.schedule().map(|s| s.activation_height(Nu5)), Some(None), "unlisted");

        info.activations.push((BranchId::Nu5, BlockHeight::from(1)));
        assert_eq!(info.schedule(), None, "NU5 below Sapling");
    }
}
