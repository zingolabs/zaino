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
    pub fn from_fn(mut f: impl FnMut(ShieldedPool) -> T) -> Self {
        let [sapling, orchard, ironwood] = ShieldedPool::ALL.map(&mut f);
        Self { sapling, orchard, ironwood }
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

    /// [`ShieldedPool::ALL`] order
    pub fn iter(&self) -> impl Iterator<Item = (ShieldedPool, &T)> {
        ShieldedPool::ALL.into_iter().map(|pool| (pool, self.get(pool)))
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
    /// - panics past 2^32 leaves: a depth-32 tree holds no more (consensus never builds it)
    pub fn advance(self, block: &Block) -> Self {
        Self::from_fn(|pool| {
            let txs = block.transactions().iter();
            let added = txs.map(|tx| tx.note_commitments(pool).count() as u64).sum();
            let full = |_| panic!("{pool:?} commitment tree past 2^32 leaves: consensus caps it");
            self.get(pool).checked_add(added).unwrap_or_else(full)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        BlockCommitments, CompactCiphertext, CompactDifficulty, DecodedHeader, EphemeralKey,
        EquihashSolution, MerkleRoot, NoteCommitment, Nullifier, OrchardAction, OrchardData,
        SaplingData, SaplingOutput, Transaction,
    };
    use zcash_protocol::{BlockHash, TxId};

    /// A block of txs carrying `(sapling outputs, orchard actions, ironwood actions)` each
    fn block_of(txs: &[(usize, usize, usize)]) -> Block {
        let output = SaplingOutput {
            cmu: NoteCommitment([0; 32]),
            ephemeral_key: EphemeralKey([0; 32]),
            enc_ciphertext: CompactCiphertext([0; CompactCiphertext::LENGTH]),
        };
        let action = OrchardAction {
            nullifier: Nullifier([0; 32]),
            cmx: NoteCommitment([0; 32]),
            ephemeral_key: EphemeralKey([0; 32]),
            enc_ciphertext: CompactCiphertext([0; CompactCiphertext::LENGTH]),
        };
        let tx = |(sapling, orchard, ironwood): &(usize, usize, usize)| Transaction {
            txid: TxId::from_bytes([0; 32]),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: SaplingData { outputs: vec![output.clone(); *sapling], ..Default::default() },
            orchard: OrchardData { actions: vec![action.clone(); *orchard], ..Default::default() },
            ironwood: OrchardData {
                actions: vec![action.clone(); *ironwood],
                ..Default::default()
            },
        };
        let header = DecodedHeader {
            hash: BlockHash([0; 32]),
            version: 4,
            prev_hash: BlockHash([0; 32]),
            height: 0.into(),
            time: 0,
            merkle_root: MerkleRoot([0; 32]),
            block_commitments: BlockCommitments([0; 32]),
            bits: CompactDifficulty::try_from_bits(0x200f_0f0f).expect("regtest nBits"),
            nonce: [0; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        };
        Block::new(header, txs.iter().map(tx).collect())
    }

    /// Per-pool commitment counts added onto the prior sizes
    #[test]
    fn advance_adds_each_pools_commitments() {
        let block = block_of(&[(0, 0, 0), (2, 1, 0), (1, 0, 3)]);

        let prior = PerPool { sapling: 10, orchard: 20, ironwood: 30 }.map(TreeSize::from);
        let expected = PerPool { sapling: 13, orchard: 21, ironwood: 33 }.map(TreeSize::from);
        assert_eq!(prior.advance(&block), expected);
    }

    #[test]
    #[should_panic(expected = "Sapling commitment tree past 2^32 leaves")]
    fn a_tree_past_2_32_leaves_panics() {
        let full = PerPool { sapling: u32::MAX, orchard: 0, ironwood: 0 }.map(TreeSize::from);
        full.advance(&block_of(&[(0, 0, 0), (1, 0, 0)]));
    }
}
