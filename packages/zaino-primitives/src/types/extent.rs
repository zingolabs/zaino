//! A contiguous run of blocks from genesis, and the reorg depth that splits it

use core::fmt;
use core::num::NonZeroU32;

use super::{Height, HeightOverflow};

/// Blocks `0..n`: `n` = the next height (`0` = empty, up to one past the protocol maximum)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Extent(u32);

impl Extent {
    pub const ZERO: Self = Self(0);

    /// Blocks `0..=last`
    pub fn through(last: Height) -> Self {
        Self(u32::from(last) + 1)
    }

    /// Blocks `0..next`
    pub fn before(next: Height) -> Self {
        Self(u32::from(next))
    }

    /// Highest height held (`None` = empty)
    pub fn last(self) -> Option<Height> {
        self.0
            .checked_sub(1)
            .map(|last| Height::try_from(last).expect("extent within the height range"))
    }

    /// The height one past the run (panics past the protocol maximum: no chain gets there)
    pub fn next(self) -> Height {
        Height::try_from(self.0).expect("extent one past the protocol height maximum")
    }

    pub fn contains(self, height: Height) -> bool {
        u32::from(height) < self.0
    }

    /// An index's own block count (built from block heights: past the maximum = a broken index)
    pub fn counted(count: u64) -> Self {
        Self::from_count(count).expect("block count past the protocol height maximum")
    }

    /// Disk boundary (a watermark read back as a count)
    pub fn from_count(count: u64) -> Result<Self, HeightOverflow> {
        let as_u32 = u32::try_from(count).map_err(|_| HeightOverflow { got: u32::MAX })?;
        match as_u32 {
            0 => Ok(Self::ZERO),
            n => Height::try_from(n - 1).map(Self::through),
        }
    }
}

impl From<Extent> for u32 {
    fn from(extent: Extent) -> Self {
        extent.0
    }
}

impl From<Extent> for u64 {
    fn from(extent: Extent) -> Self {
        u64::from(extent.0)
    }
}

impl fmt::Debug for Extent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Extent(0..{})", self.0)
    }
}

impl fmt::Display for Extent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Blocks below the tip kept reorg-able (non-final); consensus bound = 1000
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReorgDepth(NonZeroU32);

impl ReorgDepth {
    pub const CONSENSUS: Self =
        Self(NonZeroU32::new(crate::protocol::MAX_BLOCK_REORG_HEIGHT).expect("1000 is non-zero"));

    pub const fn new(depth: NonZeroU32) -> Self {
        Self(depth)
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Final blocks under `tip`: `0..tip + 1 − depth` (empty while the chain is shallower)
    pub fn final_extent(self, tip: Height) -> Extent {
        Extent((u32::from(tip) + 1).saturating_sub(self.get()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extents bound heights exclusively, round-trip their last height, clamp the final boundary
    /// at genesis, and reject a disk count past the height maximum
    #[test]
    fn extents_count_blocks_and_depth_splits_final_from_reorgable() {
        let h = |n: u32| Height::try_from(n).expect("h");
        let three = Extent::through(h(2));
        assert_eq!((three.last(), three.next()), (Some(h(2)), h(3)));
        assert!(three.contains(h(2)) && !three.contains(h(3)));
        assert_eq!((Extent::ZERO.last(), Extent::ZERO.next()), (None, h(0)));
        assert_eq!(Extent::before(h(3)), three);

        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("nz"));
        assert_eq!(depth.final_extent(h(10)), Extent::through(h(7)));
        assert_eq!(depth.final_extent(h(1)), Extent::ZERO);

        assert_eq!(Extent::from_count(3), Ok(three));
        assert_eq!(Extent::from_count(0), Ok(Extent::ZERO));
        assert!(Extent::from_count(1 << 31).is_ok());
        assert!(Extent::from_count((1 << 31) + 1).is_err());
    }
}
