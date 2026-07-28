//! `getblockheader` — block header plus cumulative chain state.

use crate::types::{
    BlockCommitments, BlockHash, BlockTime, ChainWork, CompactDifficulty, Confirmations,
    Difficulty, EquihashNonce, Height, MerkleRoot, TreeRoot,
};

/// A block header as reported by `getblockheader` with `verbose = true`.
///
/// Distinct from [`BlockHeader`](crate::types::BlockHeader), which is Zaino's
/// own model of the header as it appears in a block. This type additionally
/// carries values that are not in the header at all but are computed by the
/// validator from cumulative chain state — confirmations, difficulty,
/// chainwork, and the neighbouring block hashes.
///
/// The non-verbose (`verbose = false`) form is the raw serialised header, so it
/// is served as bytes by a separate query rather than being a variant here.
/// Verbosity is chosen by the caller, so it is a property of the request, not
/// something the response has to be polymorphic over.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockHeaderVerbose {
    /// Hash of this block.
    pub hash: BlockHash,

    /// Depth of this block in the best chain, or `-1` if it is not on the best
    /// chain. Signed for exactly that reason.
    pub confirmations: Confirmations,

    /// Height of this block.
    pub height: Height,

    /// Header version field.
    pub version: u32,

    /// Merkle root of this block's transaction tree.
    pub merkle_root: MerkleRoot,

    /// Block time, in seconds since the Unix epoch.
    pub time: BlockTime,

    /// Header nonce.
    pub nonce: EquihashNonce,

    /// Equihash solution from the header.
    ///
    /// Opaque bytes: Zaino does not validate proof of work, so the solution is
    /// carried for callers that do, and never interpreted here. Its length is
    /// network-dependent, so it is not a fixed-size array.
    pub solution: Vec<u8>,

    /// Difficulty threshold in compact (nBits) form.
    pub bits: CompactDifficulty,

    /// Difficulty as a multiple of the network minimum.
    pub difficulty: Difficulty,

    /// The `blockcommitments` field of the header.
    ///
    /// `None` from validators that do not report it — it is Zebra-specific,
    /// added in ZcashFoundation/zebra#9217. Its interpretation depends on
    /// network and height.
    pub block_commitments: Option<BlockCommitments>,

    /// Sapling commitment tree root after this block.
    ///
    /// `None` for blocks before Sapling activation, and from validators that
    /// omit it.
    pub final_sapling_root: Option<TreeRoot>,

    /// Cumulative chainwork at this block.
    ///
    /// `None` from Zebra, which does not report chainwork over RPC. Modelled
    /// because a caller that has it can order competing branches without
    /// recomputing work from headers; callers must handle its absence rather
    /// than assume it.
    pub chainwork: Option<ChainWork>,

    /// Hash of the previous block.
    ///
    /// `None` for the genesis block, which has no parent.
    pub previous_block_hash: Option<BlockHash>,

    /// Hash of the next block on the best chain.
    ///
    /// `None` when this block is the tip, or is not on the best chain.
    pub next_block_hash: Option<BlockHash>,
}
