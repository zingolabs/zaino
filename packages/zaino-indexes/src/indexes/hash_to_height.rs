//! HashToHeightIndex (BlockLocal × Append): block hash → height.

use zaino_persistence_codec::keys::{HashKey, HeightKey};
use zaino_persistence_codec::EntryCodec;
use zaino_primitives::types::BlockHash;
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

/// Per-index context.
pub struct HashToHeightCtx {
    /// Block hash.
    pub hash: BlockHash,
    /// Block height.
    pub height: BlockHeight,
}

/// Delta.
pub struct HashToHeightEntry {
    /// Block hash (key).
    pub hash: BlockHash,
    /// Block height (value).
    pub height: BlockHeight,
}

/// Index definition.
pub struct HashToHeightIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("hash_to_height");

impl IndexDef for HashToHeightIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = HashToHeightEntry;
    type BlockContext = HashToHeightCtx;
    const NAME: IndexId = ID;
}

impl ExtractLocal for HashToHeightIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &HashToHeightCtx) -> Result<Self::Delta, Self::Error> {
        Ok(HashToHeightEntry {
            hash: ctx.hash,
            height: ctx.height,
        })
    }
}

impl MergeAppend for HashToHeightIndex {}

impl Schema<Vec<HashToHeightEntry>> for HashToHeightIndex {
    fn into_entries(entries: Vec<HashToHeightEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.hash, e.height)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<HashToHeightEntry> {
        entries
            .into_iter()
            .map(|(hash, height)| HashToHeightEntry { hash, height })
            .collect()
    }
}

impl EntryCodec for HashToHeightIndex {
    type Key = BlockHash;
    type Value = BlockHeight;
    // The key is a plain 32-byte hash and the value a plain block height, so both
    // reuse the shared key records rather than re-deriving a layout.
    type PersistentKey = HashKey<BlockHash>;
    type PersistentValue = HeightKey<BlockHeight>;

    fn fingerprint_samples() -> Vec<(BlockHash, BlockHeight)> {
        vec![(BlockHash::from([1u8; 32]), BlockHeight::new(2))]
    }
}
