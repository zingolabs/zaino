//! Cumulative note-commitment tree size for a shielded pool.

use core::fmt;

/// Cumulative count of note commitments in a pool's commitment tree, as of a
/// given block.
///
/// One quantity, one width. The same count is reported by a validator as a
/// `u64`, carried through the domain, and finally narrowed to the `u32` the
/// proto/DB surface uses. Modelling it as a single `u64`-backed newtype moves
/// that narrowing to one checked door ([`try_to_u32`](Self::try_to_u32)) instead
/// of leaving a silent `as u32` truncation at each boundary (issue #549).
///
/// `u64` is the natural width the count is delivered in, so construction is
/// infallible: there is no bound to check beyond machine representability, which
/// a `u64` satisfies by definition.
///
/// # A relation this type does not enforce
///
/// A pool's tree only grows, so across a run of blocks on one chain the size is
/// monotonically non-decreasing, and a reorg rewinds it to the fork point. That
/// is a *cross-block* relation between successive `TreeSize` values, not an
/// invariant of a single value, so it is not encoded here. A future relation
/// over a block sequence could carry it; today it lives in prose.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TreeSize(u64);

/// Error returned when a [`TreeSize`] does not fit the `u32` proto/DB surface.
///
/// A commitment tree past `2^32 - 1` notes is impossible on today's chain and
/// would mean corruption. It is rejected rather than truncated: a silently
/// wrapped size would put a wrong treestate on the wire or on disk, which no
/// later read could detect (issue #549).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("commitment tree size {got} does not fit into u32")]
pub struct TreeSizeOverflow {
    /// The size that did not fit.
    pub got: u64,
}

impl TreeSize {
    /// The empty tree — a pool that has committed no notes.
    pub const ZERO: Self = Self(0);

    /// Wrap a cumulative note count.
    ///
    /// Infallible: `u64` is the count's natural width.
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    /// The cumulative note count as a `u64`.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Narrow to the `u32` the proto/DB surface uses, rejecting a size that does
    /// not fit rather than truncating it.
    ///
    /// The one checked door onto the narrow surface: every boundary that writes
    /// a tree size as a `u32` goes through here, so the truncation that issue
    /// #549 reported cannot happen silently.
    pub fn try_to_u32(self) -> Result<u32, TreeSizeOverflow> {
        u32::try_from(self.0).map_err(|_| TreeSizeOverflow { got: self.0 })
    }
}

impl From<u64> for TreeSize {
    fn from(count: u64) -> Self {
        Self(count)
    }
}

impl From<TreeSize> for u64 {
    fn from(size: TreeSize) -> Self {
        size.0
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

    #[test]
    fn zero_is_empty() {
        assert_eq!(TreeSize::ZERO.get(), 0);
        assert_eq!(TreeSize::default(), TreeSize::ZERO);
    }

    #[test]
    fn round_trips_u64() {
        let count = 123_456_789_u64;
        assert_eq!(u64::from(TreeSize::new(count)), count);
        assert_eq!(TreeSize::from(count).get(), count);
    }

    #[test]
    fn narrows_within_range() {
        let size = TreeSize::new(u64::from(u32::MAX));
        assert_eq!(size.try_to_u32(), Ok(u32::MAX));
    }

    #[test]
    fn narrows_zero() {
        assert_eq!(TreeSize::ZERO.try_to_u32(), Ok(0));
    }

    #[test]
    fn rejects_the_off_by_one_at_two_to_the_32() {
        // The exact #549 boundary: 2^32 is the first value a u32 cannot hold.
        let over = u64::from(u32::MAX) + 1;
        assert_eq!(TreeSize::new(over).try_to_u32(), Err(TreeSizeOverflow { got: over }));
    }

    #[test]
    fn narrows_then_widens_within_range() {
        let size = TreeSize::new(42);
        let narrowed = size.try_to_u32().expect("42 fits u32");
        assert_eq!(TreeSize::new(u64::from(narrowed)), size);
    }

    #[test]
    fn ordering_follows_count() {
        assert!(TreeSize::new(1) < TreeSize::new(2));
    }
}
