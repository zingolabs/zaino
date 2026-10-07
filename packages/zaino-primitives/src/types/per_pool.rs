//! One value per shielded pool.

use super::{Block, ShieldedPool, TreeSize, TreeSizeOutOfRange};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerPool<T> {
    pub sapling: T,
    pub orchard: T,
    pub ironwood: T,
}

impl<T> PerPool<T> {
    /// `f(pool)` for each pool, in [`ShieldedPool::ALL`] order
    pub fn from_fn(mut f: impl FnMut(ShieldedPool) -> T) -> Self {
        Self {
            sapling: f(ShieldedPool::Sapling),
            orchard: f(ShieldedPool::Orchard),
            ironwood: f(ShieldedPool::Ironwood),
        }
    }

    pub fn get(&self, pool: ShieldedPool) -> &T {
        match pool {
            ShieldedPool::Sapling => &self.sapling,
            ShieldedPool::Orchard => &self.orchard,
            ShieldedPool::Ironwood => &self.ironwood,
        }
    }

    pub fn get_mut(&mut self, pool: ShieldedPool) -> &mut T {
        match pool {
            ShieldedPool::Sapling => &mut self.sapling,
            ShieldedPool::Orchard => &mut self.orchard,
            ShieldedPool::Ironwood => &mut self.ironwood,
        }
    }

    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> PerPool<U> {
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
    use crate::types::{
        CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier, OrchardAction, OrchardData,
        SaplingData, SaplingOutput, Transaction, TransactionId,
    };

    #[test]
    fn from_fn_fills_each_pool_in_all_order() {
        let mut visited = Vec::new();
        let pools = PerPool::from_fn(|pool| {
            visited.push(pool);
            pool
        });
        let expected = PerPool {
            sapling: ShieldedPool::Sapling,
            orchard: ShieldedPool::Orchard,
            ironwood: ShieldedPool::Ironwood,
        };
        assert_eq!((pools, visited), (expected, ShieldedPool::ALL.to_vec()));
    }

    /// Per-pool commitment counts added onto the prior sizes; past `u32` = an error, not a wrap
    #[test]
    fn advance_adds_each_pools_commitments_and_refuses_the_u32_ceiling() {
        let output = SaplingOutput {
            cmu: NoteCommitment::from([1; 32]),
            ephemeral_key: EphemeralKey::from([2; 32]),
            enc_ciphertext: CompactCiphertext::from([3; CompactCiphertext::LENGTH]),
        };
        let action = OrchardAction {
            nullifier: Nullifier::from([4; 32]),
            cmx: NoteCommitment::from([5; 32]),
            ephemeral_key: EphemeralKey::from([6; 32]),
            enc_ciphertext: CompactCiphertext::from([7; CompactCiphertext::LENGTH]),
        };
        let tx = |tag: u8, sapling: usize, orchard: usize, ironwood: usize| Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: SaplingData { outputs: vec![output.clone(); sapling], ..Default::default() },
            orchard: OrchardData { actions: vec![action.clone(); orchard], ..Default::default() },
            ironwood: OrchardData { actions: vec![action.clone(); ironwood], ..Default::default() },
        };
        let mut chain = crate::testing::Chain::new();
        let mined = chain.mine_with(chain.genesis().hash, vec![tx(1, 2, 1, 0), tx(2, 1, 0, 3)]);
        let block: &Block = chain.block(mined.hash);

        let prior = PerPool { sapling: 10, orchard: 20, ironwood: 30 }.map(TreeSize::from);
        let expected = PerPool { sapling: 13, orchard: 21, ironwood: 33 }.map(TreeSize::from);
        assert_eq!(prior.advance(block), Ok(expected));

        let full = PerPool { sapling: u32::MAX, orchard: 0, ironwood: 0 }.map(TreeSize::from);
        assert!(full.advance(block).is_err(), "sapling past u32::MAX");
    }
}
