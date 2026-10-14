//! Regtest schedules for a [`MockChain`](super::MockChain) (zebrad regtest's config)

use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters, UPGRADES_IN_ORDER};
use zcash_protocol::local_consensus::LocalNetwork;

use crate::types::{NetworkUpgradeInfo, NetworkUpgradeStatus};

/// Overwinter ..= NU6.3 at `height`, NU7 off
pub fn all_at(height: BlockHeight) -> LocalNetwork {
    LocalNetwork::from_activation_heights(|upgrade| {
        (upgrade != NetworkUpgrade::Nu7).then_some(height)
    })
}

/// `getblockchaininfo` `upgrades` at `tip`: every scheduled upgrade, active or pending
pub(super) fn info(upgrades: &LocalNetwork, tip: BlockHeight) -> Vec<NetworkUpgradeInfo> {
    let scheduled = UPGRADES_IN_ORDER.iter().filter_map(|&upgrade| {
        let activation_height = upgrades.activation_height(upgrade)?;
        Some(NetworkUpgradeInfo {
            branch_id: upgrade.branch_id(),
            name: upgrade.to_string(),
            activation_height,
            status: match activation_height <= tip {
                true => NetworkUpgradeStatus::Active,
                false => NetworkUpgradeStatus::Pending,
            },
        })
    });
    scheduled.collect()
}
