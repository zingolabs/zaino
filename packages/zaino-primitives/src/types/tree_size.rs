//! Cumulative note-commitment tree size for a shielded pool.

use core::fmt;

/// Cumulative count of note commitments in a pool's commitment tree, as of a
/// given block — a size the wire/storage formats can carry.
///
/// Sapling, Orchard and Ironwood trees have depth 32, so a pool's size ranges
/// over `0..=2^32`. Every format Zaino writes a size to (the proto
/// `ChainMetadata` and the v1 database) holds it as a `u32`, which covers
/// `0..=2^32 - 1`: one value short. `TreeSize` is `u32`-backed, so the
/// invariant is the formats' range, and a size outside it is refused where it
/// enters, at the single fallible door [`TryFrom<u64>`]. A full tree
/// (exactly `2^32` notes) is reachable on a chain but is not representable
/// here; it fails loudly at ingest instead of being written as `0` (issue
/// #549). Conversions onto the `u32` formats are then infallible.
///
/// # A relation this type does not enforce
///
/// A pool's tree only grows, so across a run of blocks on one chain the size is
/// monotonically non-decreasing, and a reorg rewinds it to the fork point. That
/// is a *cross-block* relation between successive `TreeSize` values, not an
/// invariant of a single value, so it is not encoded here. A future relation
/// over a block sequence could carry it; today it lives in prose.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TreeSize(u32);

/// A reported tree size exceeds the compact protocol's `u32` range.
///
/// Raised by [`TreeSize::try_from`] on a `u64` size, which is the width
/// validators report. The only in-protocol value that trips it is a full
/// depth-32 tree (`2^32`); anything larger is a malformed report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("tree size {got} exceeds the compact protocol's u32 range")]
pub struct TreeSizeOutOfRange {
    /// The reported size.
    pub got: u64,
}

impl TreeSize {
    /// The empty tree — a pool that has committed no notes.
    pub const ZERO: Self = Self(0);

    /// The cumulative note count.
    pub const fn get(self) -> u32 {
        self.0
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
        u32::try_from(count)
            .map(Self)
            .map_err(|_| TreeSizeOutOfRange { got: count })
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

    #[test]
    fn zero_is_empty() {
        assert_eq!(TreeSize::ZERO.get(), 0);
        assert_eq!(TreeSize::default(), TreeSize::ZERO);
    }

    #[test]
    fn round_trips_u32() {
        let count = 123_456_789_u32;
        assert_eq!(u32::from(TreeSize::from(count)), count);
        assert_eq!(u64::from(TreeSize::from(count)), u64::from(count));
    }

    #[test]
    fn accepts_u32_max_from_u64() {
        let size = TreeSize::try_from(u64::from(u32::MAX));
        assert_eq!(size, Ok(TreeSize::from(u32::MAX)));
    }

    #[test]
    fn rejects_a_full_tree_from_u64() {
        // A full depth-32 tree holds 2^32 notes, the first value a u32 cannot
        // hold: the exact #549 boundary.
        let full = 1_u64 << 32;
        assert_eq!(
            TreeSize::try_from(full),
            Err(TreeSizeOutOfRange { got: full })
        );
    }

    #[test]
    fn ordering_follows_count() {
        assert!(TreeSize::from(1) < TreeSize::from(2));
    }
}
