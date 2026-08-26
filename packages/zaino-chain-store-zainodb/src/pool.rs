//! Which shielded pool a piece of block data belongs to.
//!
//! # Temporary
//!
//! Duplicates `zaino_primitives::types::ShieldedPool`, which carries the same
//! three variants and none of the activation helpers below. The two collapse
//! once the store's reads are expressed in domain types; until then this one
//! stays because the activation heights it resolves are what the block builder
//! branches on.

/// The available shielded pools
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShieldedPool {
    /// Sapling
    Sapling,
    /// Orchard
    Orchard,
    /// Ironwood
    Ironwood,
}

impl ShieldedPool {
    /// This pool, as `zaino-primitives` names it.
    ///
    /// Two enums with the same variants, kept apart because this one carries
    /// activation semantics — which network upgrade turns the pool on — and
    /// that needs a consensus-parameter dependency a vocabulary crate must not
    /// have. The conversion lives here rather than at a call site so the match
    /// is exhaustive in the crate that defines the type: adding a pool then
    /// fails to compile here instead of falling through a wildcard somewhere
    /// else.
    pub fn to_domain(self) -> zaino_primitives::types::ShieldedPool {
        match self {
            ShieldedPool::Sapling => zaino_primitives::types::ShieldedPool::Sapling,
            ShieldedPool::Orchard => zaino_primitives::types::ShieldedPool::Orchard,
            ShieldedPool::Ironwood => zaino_primitives::types::ShieldedPool::Ironwood,
        }
    }

    /// The network upgrade that activates this pool.
    pub(crate) fn activation_upgrade(&self) -> zebra_chain::parameters::NetworkUpgrade {
        match self {
            ShieldedPool::Sapling => zebra_chain::parameters::NetworkUpgrade::Sapling,
            ShieldedPool::Orchard => zebra_chain::parameters::NetworkUpgrade::Nu5,
            ShieldedPool::Ironwood => zebra_chain::parameters::NetworkUpgrade::Nu6_3,
        }
    }

    /// [`ShieldedPool::activation_upgrade`] in `zcash_protocol` terms, for call sites
    /// gated through [`zcash_protocol::consensus::Parameters`].
    pub(crate) fn zcash_protocol_activation_upgrade(
        &self,
    ) -> zcash_protocol::consensus::NetworkUpgrade {
        match self {
            ShieldedPool::Sapling => zcash_protocol::consensus::NetworkUpgrade::Sapling,
            ShieldedPool::Orchard => zcash_protocol::consensus::NetworkUpgrade::Nu5,
            ShieldedPool::Ironwood => zcash_protocol::consensus::NetworkUpgrade::Nu6_3,
        }
    }

    /// Returns the string representative of the given pool.
    ///
    /// Used for display purposes and in converting the strongly types `PoolType`
    /// struct into the string that the Zcash RPCs require as input.
    pub fn pool_string(&self) -> String {
        match self {
            ShieldedPool::Sapling => "sapling".to_string(),
            ShieldedPool::Orchard => "orchard".to_string(),
            ShieldedPool::Ironwood => "ironwood".to_string(),
        }
    }
}
