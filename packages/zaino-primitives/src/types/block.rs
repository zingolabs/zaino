//! Block and block header

use super::transaction::Transaction;
use super::{
    BlockCommitments, BlockHash, BlockRef, BlockTime, CompactDifficulty, EquihashNonce,
    EquihashSolution, Height, MerkleRoot,
};

/// Every consensus field + the hash (SHA-256d of the header bytes) and height naming the block
/// (carried whole: the hash commits to all of them)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedHeader {
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

impl DecodedHeader {
    /// Next above `parent`: one height up, `prev_hash` = its hash (`None` = nothing below: genesis)
    pub fn extends(&self, parent: Option<BlockRef>) -> bool {
        match parent {
            None => self.height == Height::GENESIS,
            Some(parent) => parent.height.next() == self.height && parent.hash == self.prev_hash,
        }
    }
}

/// A body its own header does not commit to
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BodyMismatch {
    #[error("txids do not rebuild the header's merkle root")]
    MerkleRoot,
    #[error("body repeats a txid pair (CVE-2012-2459)")]
    Mutated,
}

/// Decoded from its consensus bytes: the one parse every index consumes
///
/// - Each index keeps what it needs and drops the rest (no per-consumer re-parse)
/// - Position = block order (coinbase = slot 0); no cumulative indexed state
#[derive(Debug, Clone)]
pub struct Block {
    header: DecodedHeader,
    transactions: Vec<Transaction>,
}

impl Block {
    /// Every block mines a coinbase (slot 0): an empty list = a decode bug, never data
    pub fn new(header: DecodedHeader, transactions: Vec<Transaction>) -> Self {
        assert!(!transactions.is_empty(), "block {:?} with no coinbase", header.height);
        Self { header, transactions }
    }

    pub fn header(&self) -> &DecodedHeader {
        &self.header
    }

    pub fn at(&self) -> BlockRef {
        BlockRef { hash: self.header.hash, height: self.header.height }
    }

    /// Coinbase first, never empty
    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// Its transactions = the ones its header commits to (hash matched = the body asked for)
    pub fn check_body(&self) -> Result<(), BodyMismatch> {
        let txids: Vec<_> = self.transactions.iter().map(|tx| tx.txid).collect();
        match MerkleRoot::of_txids(&txids) {
            None => Err(BodyMismatch::Mutated),
            Some(root) if root != self.header.merkle_root => Err(BodyMismatch::MerkleRoot),
            Some(_) => Ok(()),
        }
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

    /// Block 1 extends genesis and nothing else: not an empty parent, a sibling, itself, or a gap
    #[test]
    fn a_header_extends_only_its_parent_and_genesis_only_nothing() {
        use crate::testing::{h, MockChain};

        let mut chain = MockChain::regtest();
        let two = chain.mine_empty(2);
        let sibling = chain.fork(h(0)).mine_empty(1).tip();
        let (genesis, one) = (chain.genesis(), chain.at(h(1)));
        let header = |at: BlockRef| chain.block(at.hash).header().clone();
        let cases = [
            (header(genesis), None, true),
            (header(genesis), Some(genesis), false),
            (header(one), Some(genesis), true),
            (header(one), None, false),
            (header(one), Some(sibling), false),
            (header(one), Some(one), false),
            (header(two), Some(genesis), false),
            (header(two), Some(one), true),
        ];
        for (header, parent, expected) in cases {
            assert_eq!(header.extends(parent), expected, "{:?} on {parent:?}", header.height);
        }
        assert_eq!(chain.block(two.hash).at(), two);
    }

    /// Body = what its header commits to: its own passes; a txid added or reordered moves the root;
    /// a repeated last pair keeps the root (CVE-2012-2459) and is refused as mutated
    #[test]
    fn a_body_passes_only_when_its_txids_rebuild_its_headers_merkle_root() {
        let mut chain = crate::testing::MockChain::regtest();
        let mined = chain.mine(|b| b.tx(|t| t.txid([1; 32])).tx(|t| t.txid([2; 32])));
        let block = chain.block(mined.hash);
        let txs = block.transactions().to_vec();
        let with = |txs: Vec<Transaction>| Block::new(block.header().clone(), txs);

        let added = [txs.clone(), vec![txs[0].clone()]].concat();
        let reordered = vec![txs[0].clone(), txs[2].clone(), txs[1].clone()];
        let repeated = [txs.clone(), vec![txs[2].clone()]].concat();
        let cases = [
            (with(txs.clone()), Ok(())),
            (with(added), Err(BodyMismatch::MerkleRoot)),
            (with(reordered), Err(BodyMismatch::MerkleRoot)),
            (with(txs[..2].to_vec()), Err(BodyMismatch::MerkleRoot)),
            (with(repeated), Err(BodyMismatch::Mutated)),
        ];
        for (index, (body, expected)) in cases.into_iter().enumerate() {
            assert_eq!(body.check_body(), expected, "case {index}");
        }
    }

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
