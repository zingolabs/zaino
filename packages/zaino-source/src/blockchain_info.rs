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
    use zcash_protocol::consensus::{
        NetworkType,
        NetworkUpgrade::{Nu5, Sapling},
        Parameters,
    };
    use zcash_protocol::BlockHash;

    use super::*;

    /// - Listing in any order reads back as the schedule it lists
    /// - Unlisted = never
    /// - Out of protocol order or without Sapling = refused
    #[test]
    fn a_listed_schedule_reads_back_unlisted_never_activates_and_disorder_or_no_sapling_is_refused()
    {
        let mainnet = compiled_schedule(NetworkType::Main).expect("mainnet compiled in");
        let listing = UPGRADES_IN_ORDER.iter().rev().filter_map(|&upgrade| {
            Some((upgrade.branch_id(), mainnet.activation_height(upgrade)?))
        });
        let tip = BlockRef { hash: BlockHash([0; 32]), height: BlockHeight::from(3_500_000) };
        let mut info =
            BlockchainInfo { tip, estimated_height: tip.height, activations: listing.collect() };
        assert_eq!(info.schedule(), Ok(mainnet));

        let mut no_sapling = info.clone();
        no_sapling.activations.retain(|(id, _)| *id != Sapling.branch_id());
        assert_eq!(no_sapling.schedule(), Err(ScheduleError::NoSapling));

        info.activations.retain(|(id, _)| *id != Nu5.branch_id());
        assert_eq!(info.schedule().map(|s| s.activation_height(Nu5)), Ok(None), "unlisted");

        info.activations.push((BranchId::Nu5, BlockHeight::from(1)));
        let out_of_order = ScheduleError::OutOfOrder { listed: info.activations.clone() };
        assert_eq!(info.schedule(), Err(out_of_order), "NU5 below Sapling");
    }
}
