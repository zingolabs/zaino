//! Block and block header.

use super::transaction::Transaction;
use super::{
    BlockCommitments, BlockHash, BlockTime, CompactDifficulty, EquihashNonce, EquihashSolution,
    Height, MerkleRoot,
};

/// Block header — every consensus field, plus the hash and height that name
/// the block.
///
/// Zaino does not validate proof of work, but the header is carried whole
/// rather than as the subset an index happens to read: a consumer that
/// re-serializes a block, or persists one, needs the fields the block hash
/// commits to. `version` and `solution` are here for that reason alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHeader {
    /// Block hash (double-SHA256 of the serialized header).
    pub hash: BlockHash,
    /// Block version number.
    pub version: u32,
    /// Previous block's hash.
    pub prev_hash: BlockHash,
    /// Block height.
    pub height: Height,
    /// Timestamp when mined.
    pub time: BlockTime,
    /// Merkle root of transaction tree.
    pub merkle_root: MerkleRoot,
    /// Block commitments field (hashFinalSaplingRoot / hashBlockCommitments).
    pub block_commitments: BlockCommitments,
    /// Compact difficulty target (nBits).
    pub bits: CompactDifficulty,
    /// Equihash nonce.
    pub nonce: EquihashNonce,
    /// Equihash proof-of-work solution.
    pub solution: EquihashSolution,
}

/// A complete block: header + transactions + chain metadata.
///
/// This is the domain-level block — not a wire format. Adapters
/// parse from their wire format (hex RPC, ReadState, etc.) into
/// this type. Indexes extract from it via `ProvideContext`.
///
/// Transaction position is the block's to know, not the transaction's: a
/// transaction's slot — and so whether it is the coinbase — is read from the
/// order of [`transactions`](Self::transactions), with [`coinbase`](Self::coinbase)
/// as the named accessor for position 0. Prefer [`try_new`](Self::try_new) over a
/// struct literal: it rejects an empty transaction list, the one whole-block
/// invariant that outlives the removal of the per-transaction index (a block
/// always contains at least its coinbase).
#[derive(Debug, Clone)]
pub struct Block {
    /// Block header.
    pub header: BlockHeader,
    /// Transactions in block order.
    pub transactions: Vec<Transaction>,
    /// Commitment tree metadata after this block.
    pub chain_metadata: ChainMetadata,
}

/// A [`Block`] could not be constructed from the given parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlockError {
    /// The transaction list was empty. Every block mines a coinbase, so a
    /// block with no transactions is malformed.
    #[error("block has no transactions; a block must contain at least the coinbase")]
    NoTransactions,
}

impl Block {
    /// Assemble a block from its parts, rejecting a malformed transaction list.
    ///
    /// The only check is non-emptiness: a block always contains at least its
    /// coinbase. With transaction position derived from list order rather than
    /// stored per transaction, there is no index-versus-slot agreement left to
    /// validate — the order *is* the position.
    pub fn try_new(
        header: BlockHeader,
        transactions: Vec<Transaction>,
        chain_metadata: ChainMetadata,
    ) -> Result<Self, BlockError> {
        if transactions.is_empty() {
            return Err(BlockError::NoTransactions);
        }
        Ok(Self {
            header,
            transactions,
            chain_metadata,
        })
    }

    /// The coinbase transaction — the first in block order.
    ///
    /// Position is the sole authority for coinbase-ness: the coinbase is
    /// consensus-guaranteed to be transaction 0. Returns `None` only for an
    /// empty transaction list, which [`try_new`](Self::try_new) rejects at
    /// construction.
    pub fn coinbase(&self) -> Option<&Transaction> {
        self.transactions.first()
    }
}

/// Chain-level metadata attached to a block.
///
/// Sizes are plain counts, not `Option`: a pool that is not yet active at this
/// block has contributed no notes, so its cumulative size is genuinely `0`.
/// This differs from [`TreeRoots`](super::TreeRoots), where an absent root and
/// a zero root are distinct facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainMetadata {
    /// Cumulative Sapling note commitment tree size after this block.
    pub sapling_tree_size: u32,
    /// Cumulative Orchard note commitment tree size after this block.
    pub orchard_tree_size: u32,
    /// Cumulative Ironwood note commitment tree size after this block (NU6.3).
    pub ironwood_tree_size: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EquihashSolution, Height, TransactionId};

    fn header() -> BlockHeader {
        BlockHeader {
            hash: [0u8; 32].into(),
            version: 4,
            prev_hash: [0u8; 32].into(),
            height: Height::try_from(1).expect("a valid height"),
            time: 0,
            merkle_root: [0u8; 32].into(),
            block_commitments: [0u8; 32].into(),
            bits: 0,
            nonce: [0u8; 32],
            solution: EquihashSolution::Regtest([0u8; 36]),
        }
    }

    fn chain_metadata() -> ChainMetadata {
        ChainMetadata {
            sapling_tree_size: 0,
            orchard_tree_size: 0,
            ironwood_tree_size: 0,
        }
    }

    fn tx(tag: u8) -> Transaction {
        Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    #[test]
    fn try_new_rejects_an_empty_transaction_list() {
        let result = Block::try_new(header(), Vec::new(), chain_metadata());

        assert_eq!(result.unwrap_err(), BlockError::NoTransactions);
    }

    #[test]
    fn try_new_accepts_a_block_with_a_coinbase() {
        let block = Block::try_new(header(), vec![tx(0), tx(1)], chain_metadata())
            .expect("a non-empty block is valid");

        assert_eq!(block.transactions.len(), 2);
    }

    /// Coinbase-ness is positional: the coinbase is transaction 0, read from
    /// block order, never from a field on the transaction. A block whose
    /// transactions are supplied in some order reports whatever sits at
    /// position 0 as its coinbase — there is no stored index that could name a
    /// different one.
    #[test]
    fn coinbase_is_the_first_transaction_by_position() {
        let coinbase = tx(0xcb);
        let coinbase_txid = coinbase.txid;

        let block = Block::try_new(header(), vec![coinbase, tx(0x11)], chain_metadata())
            .expect("a non-empty block is valid");

        assert_eq!(
            block.coinbase().expect("a non-empty block has a coinbase").txid,
            coinbase_txid
        );
    }
}
