//! What `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots` need beside the reader
//!
//! - per request: one 48 B record read, ≤ 33 node reads (32 B) per pool, ~1 KB serialized, no
//!   hashing

use zaino_primitives::types::PerPool;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};
use zcash_protocol::local_consensus::LocalNetwork;

/// Height each pool's tree begins: Sapling, NU5 (orchard), NU6.3 (ironwood), `None` = unscheduled
///
/// - zebra's `z_gettreestate` omits a pool below its upgrade (lightwalletd then answers `""`)
pub fn pool_activations(schedule: &LocalNetwork) -> PerPool<Option<BlockHeight>> {
    PerPool {
        sapling: schedule.activation_height(NetworkUpgrade::Sapling),
        orchard: schedule.activation_height(NetworkUpgrade::Nu5),
        ironwood: schedule.activation_height(NetworkUpgrade::Nu6_3),
    }
}

/// Small (transport maps these onto gRPC codes)
///
/// - no transport named in this crate
/// - `Inconsistent` = stored nodes rebuild no frontier (a fold bug, not a bad request)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("no tree state at height {height}")]
    NotFound { height: BlockHeight },

    #[error("stored tree state at height {height} is inconsistent")]
    Inconsistent { height: BlockHeight },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - Sapling, NU5 (orchard), NU6.3 (ironwood) from the schedule
    /// - unscheduled pool = `None`
    #[test]
    fn pool_activations_are_the_schedules_pool_upgrades() {
        use zaino_primitives::protocol::compiled_schedule;
        use zaino_source::testing::{all_at, h};
        use zcash_protocol::consensus::NetworkType;

        let mainnet = compiled_schedule(NetworkType::Main).expect("mainnet compiled in");
        let expected = PerPool {
            sapling: Some(h(419_200)),
            orchard: Some(h(1_687_104)),
            ironwood: Some(h(3_428_143)),
        };
        assert_eq!(pool_activations(&mainnet), expected);

        let no_nu5 = LocalNetwork { nu5: None, ..all_at(h(1)) };
        let expected = PerPool { sapling: Some(h(1)), orchard: None, ironwood: Some(h(1)) };
        assert_eq!(pool_activations(&no_nu5), expected, "no NU5 = never orchard");
    }
}
