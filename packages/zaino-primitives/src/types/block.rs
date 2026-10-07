//! Block and block header

use super::transaction::Transaction;
use super::{
    BlockCommitments, BlockHash, BlockTime, CompactDifficulty, EquihashNonce, EquihashSolution,
    Height, MerkleRoot,
};

/// Every consensus field + the hash (SHA-256d of the header bytes) and height naming the block
/// (carried whole: the hash commits to all of them)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHeader {
    pub hash: BlockHash,
    pub version: u32,
    pub prev_hash: BlockHash,
    pub height: Height,
    pub time: BlockTime,
    pub merkle_root: MerkleRoot,
    pub block_commitments: BlockCommitments,
    pub bits: CompactDifficulty,
    pub nonce: EquihashNonce,
    pub solution: EquihashSolution,
}

/// Decoded from its consensus bytes: the one parse every index consumes
///
/// - Each index keeps what it needs and drops the rest (no per-consumer re-parse)
/// - Position = block order (coinbase = slot 0); no cumulative indexed state
#[derive(Debug, Clone)]
pub struct Block {
    header: BlockHeader,
    transactions: Vec<Transaction>,
}

impl Block {
    /// Every block mines a coinbase (slot 0): an empty list = a decode bug, never data
    pub fn new(header: BlockHeader, transactions: Vec<Transaction>) -> Self {
        assert!(!transactions.is_empty(), "block {:?} with no coinbase", header.height);
        Self { header, transactions }
    }

    pub fn header(&self) -> &BlockHeader {
        &self.header
    }

    /// Coinbase first, never empty
    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// Bytes held in memory: inline + every allocation's capacity (allocator overhead excluded)
    pub fn footprint(&self) -> usize {
        size_of::<Self>()
            + size_of::<Transaction>() * self.transactions.capacity()
            + self.transactions.iter().map(Transaction::heap_size).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        transaction::{
            OrchardAction, OrchardData, OutPoint, SaplingData, SaplingOutput, TransparentData,
            TransparentOutput,
        },
        CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier, Script, TransactionId,
        Zatoshis,
    };

    /// Every allocation counted once at its capacity: transactions, each pool's vector, scripts
    #[test]
    fn footprint_counts_every_allocation_at_capacity() {
        let action = OrchardAction {
            nullifier: Nullifier::from([1; 32]),
            cmx: NoteCommitment::from([2; 32]),
            ephemeral_key: EphemeralKey::from([3; 32]),
            enc_ciphertext: CompactCiphertext::from([4; CompactCiphertext::LENGTH]),
        };
        let mut script = Vec::with_capacity(40);
        script.extend_from_slice(&[0x76; 25]);
        let tx = Transaction {
            txid: TransactionId::from([9; 32]),
            transparent: TransparentData {
                coinbase: false,
                inputs: vec![OutPoint { txid: TransactionId::from([8; 32]), vout: 0 }; 2],
                outputs: vec![TransparentOutput {
                    value: Zatoshis::new(5).expect("in supply"),
                    script: Script::new(script),
                }],
            },
            sprout: Default::default(),
            sapling: SaplingData { outputs: Vec::with_capacity(4), ..SaplingData::default() },
            orchard: OrchardData { actions: vec![action.clone(); 3], ..OrchardData::default() },
            ironwood: OrchardData { actions: vec![action], ..OrchardData::default() },
        };
        let chain = crate::testing::MockChain::regtest();
        let header = chain.block(chain.genesis().hash).header().clone();
        let block = Block::new(header, vec![tx]);

        let expected = size_of::<Block>()
            + size_of::<Transaction>()
            + 2 * size_of::<OutPoint>()
            + size_of::<TransparentOutput>()
            + 40
            + 4 * size_of::<SaplingOutput>()
            + 4 * size_of::<OrchardAction>();
        assert_eq!(block.footprint(), expected);
    }
}
