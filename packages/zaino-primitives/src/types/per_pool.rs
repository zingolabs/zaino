//! One value per shielded pool

use super::{Block, TreeSize};
use zcash_protocol::ShieldedPool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerPool<T> {
    pub sapling: T,
    pub orchard: T,
    pub ironwood: T,
}

impl<T> PerPool<T> {
    pub fn get(&self, pool: ShieldedPool) -> &T {
        match pool {
            ShieldedPool::Sapling => &self.sapling,
            ShieldedPool::Orchard => &self.orchard,
            ShieldedPool::Ironwood => &self.ironwood,
        }
    }

    #[cfg(test)]
    pub(crate) fn map<U>(self, mut f: impl FnMut(T) -> U) -> PerPool<U> {
        PerPool { sapling: f(self.sapling), orchard: f(self.orchard), ironwood: f(self.ironwood) }
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
}
