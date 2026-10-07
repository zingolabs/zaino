//! Activation schedule a [`MockChain`](super::MockChain) mines under (zebrad regtest's config)

use zcash_protocol::consensus::NetworkUpgrade;

use crate::types::{ConsensusBranchId, Height, NetworkUpgradeInfo, NetworkUpgradeStatus};

/// Protocol order (`NetworkUpgrade` derives neither `Ord` nor `Hash`)
const ORDER: [NetworkUpgrade; 11] = [
    NetworkUpgrade::Overwinter,
    NetworkUpgrade::Sapling,
    NetworkUpgrade::Blossom,
    NetworkUpgrade::Heartwood,
    NetworkUpgrade::Canopy,
    NetworkUpgrade::Nu5,
    NetworkUpgrade::Nu6,
    NetworkUpgrade::Nu6_1,
    NetworkUpgrade::Nu6_2,
    NetworkUpgrade::Nu6_3,
    NetworkUpgrade::Nu7,
];

const PRE_BLOSSOM_SPACING: u32 = 150;
const POST_BLOSSOM_SPACING: u32 = 75;
/// ZIP 218
const POST_NU7_SPACING: u32 = 25;

/// Activation height per upgrade (`None` = never), indexed by `ORDER`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upgrades([Option<Height>; ORDER.len()]);

impl Upgrades {
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

    pub fn activation(&self, upgrade: NetworkUpgrade) -> Option<Height> {
        self.0[slot(upgrade)]
    }

    pub(super) fn active(&self, upgrade: NetworkUpgrade, at: Height) -> bool {
        self.activation(upgrade).is_some_and(|from| from <= at)
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

    /// Branch in force at `at` (Sprout = 0)
    pub(super) fn branch_at(&self, at: Height) -> ConsensusBranchId {
        let latest = ORDER.iter().rev().find(|upgrade| self.active(**upgrade, at));
        ConsensusBranchId::new(latest.map_or(0, |upgrade| u32::from(upgrade.branch_id())))
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

fn slot(upgrade: NetworkUpgrade) -> usize {
    ORDER.iter().position(|known| *known == upgrade).expect("ORDER lists every enabled upgrade")
}
