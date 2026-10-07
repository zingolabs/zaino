//! One value per shielded pool

use super::{Block, ShieldedPool, TreeSize, TreeSizeOutOfRange};

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
    pub fn advance(self, block: &Block) -> Result<Self, TreeSizeOutOfRange> {
        let mut added = PerPool::<u64>::default();
        for tx in block.transactions() {
            added.sapling += tx.sapling.outputs.len() as u64;
            added.orchard += tx.orchard.actions.len() as u64;
            added.ironwood += tx.ironwood.actions.len() as u64;
        }
        Ok(Self {
            sapling: self.sapling.checked_add(added.sapling)?,
            orchard: self.orchard.checked_add(added.orchard)?,
            ironwood: self.ironwood.checked_add(added.ironwood)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockChain;

    /// Per-pool commitment counts added onto the prior sizes; past `u32` = an error, not a wrap
    #[test]
    fn advance_adds_each_pools_commitments_and_refuses_the_u32_ceiling() {
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
        assert_eq!(prior.advance(block), Ok(expected));

        let full = PerPool { sapling: u32::MAX, orchard: 0, ironwood: 0 }.map(TreeSize::from);
        assert!(full.advance(block).is_err(), "sapling past u32::MAX");
    }
}
