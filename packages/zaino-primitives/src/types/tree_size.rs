//! Cumulative note-commitment tree sizes: one pool's [`TreeSize`], all pools' [`TreeSizes`]

use core::fmt;

use super::{Block, PerPool};

/// Cumulative note-commitment count of one pool's tree, as of a block
///
/// - `u32`-backed = every format's range
/// - full depth-32 tree (`2^32`) panics in [`TreeSizes::advance`], never written as `0` (#549)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TreeSize(u32);

impl TreeSize {
    pub(crate) const ZERO: Self = Self(0);

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl From<u32> for TreeSize {
    fn from(count: u32) -> Self {
        Self(count)
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
    /// - panics past 2^32 leaves: a depth-32 tree holds no more (consensus never builds it)
    pub fn advance(self, block: &Block) -> Self {
        Self::from_fn(|pool| {
            let txs = block.transactions().iter();
            let added: usize = txs.map(|tx| tx.note_commitments(pool).count()).sum();
            let added = u64::try_from(added).expect("usize <= u64 (64-bit targets only)");
            let total = u64::from(*self.get(pool)).saturating_add(added);
            let full = |_| {
                panic!("{pool:?} commitment tree past 2^32 leaves at {total}: consensus caps it")
            };
            TreeSize(u32::try_from(total).unwrap_or_else(full))
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

    /// Block of one tx per entry, each carrying that many sapling outputs / orchard and ironwood
    /// actions
    fn block_of(txs: &[PerPool<usize>]) -> Block {
        let output = SaplingOutput {
            cmu: NoteCommitment([0; 32]),
            ephemeral_key: EphemeralKey([0; 32]),
            enc_ciphertext: CompactCiphertext([0; 52]),
        };
        let action = OrchardAction {
            nullifier: Nullifier([0; 32]),
            cmx: NoteCommitment([0; 32]),
            ephemeral_key: EphemeralKey([0; 32]),
            enc_ciphertext: CompactCiphertext([0; 52]),
        };
        let tx = |PerPool { sapling, orchard, ironwood }: &PerPool<usize>| Transaction {
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
        let block = block_of(&[
            PerPool { sapling: 0, orchard: 0, ironwood: 0 },
            PerPool { sapling: 2, orchard: 1, ironwood: 0 },
            PerPool { sapling: 1, orchard: 0, ironwood: 3 },
        ]);

        let prior = PerPool { sapling: 10, orchard: 20, ironwood: 30 }.map(TreeSize::from);
        let expected = PerPool { sapling: 13, orchard: 21, ironwood: 33 }.map(TreeSize::from);
        assert_eq!(prior.advance(&block), expected, "prior sizes + the block's commitments");
    }

    #[test]
    #[should_panic(expected = "Sapling commitment tree past 2^32 leaves at 4294967296")]
    fn a_tree_past_2_32_leaves_panics() {
        let full = PerPool { sapling: u32::MAX, orchard: 0, ironwood: 0 }.map(TreeSize::from);
        full.advance(&block_of(&[PerPool { sapling: 1, orchard: 0, ironwood: 0 }]));
    }

    /// #549 boundary: `u32::MAX` leaves held, not refused or wrapped to 0
    #[test]
    fn a_tree_fills_to_u32_max() {
        let nearly = PerPool { sapling: u32::MAX - 1, orchard: 0, ironwood: 0 }.map(TreeSize::from);
        let filled = nearly.advance(&block_of(&[PerPool { sapling: 1, orchard: 0, ironwood: 0 }]));
        assert_eq!(filled.sapling, TreeSize::from(u32::MAX));
    }
}
