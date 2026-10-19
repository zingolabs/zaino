//! Cumulative note-commitment tree sizes: one pool's [`TreeSize`], all pools' [`TreeSizes`]

use core::fmt;

use super::{Block, PerPool};

/// Cumulative note-commitment count of one pool's tree, as of a block
///
/// - `u32`-backed = every format's range
/// - full depth-32 tree (`2^32`) refused at [`TryFrom<u64>`] / `checked_add`, never written as `0`
///   (#549)
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

/// Cumulative commitment tree sizes after a block (a pool not yet active = size 0, not absent)
pub type TreeSizes = PerPool<TreeSize>;

impl TreeSizes {
    /// Before genesis
    pub const ZERO: Self =
        Self { sapling: TreeSize::ZERO, orchard: TreeSize::ZERO, ironwood: TreeSize::ZERO };

    /// Sizes after `block`, given these before it
    ///
    /// - Orchard/Ironwood action = spend + output, exactly one note commitment → counts once
    /// - panics past 2^32 leaves: a depth-32 tree holds no more (consensus never builds it)
    pub fn advance(self, block: &Block) -> Self {
        let mut added = PerPool::<u64>::default();
        for tx in block.transactions() {
            added.sapling += tx.sapling.outputs.len() as u64;
            added.orchard += tx.orchard.actions.len() as u64;
            added.ironwood += tx.ironwood.actions.len() as u64;
        }
        let add = |size: TreeSize, count: u64, pool: &'static str| {
            let full = |_| panic!("{pool} commitment tree past 2^32 leaves: consensus caps it");
            size.checked_add(count).unwrap_or_else(full)
        };
        Self {
            sapling: add(self.sapling, added.sapling, "sapling"),
            orchard: add(self.orchard, added.orchard, "orchard"),
            ironwood: add(self.ironwood, added.ironwood, "ironwood"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockChain;

    /// Per-pool commitment counts added onto the prior sizes
    #[test]
    fn advance_adds_each_pools_commitments() {
        let mut chain = MockChain::regtest();
        let mined = chain.mine(|b| {
            b.tx(|t| t.sapling_output(1).sapling_output(2).orchard_action([4; 32], 5)).tx(|t| {
                t.sapling_output(3)
                    .ironwood_action([6; 32], 7)
                    .ironwood_action([8; 32], 9)
                    .ironwood_action([10; 32], 11)
            })
        });
        let block: &Block = chain.block(mined.hash);

        let prior = PerPool { sapling: 10, orchard: 20, ironwood: 30 }.map(TreeSize::from);
        let expected = PerPool { sapling: 13, orchard: 21, ironwood: 33 }.map(TreeSize::from);
        assert_eq!(prior.advance(block), expected);
    }

    #[test]
    #[should_panic(expected = "sapling commitment tree past 2^32 leaves")]
    fn a_tree_past_2_32_leaves_panics() {
        let mut chain = MockChain::regtest();
        let mined = chain.mine(|b| b.tx(|t| t.sapling_output(1)));
        let full = PerPool { sapling: u32::MAX, orchard: 0, ironwood: 0 }.map(TreeSize::from);
        full.advance(chain.block(mined.hash));
    }

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
