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

/// Why a listed schedule is no schedule Zaino can serve
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("upgrades listed out of protocol order: {listed:?}")]
    OutOfOrder { listed: Vec<(BranchId, BlockHeight)> },
    #[error("no Sapling activation listed (every shielded tree starts at or after it)")]
    NoSapling,
}

impl BlockchainInfo {
    /// Listed heights as a schedule (unlisted = never)
    ///
    /// - Sapling required: every schedule Zaino serves activates it (`pool_activations`)
    pub fn schedule(&self) -> Result<LocalNetwork, ScheduleError> {
        let listed = |upgrade: NetworkUpgrade| {
            let entry = self.activations.iter().find(|(id, _)| *id == upgrade.branch_id());
            entry.map(|(_, height)| *height)
        };
        let heights: Vec<BlockHeight> =
            UPGRADES_IN_ORDER.iter().filter_map(|&upgrade| listed(upgrade)).collect();
        if !heights.is_sorted() {
            return Err(ScheduleError::OutOfOrder { listed: self.activations.clone() });
        }
        if listed(NetworkUpgrade::Sapling).is_none() {
            return Err(ScheduleError::NoSapling);
        }
        Ok(LocalNetwork::from_activation_heights(listed))
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::protocol::compiled_schedule;
    use zcash_protocol::consensus::{NetworkType, NetworkUpgrade::Nu5, Parameters};
    use zcash_protocol::BlockHash;

    use super::*;

    fn listing(activations: &[(BranchId, u32)]) -> BlockchainInfo {
        let tip = BlockRef { hash: BlockHash([0; 32]), height: BlockHeight::from(3_500_000) };
        let activations =
            activations.iter().map(|&(branch, at)| (branch, BlockHeight::from(at))).collect();
        BlockchainInfo { tip, estimated_height: tip.height, activations }
    }

    #[test]
    fn mainnets_listing_in_reverse_order_reads_back_as_mainnets_schedule() {
        let mainnet = compiled_schedule(NetworkType::Main).expect("mainnet compiled in");
        let reversed = UPGRADES_IN_ORDER.iter().rev().filter_map(|&upgrade| {
            Some((upgrade.branch_id(), mainnet.activation_height(upgrade)?))
        });
        let tip = BlockRef { hash: BlockHash([0; 32]), height: BlockHeight::from(3_500_000) };
        let info =
            BlockchainInfo { tip, estimated_height: tip.height, activations: reversed.collect() };

        let schedule = info.schedule();

        assert_eq!(schedule, Ok(mainnet), "listed newest first");
    }

    #[test]
    fn an_unlisted_upgrade_never_activates() {
        let info =
            listing(&[(BranchId::Overwinter, 1), (BranchId::Sapling, 1), (BranchId::Blossom, 2)]);

        let nu5 = info.schedule().map(|schedule| schedule.activation_height(Nu5));

        assert_eq!(nu5, Ok(None), "NU5 unlisted");
    }

    #[test]
    fn a_listing_without_sapling_is_refused() {
        let info = listing(&[(BranchId::Overwinter, 347_500), (BranchId::Nu5, 1_687_104)]);

        assert_eq!(info.schedule(), Err(ScheduleError::NoSapling), "Overwinter + NU5 only");
    }

    #[test]
    fn a_listing_out_of_protocol_order_is_refused_naming_it() {
        let info = listing(&[(BranchId::Sapling, 419_200), (BranchId::Nu5, 1)]);

        let out_of_order = ScheduleError::OutOfOrder { listed: info.activations.clone() };
        assert_eq!(info.schedule(), Err(out_of_order), "NU5 at 1, below Sapling at 419 200");
    }
}
