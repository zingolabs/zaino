//! Core types for the block store.

/// 32-byte block hash.
pub type BlockHash = [u8; 32];

/// Block height (distance from genesis).
pub type Height = u32;

/// A block stored in the block store. Carries `hash`, `height`, and
/// `prev_hash` for chain-traversal, plus an opaque payload. The store never
/// interprets the payload — it is up to the ingester and the serving layer to
/// agree on its encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Hash of this block.
    pub hash: BlockHash,
    /// Height of this block (genesis = 0).
    pub height: Height,
    /// Hash of the parent block.
    pub prev_hash: BlockHash,
    /// Opaque block payload (e.g. serialised protobuf bytes).
    pub data: Vec<u8>,
}

impl Block {
    /// Create a new block.
    pub fn new(height: Height, hash: BlockHash, prev_hash: BlockHash, data: Vec<u8>) -> Self {
        Self {
            hash,
            height,
            prev_hash,
            data,
        }
    }
}

/// Genesis block hash (all zeros).
pub const GENESIS_HASH: BlockHash = [0u8; 32];

/// Genesis block (height 0, hash = genesis, prev_hash = self, empty payload).
pub fn genesis_block() -> Block {
    Block::new(0, GENESIS_HASH, GENESIS_HASH, Vec::new())
}

/// Maximum reorg depth per the Zcash protocol is 100 blocks.
///
/// Set to 101 (= N + 1) so `find_anchor_index` can walk back far enough
/// to hit the LMDB boundary when the fork point is at the freezer tip.
/// The +1 accounts for the fact that the walk must reach `h = fork + 1`
/// to check `h - 1 = fork` — testing the fork point itself costs an
/// extra step.
pub const MAX_REORG_DEPTH: u32 = 101;
