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

impl core::fmt::Display for ShieldedPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sapling => write!(f, "sapling"),
            Self::Orchard => write!(f, "orchard"),
            Self::Ironwood => write!(f, "ironwood"),
        }
    }
}
