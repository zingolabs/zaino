//! Global block offset newtype.

/// A zero-based offset into the full sync range.
///
/// Block offset 0 is the first block provisioned, offset 1 is the
/// second, etc. This is NOT a chain height — it's a position within
/// the range being synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockOffset(u32);

impl BlockOffset {
    /// Create a block offset.
    pub const fn new(offset: u32) -> Self {
        Self(offset)
    }

    /// The raw numeric value.
    pub const fn value(&self) -> u32 {
        self.0
    }
}

impl core::fmt::Display for BlockOffset {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "offset:{}", self.0)
    }
}
