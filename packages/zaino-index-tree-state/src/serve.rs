//! What `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots` need beside the reader
//!
//! - per request: one 48 B record read, ≤ 33 node reads (32 B) per pool, ~1 KB serialized, no
//!   hashing

use zaino_primitives::types::{BlockchainInfo, ConsensusBranchId, Height, ShieldedPool};
use zcash_protocol::consensus::BranchId;

/// Height each pool's tree begins, from the validator's schedule (`None` = unscheduled)
///
/// - zebra's `z_gettreestate` omits a pool below its upgrade, lightwalletd then answers `""`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolActivations {
    pub sapling: Height,
    pub orchard: Option<Height>,
    pub ironwood: Option<Height>,
}

impl PoolActivations {
    /// Keyed by branch id: Sapling, NU5 (orchard), NU6.3 (ironwood)
    pub fn from_validator(info: &BlockchainInfo) -> Self {
        let activation = |branch: BranchId| {
            let id = ConsensusBranchId::new(u32::from(branch));
            info.upgrades.iter().find(|upgrade| upgrade.branch_id == id)
        };
        Self {
            sapling: info.sapling_activation,
            orchard: activation(BranchId::Nu5).map(|upgrade| upgrade.activation_height),
            ironwood: activation(BranchId::Nu6_3).map(|upgrade| upgrade.activation_height),
        }
    }

    /// `pool` has a tree at `at`
    pub fn active(&self, pool: ShieldedPool, at: Height) -> bool {
        let from = match pool {
            ShieldedPool::Sapling => Some(self.sapling),
            ShieldedPool::Orchard => self.orchard,
            ShieldedPool::Ironwood => self.ironwood,
        };
        from.is_some_and(|from| from <= at)
    }
}

/// Small (transport maps these onto gRPC codes; this crate names no transport)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("no tree state at height {height}")]
    NotFound { height: Height },

    /// Stored nodes that will not rebuild a frontier (a fold bug, not a bad request)
    #[error("stored tree state at height {height} is inconsistent")]
    Inconsistent { height: Height },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keyed by branch id, not list order or name: Sapling, NU5 (orchard), NU6.3 (ironwood);
    /// absent upgrade = `None`; pending = still its scheduled height
    #[test]
    fn pool_activations_come_from_the_validators_upgrade_schedule() {
        use zaino_primitives::types::{
            BlockHash, ConsensusBranchIds, NetworkUpgradeInfo, NetworkUpgradeStatus,
        };

        let h = |n: u32| Height::try_from(n).expect("h");
        let upgrade = |branch: u32, height: u32, status| NetworkUpgradeInfo {
            branch_id: ConsensusBranchId::new(branch),
            name: "label only".to_owned(),
            activation_height: h(height),
            status,
        };
        let info = |upgrades| BlockchainInfo {
            blocks: h(3_500_000),
            estimated_height: h(3_500_000),
            best_block_hash: BlockHash::from([0u8; 32]),
            sapling_activation: h(419_200),
            upgrades,
            consensus: ConsensusBranchIds {
                chain_tip: ConsensusBranchId::new(0x37a5_165b),
                next_block: ConsensusBranchId::new(0x37a5_165b),
            },
        };
        let active = NetworkUpgradeStatus::Active;

        let mainnet = info(vec![
            upgrade(0x37a5_165b, 3_428_143, active),
            upgrade(0xc8e7_1055, 2_726_400, active),
            upgrade(0x76b8_09bb, 419_200, active),
            upgrade(0xc2d6_d0b4, 1_687_104, active),
        ]);
        let expected = PoolActivations {
            sapling: h(419_200),
            orchard: Some(h(1_687_104)),
            ironwood: Some(h(3_428_143)),
        };
        assert_eq!(PoolActivations::from_validator(&mainnet), expected);

        let pending = info(vec![
            upgrade(0x76b8_09bb, 419_200, active),
            upgrade(0x37a5_165b, 4_000_000, NetworkUpgradeStatus::Pending),
        ]);
        let expected =
            PoolActivations { sapling: h(419_200), orchard: None, ironwood: Some(h(4_000_000)) };
        assert_eq!(PoolActivations::from_validator(&pending), expected, "no NU5 = never orchard");
    }
}
