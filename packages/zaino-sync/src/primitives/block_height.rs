//! Block height newtype.

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
