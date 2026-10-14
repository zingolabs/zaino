//! [`UpgradeSchedule`] builders for a [`MockChain`](super::MockChain) (zebrad regtest's config)

use zcash_protocol::consensus::NetworkUpgrade;

use crate::types::{
    slot, ConsensusBranchId, NetworkUpgradeInfo, NetworkUpgradeStatus, UpgradeSchedule, ORDER,
};
use zcash_protocol::consensus::BlockHeight;

impl UpgradeSchedule {
    /// Overwinter ..= NU6.3 at `height`, NU7 off
    pub fn all_at(height: BlockHeight) -> Self {
        Self(ORDER.map(|upgrade| (upgrade != NetworkUpgrade::Nu7).then_some(height)))
    }

    pub fn with(mut self, upgrade: NetworkUpgrade, at: BlockHeight) -> Self {
        self.0[slot(upgrade)] = Some(at);
        self
    }

    /// `upgrade` and every scheduled upgrade after it at `at` (`all_at(h(1)).onward(Nu5, h(3))`)
    pub fn onward(mut self, upgrade: NetworkUpgrade, at: BlockHeight) -> Self {
        for scheduled in self.0[slot(upgrade)..].iter_mut().filter(|held| held.is_some()) {
            *scheduled = Some(at);
        }
        self.0[slot(upgrade)] = Some(at);
        self
    }

    pub fn without(mut self, upgrade: NetworkUpgrade) -> Self {
        self.0[slot(upgrade)] = None;
        self
    }

    /// Scheduled heights non-decreasing in protocol order (zebrad refuses any other config)
    pub(super) fn assert_ordered(&self) {
        let mut previous: Option<(NetworkUpgrade, BlockHeight)> = None;
        for (upgrade, at) in self.scheduled() {
            if let Some((before, from)) = previous {
                assert!(from <= at, "{upgrade} at {at} activates before {before} at {from}");
            }
            previous = Some((upgrade, at));
        }
    }

    /// `getblockchaininfo` `upgrades` at `tip`: every scheduled upgrade, active or pending
    pub(super) fn info(&self, tip: BlockHeight) -> Vec<NetworkUpgradeInfo> {
        self.scheduled()
            .map(|(upgrade, activation_height)| NetworkUpgradeInfo {
                branch_id: ConsensusBranchId::of(upgrade),
                name: upgrade.to_string(),
                activation_height,
                status: match activation_height <= tip {
                    true => NetworkUpgradeStatus::Active,
                    false => NetworkUpgradeStatus::Pending,
                },
            })
            .collect()
    }
}
