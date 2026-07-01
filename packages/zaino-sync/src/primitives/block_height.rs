//! Block height newtype.
//!
//! NOTE: zaino-state defines its own `BlockHeight`. Long-term, shared
//! primitives like this should live in a single zero-dependency crate
//! (e.g. `zaino-primitives`) that both zaino-sync and zaino-state depend
//! on. The sync engine's height needs are likely a subset of the domain
//! type's (Ord + Copy + Display), so unification should be
//! straightforward. Until then, this local definition avoids coupling
//! zaino-sync to zaino-state.

/// A block height on the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHeight(u64);

impl BlockHeight {
    /// Create a block height.
    pub const fn new(height: u64) -> Self {
        Self(height)
    }

    /// The raw numeric value.
    pub const fn value(&self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for BlockHeight {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}
