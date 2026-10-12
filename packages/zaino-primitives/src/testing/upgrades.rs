//! [`UpgradeSchedule`] builders for a [`MockChain`](super::MockChain) (zebrad regtest's config)

use zcash_protocol::consensus::NetworkUpgrade;

use crate::types::{
    slot, ConsensusBranchId, Height, NetworkUpgradeInfo, NetworkUpgradeStatus, UpgradeSchedule,
    ORDER,
};

const PRE_BLOSSOM_SPACING: u32 = 150;
const POST_BLOSSOM_SPACING: u32 = 75;
/// ZIP 218
const POST_NU7_SPACING: u32 = 25;

impl UpgradeSchedule {
    /// Overwinter ..= NU6.3 at `height`, NU7 off
    pub fn all_at(height: Height) -> Self {
        Self(ORDER.map(|upgrade| (upgrade != NetworkUpgrade::Nu7).then_some(height)))
    }

    pub fn with(mut self, upgrade: NetworkUpgrade, at: Height) -> Self {
        self.0[slot(upgrade)] = Some(at);
        self
    }

    /// `upgrade` and every scheduled upgrade after it at `at` (`all_at(h(1)).onward(Nu5, h(3))`)
    pub fn onward(mut self, upgrade: NetworkUpgrade, at: Height) -> Self {
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
        let scheduled = ORDER.iter().zip(self.0).filter_map(|(nu, at)| Some((nu, at?)));
        let mut previous: Option<(&NetworkUpgrade, Height)> = None;
        for (upgrade, at) in scheduled {
            if let Some((before, from)) = previous {
                assert!(from <= at, "{upgrade} at {at} activates before {before} at {from}");
            }
            previous = Some((upgrade, at));
        }
    }

    /// `PoWTargetSpacing(at)`, seconds
    pub(super) fn spacing(&self, at: Height) -> u32 {
        match (self.active(NetworkUpgrade::Nu7, at), self.active(NetworkUpgrade::Blossom, at)) {
            (true, _) => POST_NU7_SPACING,
            (false, true) => POST_BLOSSOM_SPACING,
            (false, false) => PRE_BLOSSOM_SPACING,
        }
    }

    /// `getblockchaininfo` `upgrades` at `tip`: every scheduled upgrade, active or pending
    pub(super) fn info(&self, tip: Height) -> Vec<NetworkUpgradeInfo> {
        let scheduled = ORDER.iter().zip(self.0).filter_map(|(nu, at)| Some((*nu, at?)));
        scheduled
            .map(|(upgrade, activation_height)| NetworkUpgradeInfo {
                branch_id: ConsensusBranchId::new(u32::from(upgrade.branch_id())),
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
