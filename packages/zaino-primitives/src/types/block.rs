use super::transaction::Transaction;
use super::{BlockCommitments, BlockRef, CompactDifficulty, EquihashSolution, MerkleRoot};
use zcash_protocol::consensus::{BlockHeight, H0};
use zcash_protocol::BlockHash;

/// Every consensus field + the hash (SHA-256d of the header bytes) and height naming the block
/// (carried whole: the hash commits to all of them)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedHeader {
    pub hash: BlockHash,
    pub version: u32,
    pub prev_hash: BlockHash,
    pub height: BlockHeight,
    pub time: u32,
    pub merkle_root: MerkleRoot,
    pub block_commitments: BlockCommitments,
    pub bits: CompactDifficulty,
    pub nonce: [u8; 32],
    pub solution: EquihashSolution,
}

impl DecodedHeader {
    /// Next above `parent`: one height up, `prev_hash` = its hash (`None` = nothing below: genesis)
    pub fn extends(&self, parent: Option<BlockRef>) -> bool {
        match parent {
            None => self.height == H0,
            Some(parent) => parent.height + 1 == self.height && parent.hash == self.prev_hash,
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
/// - Position = block order (coinbase = slot 0)
/// - No cumulative indexed state
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

    pub fn block_ref(&self) -> BlockRef {
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
            OrchardAction, OrchardData, SaplingData, SaplingOutput, TransparentData,
            TransparentOutput,
        },
        CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier,
    };
    use zcash_protocol::value::Zatoshis;
    use zcash_protocol::TxId;
    use zcash_transparent::address::Script;
    use zcash_transparent::bundle::OutPoint;

    /// `(height, hash, prev_hash)`, every other field zero
    fn header(height: u32, hash: u8, prev: u8) -> DecodedHeader {
        DecodedHeader {
            hash: BlockHash([hash; 32]),
            version: 4,
            prev_hash: BlockHash([prev; 32]),
            height: BlockHeight::from(height),
            time: 0,
            merkle_root: MerkleRoot([0; 32]),
            block_commitments: BlockCommitments([0; 32]),
            bits: CompactDifficulty::try_from_bits(0x200f_0f0f).expect("regtest nBits"),
            nonce: [0; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        }
    }

    fn tx(txid: u8) -> Transaction {
        Transaction {
            txid: TxId::from_bytes([txid; 32]),
            transparent: TransparentData::default(),
            sprout: Default::default(),
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        }
    }

    /// Block 1 extends genesis and nothing else: not an empty parent, a sibling, itself, or a gap
    #[test]
    fn a_header_extends_only_its_parent_and_genesis_only_nothing() {
        let (genesis, one, two) =
            (header(0, 0x10, 0), header(1, 0x11, 0x10), header(2, 0x12, 0x11));
        let sibling = header(1, 0x21, 0x10);
        let at = |header: &DecodedHeader| BlockRef { hash: header.hash, height: header.height };
        let cases = [
            (&genesis, None, true),
            (&genesis, Some(at(&genesis)), false),
            (&one, Some(at(&genesis)), true),
            (&one, None, false),
            (&one, Some(at(&sibling)), false),
            (&one, Some(at(&one)), false),
            (&two, Some(at(&genesis)), false),
            (&two, Some(at(&one)), true),
        ];
        for (header, parent, expected) in cases {
            assert_eq!(header.extends(parent), expected, "{:?} on {parent:?}", header.height);
        }
        assert_eq!(Block::new(two.clone(), vec![tx(0)]).block_ref(), at(&two));
    }

    /// Body = what its header commits to
    ///
    /// - its own passes
    /// - txid added or reordered → root moves
    /// - repeated last pair keeps the root (CVE-2012-2459) → refused as mutated
    #[test]
    fn a_body_passes_only_when_its_txids_rebuild_its_headers_merkle_root() {
        let txs = vec![tx(0), tx(1), tx(2)];
        let txids: Vec<TxId> = txs.iter().map(|tx| tx.txid).collect();
        let committed = DecodedHeader {
            merkle_root: MerkleRoot::of_txids(&txids).expect("distinct txids"),
            ..header(1, 0x11, 0x10)
        };
        let with = |txs: Vec<Transaction>| Block::new(committed.clone(), txs);

        let added = [txs.clone(), vec![txs[0].clone()]].concat();
        let reordered = vec![txs[0].clone(), txs[2].clone(), txs[1].clone()];
        let repeated = [txs.clone(), vec![txs[2].clone()]].concat();
        #[rustfmt::skip]
        let cases = [
            ("its own txids",       with(txs.clone()),        Ok(())),
            ("a txid added",        with(added),              Err(BodyMismatch::MerkleRoot)),
            ("txids reordered",     with(reordered),          Err(BodyMismatch::MerkleRoot)),
            ("last txid dropped",   with(txs[..2].to_vec()),  Err(BodyMismatch::MerkleRoot)),
            ("last pair repeated",  with(repeated),           Err(BodyMismatch::Mutated)),
        ];
        for (case, body, expected) in cases {
            assert_eq!(body.check_body(), expected, "{case}");
        }
    }

    /// Every allocation counted once at its capacity: transactions, each pool's vector, scripts
    #[test]
    fn footprint_counts_every_allocation_at_capacity() {
        let action = OrchardAction {
            nullifier: Nullifier([1; 32]),
            cmx: NoteCommitment([2; 32]),
            ephemeral_key: EphemeralKey([3; 32]),
            enc_ciphertext: CompactCiphertext([4; CompactCiphertext::LENGTH]),
        };
        let mut script = Script::default();
        script.0 .0 = Vec::with_capacity(40);
        script.0 .0.extend_from_slice(&[0x76; 25]);
        let tx = Transaction {
            txid: TxId::from_bytes([9; 32]),
            transparent: TransparentData {
                coinbase: false,
                inputs: vec![OutPoint::new([8; 32], 0); 2],
                outputs: vec![TransparentOutput {
                    value: Zatoshis::from_u64(5).expect("in supply"),
                    script,
                }],
            },
            sprout: Default::default(),
            sapling: SaplingData { outputs: Vec::with_capacity(4), ..SaplingData::default() },
            orchard: OrchardData { actions: vec![action.clone(); 3], ..OrchardData::default() },
            ironwood: OrchardData { actions: vec![action], ..OrchardData::default() },
        };
        let block = Block::new(header(0, 0x10, 0), vec![tx]);

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
