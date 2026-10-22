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

    /// Block of one tx per entry, each carrying that many sapling outputs / orchard and ironwood
    /// actions
    fn block_of(txs: &[PerPool<usize>]) -> Block {
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
    #[should_panic(expected = "Sapling commitment tree past 2^32 leaves")]
    fn a_tree_past_2_32_leaves_panics() {
        let full = PerPool { sapling: u32::MAX, orchard: 0, ironwood: 0 }.map(TreeSize::from);
        full.advance(&block_of(&[PerPool { sapling: 1, orchard: 0, ironwood: 0 }]));
    }

    /// Both doors refuse exactly the #549 boundary (a full depth-32 tree, `2^32`) and accept
    /// everything below it
    #[test]
    fn sizes_enter_up_to_u32_max_and_a_full_tree_is_refused_at_both_doors() {
        let full = 1_u64 << 32;

        let largest = TreeSize::try_from(u64::from(u32::MAX));
        assert_eq!(largest, Ok(TreeSize::from(u32::MAX)), "u32::MAX enters");
        assert_eq!(TreeSize::try_from(full), Err(TreeSizeOutOfRange { got: full }), "2^32 refused");

        assert_eq!(TreeSize::ZERO.checked_add(0), Ok(TreeSize::ZERO), "0 + 0");
        assert_eq!(TreeSize::from(10).checked_add(5), Ok(TreeSize::from(15)), "10 + 5");
        let past_u32 = TreeSize::from(u32::MAX).checked_add(1);
        assert_eq!(past_u32, Err(TreeSizeOutOfRange { got: full }), "refused, not wrapped to 0");
        let past_u64 = TreeSize::from(1).checked_add(u64::MAX);
        assert_eq!(past_u64, Err(TreeSizeOutOfRange { got: u64::MAX }), "names the count");
    }
}
