//! Block/output references and ranges the read surface addresses blocks by.

use zaino_primitives::types::{BlockHash, Height, TransactionId};

/// A block's height and hash together — the tip/fork "block id".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId {
    pub height: Height,
    pub hash: BlockHash,
}

/// How a caller names a block to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockRef {
    Height(Height),
    Hash(BlockHash),
}

/// `[start, end)` height range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeightRange {
    pub start: Height,
    pub end: Height,
}

/// A transparent outpoint `(txid, index)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Outpoint {
    pub txid: TransactionId,
    pub index: u32,
}
