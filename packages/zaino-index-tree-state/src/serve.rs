//! What `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots` need beside the reader
//!
//! - per request: one 48 B record read, ≤ 33 node reads (32 B) per pool, ~1 KB serialized, no
//!   hashing

use zaino_primitives::types::{Height, ShieldedPool, UpgradeSchedule};
use zcash_protocol::consensus::NetworkUpgrade;

/// Height each pool's tree begins (`None` = unscheduled)
///
/// - zebra's `z_gettreestate` omits a pool below its upgrade, lightwalletd then answers `""`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolActivations {
    pub sapling: Height,
    pub orchard: Option<Height>,
    pub ironwood: Option<Height>,
}

impl PoolActivations {
    /// Sapling, NU5 (orchard), NU6.3 (ironwood)
    pub fn new(schedule: &UpgradeSchedule) -> Self {
        Self {
            sapling: schedule
                .activation(NetworkUpgrade::Sapling)
                .expect("every schedule activates Sapling (compiled tables, getblockchaininfo)"),
            orchard: schedule.activation(NetworkUpgrade::Nu5),
            ironwood: schedule.activation(NetworkUpgrade::Nu6_3),
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
///
/// - `Inconsistent` = stored nodes rebuild no frontier (a fold bug, not a bad request)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("no tree state at height {height}")]
    NotFound { height: Height },

    #[error("stored tree state at height {height} is inconsistent")]
    Inconsistent { height: Height },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sapling, NU5 (orchard), NU6.3 (ironwood) from the schedule; an unscheduled pool = `None`
    #[test]
    fn pool_activations_are_the_schedules_pool_upgrades() {
        use zaino_primitives::testing::h;

        let mainnet = UpgradeSchedule::compiled(zcash_protocol::consensus::NetworkType::Main)
            .expect("mainnet compiled in");
        let expected = PoolActivations {
            sapling: h(419_200),
            orchard: Some(h(1_687_104)),
            ironwood: Some(h(3_428_143)),
        };
        assert_eq!(PoolActivations::new(&mainnet), expected);

        let no_nu5 = UpgradeSchedule::all_at(h(1)).without(NetworkUpgrade::Nu5);
        let expected = PoolActivations { sapling: h(1), orchard: None, ironwood: Some(h(1)) };
        assert_eq!(PoolActivations::new(&no_nu5), expected, "no NU5 = never orchard");
    }
}
