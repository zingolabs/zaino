//! Block and block header.

use super::transaction::Transaction;
use super::{
    BlockCommitments, BlockHash, BlockTime, CompactDifficulty, EquihashNonce, Height, MerkleRoot,
};

/// Block header — the fields indexes need. No equihash solution
/// (indexer doesn't validate PoW).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHeader {
    /// Block hash (double-SHA256 of the serialized header).
    pub hash: BlockHash,
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
}

/// A complete block: header + transactions + chain metadata.
///
/// This is the domain-level block — not a wire format. Adapters
/// parse from their wire format (hex RPC, ReadState, etc.) into
/// this type. Indexes extract from it via `ProvideContext`.
#[derive(Debug, Clone)]
pub struct Block {
    /// Block header.
    pub header: BlockHeader,
    /// Transactions in block order.
    pub transactions: Vec<Transaction>,
    /// Commitment tree metadata after this block.
    pub chain_metadata: ChainMetadata,
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
