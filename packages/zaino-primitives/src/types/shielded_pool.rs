//! Zcash shielded pool identifier

/// `Ironwood` activates at NU6.3
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShieldedPool {
    Sapling,
    Orchard,
    Ironwood,
}

impl ShieldedPool {
    /// Activation order; the one enumeration (per-pool filters / folds iterate this)
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
