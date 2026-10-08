//! Per-pool activation heights, learned from the validator's upgrade schedule.

use super::{Height, ShieldedPool};

/// Consensus branch id of the upgrade that activates the Sapling pool.
pub const SAPLING_BRANCH_ID: u32 = 0x76b8_09bb;
/// Consensus branch id of NU5, which activates the Orchard pool.
pub const NU5_BRANCH_ID: u32 = 0xc2d6_d0b4;
/// Consensus branch id of NU6.3, which activates the Ironwood pool.
pub const NU6_3_BRANCH_ID: u32 = 0x37a5_165b;

/// The height each shielded pool activates at on the chain being served.
///
/// `None` for a pool whose upgrade is not on this network's schedule — it never
/// activates here (regtest without NU6.3, say), so it is never reported. Zaino
/// carries no compiled-in schedule; this is built from the validator's reported
/// upgrades at boot, so an indexer and its validator cannot disagree about where
/// a pool turns on. It decides the one thing a tree size cannot: a pool that is
/// active but still empty (from its activation height up to its first note) must
/// serve the empty tree, not be reported absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolActivations {
    /// Sapling activation height.
    pub sapling: Option<Height>,
    /// Orchard (NU5) activation height.
    pub orchard: Option<Height>,
    /// Ironwood (NU6.3) activation height.
    pub ironwood: Option<Height>,
}

impl PoolActivations {
    /// Nothing known — every pool unscheduled. The conservative value a reader
    /// that does no treestate serving falls back to; a serving deployment builds
    /// the real schedule from the validator at boot.
    pub const fn unknown() -> Self {
        Self {
            sapling: None,
            orchard: None,
            ironwood: None,
        }
    }

    /// Build from the validator's reported upgrade schedule: each `(branch id,
    /// activation height)` pair selects the pool its upgrade activates (by
    /// consensus branch id, never by name); upgrades that activate no pool are
    /// ignored.
    pub fn from_branch_activations(entries: impl IntoIterator<Item = (u32, Height)>) -> Self {
        let mut activations = Self::unknown();
        for (branch_id, height) in entries {
            match branch_id {
                SAPLING_BRANCH_ID => activations.sapling = Some(height),
                NU5_BRANCH_ID => activations.orchard = Some(height),
                NU6_3_BRANCH_ID => activations.ironwood = Some(height),
                _ => {}
            }
        }
        activations
    }

    /// The activation height of `pool`, if it is scheduled on this network.
    pub fn activation(&self, pool: ShieldedPool) -> Option<Height> {
        match pool {
            ShieldedPool::Sapling => self.sapling,
            ShieldedPool::Orchard => self.orchard,
            ShieldedPool::Ironwood => self.ironwood,
        }
    }

    /// Whether `pool` is active at `height`: scheduled, with an activation at or
    /// below `height`.
    pub fn is_active(&self, pool: ShieldedPool, height: Height) -> bool {
        self.activation(pool)
            .is_some_and(|activation| height >= activation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(height: u32) -> Height {
        Height::try_from(height).expect("valid height")
    }

    #[test]
    fn branch_ids_select_their_pools() {
        let activations = PoolActivations::from_branch_activations([
            (SAPLING_BRANCH_ID, h(419_200)),
            (NU5_BRANCH_ID, h(1_687_104)),
            (NU6_3_BRANCH_ID, h(3_000_000)),
            // An upgrade that activates no shielded pool is ignored.
            (0x5ba8_1b19, h(347_500)),
        ]);
        assert_eq!(activations.sapling, Some(h(419_200)));
        assert_eq!(activations.orchard, Some(h(1_687_104)));
        assert_eq!(activations.ironwood, Some(h(3_000_000)));
    }

    #[test]
    fn an_unscheduled_pool_is_never_active() {
        let activations = PoolActivations::unknown();
        assert!(!activations.is_active(ShieldedPool::Ironwood, h(9_000_000)));
        assert_eq!(activations.activation(ShieldedPool::Ironwood), None);
    }

    #[test]
    fn is_active_is_inclusive_of_the_activation_height() {
        let activations =
            PoolActivations::from_branch_activations([(SAPLING_BRANCH_ID, h(419_200))]);
        assert!(!activations.is_active(ShieldedPool::Sapling, h(419_199)));
        assert!(activations.is_active(ShieldedPool::Sapling, h(419_200)));
        assert!(activations.is_active(ShieldedPool::Sapling, h(419_201)));
    }
}
