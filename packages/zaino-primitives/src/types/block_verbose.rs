//! Chain-state facts about a block that are not in the block itself.

use super::{BlockHash, ChainWork, Confirmations, Difficulty, ValuePoolBalance};

/// What a verbose block query adds to the block's own bytes.
///
/// # Why this is small
///
/// A verbose `getblock` response is mostly *derived*: hash, size, version,
/// merkle root, commitments, transaction list, time, nonce, solution, bits and
/// the previous hash all come from the serialized block, which the caller
/// already has from a raw block query. Repeating them here would give one fact
/// two sources.
///
/// What cannot be derived is this block's position in the current chain, and
/// the cumulative state as of it. That is what this carries, and it is why the
/// type is a handful of fields rather than a mirror of the RPC response. A
/// caller assembling a verbose response combines the raw block with this.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockVerbose {
    /// Depth of this block in the best chain, or `-1` if it is not on it.
    pub confirmations: Confirmations,

    /// Difficulty at this block, as a multiple of the network minimum.
    pub difficulty: Difficulty,

    /// Cumulative chainwork at this block.
    ///
    /// `None` from validators that do not track it — Zebra does not store
    /// cumulative work per height (ZcashFoundation/zebra#7109).
    pub chainwork: Option<ChainWork>,

    /// Total chain value as of this block.
    ///
    /// `None` when the validator does not report per-block supply. Not
    /// derivable from the block: it is the running total of every block before
    /// it.
    pub chain_supply: Option<ValuePoolBalance>,

    /// Per-pool value balances as of this block.
    ///
    /// Empty when the validator reports none. Cumulative, so likewise not
    /// derivable from this block alone.
    pub value_pools: Vec<ValuePoolBalance>,

    /// Cumulative note commitment tree sizes after this block.
    pub tree_sizes: BlockTreeSizes,

    /// Hash of the next block on the best chain.
    ///
    /// `None` when this block is the tip, or is not on the best chain — a fact
    /// about the chain rather than about the block.
    pub next_block_hash: Option<BlockHash>,
}

/// Cumulative note commitment tree sizes after a block.
///
/// Counts rather than `Option`s: a pool not yet active has contributed no
/// notes, so its size is genuinely zero. Same distinction
/// [`ChainMetadata`](super::ChainMetadata) draws against
/// [`TreeRoots`](super::TreeRoots), where an absent root and a zero root are
/// different facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockTreeSizes {
    /// Sapling notes committed as of this block.
    pub sapling: u64,
    /// Orchard notes committed as of this block.
    pub orchard: u64,
    /// Ironwood notes committed as of this block (NU6.3).
    pub ironwood: u64,
}
