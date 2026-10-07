//! Cumulative note-commitment tree size for a shielded pool.

use core::fmt;

/// Cumulative note-commitment count of one pool's tree, as of a block
///
/// - `u32`-backed = every format's range; a full depth-32 tree (`2^32`) refused at
///   [`TryFrom<u64>`] / [`checked_add`](Self::checked_add), never written as `0` (#549)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TreeSize(u32);

/// Size past the `u32` formats (only in-protocol cause: a full depth-32 tree)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("tree size {got} exceeds the compact protocol's u32 range")]
pub struct TreeSizeOutOfRange {
    pub got: u64,
}

impl TreeSize {
    pub(crate) const ZERO: Self = Self(0);

    pub const fn get(self) -> u32 {
        self.0
    }

    /// Size after one block's `count` new commitments
    pub(crate) fn checked_add(self, count: u64) -> Result<Self, TreeSizeOutOfRange> {
        let total =
            u64::from(self.0).checked_add(count).ok_or(TreeSizeOutOfRange { got: count })?;
        Self::try_from(total)
    }
}

impl From<u32> for TreeSize {
    fn from(count: u32) -> Self {
        Self(count)
    }
}

impl TryFrom<u64> for TreeSize {
    type Error = TreeSizeOutOfRange;

    fn try_from(count: u64) -> Result<Self, Self::Error> {
        u32::try_from(count).map(Self).map_err(|_| TreeSizeOutOfRange { got: count })
    }
}

impl From<TreeSize> for u32 {
    fn from(size: TreeSize) -> Self {
        size.0
    }
}

impl From<TreeSize> for u64 {
    fn from(size: TreeSize) -> Self {
        u64::from(size.0)
    }
}

impl fmt::Debug for TreeSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TreeSize({})", self.0)
    }
}

impl fmt::Display for TreeSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both doors refuse exactly the #549 boundary (a full depth-32 tree, `2^32`) and accept
    /// everything below it
    #[test]
    fn sizes_enter_up_to_u32_max_and_a_full_tree_is_refused_at_both_doors() {
        let full = 1_u64 << 32;

        assert_eq!(TreeSize::default(), TreeSize::ZERO);
        assert_eq!(TreeSize::try_from(u64::from(u32::MAX)), Ok(TreeSize::from(u32::MAX)));
        assert_eq!(TreeSize::try_from(full), Err(TreeSizeOutOfRange { got: full }));

        assert_eq!(TreeSize::ZERO.checked_add(0), Ok(TreeSize::ZERO));
        assert_eq!(TreeSize::from(10).checked_add(5), Ok(TreeSize::from(15)));
        let past_u32 = TreeSize::from(u32::MAX).checked_add(1);
        assert_eq!(past_u32, Err(TreeSizeOutOfRange { got: full }), "refused, not wrapped to 0");
        let past_u64 = TreeSize::from(1).checked_add(u64::MAX);
        assert_eq!(past_u64, Err(TreeSizeOutOfRange { got: u64::MAX }), "names the count");
    }
}
