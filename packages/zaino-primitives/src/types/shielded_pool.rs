//! Zcash shielded pool identifier.

/// Which shielded pool a query targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShieldedPool {
    /// Sapling shielded pool.
    Sapling,
    /// Orchard shielded pool.
    Orchard,
    /// Ironwood shielded pool (activates at NU6.3).
    Ironwood,
}

impl ShieldedPool {
    /// Every shielded pool, in activation order.
    ///
    /// The single place the set is enumerated. A consumer that needs to act on
    /// all of them — a filter over pools, a per-pool fold — iterates this
    /// rather than listing the variants again, so adding a pool is one edit
    /// here rather than one in every such consumer.
    pub const ALL: [Self; 3] = [Self::Sapling, Self::Orchard, Self::Ironwood];
}

impl core::fmt::Display for ShieldedPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sapling => write!(f, "sapling"),
            Self::Orchard => write!(f, "orchard"),
            Self::Ironwood => write!(f, "ironwood"),
        }
    }
}
