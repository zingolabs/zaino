//! Batch index newtype.

/// Identifies a batch within a sync run.
///
/// Batch 0 covers blocks `[start, start + batch_size)`, batch 1 covers
/// `[start + batch_size, start + 2*batch_size)`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BatchIndex(u32);

impl BatchIndex {
    /// Create a batch index.
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// The raw numeric value.
    pub const fn value(&self) -> u32 {
        self.0
    }
}

impl core::fmt::Display for BatchIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "batch:{}", self.0)
    }
}
