//! HashToHeightIndex (BlockLocal × Append): block hash → height.

use zaino_primitives::types::BlockHash;
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{
    ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema, SchemaDecodeError,
};

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
    fn extract(ctx: &HashToHeightCtx) -> Result<Self::Delta, ExtractError> {
        Ok(HashToHeightEntry {
            hash: ctx.hash,
            height: ctx.height,
        })
    }
}

impl MergeAppend for HashToHeightIndex {}

impl Schema<Vec<HashToHeightEntry>> for HashToHeightIndex {
    type Key = BlockHash;
    type Value = BlockHeight;

    fn into_entries(entries: Vec<HashToHeightEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.hash, e.height)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<HashToHeightEntry> {
        entries
            .into_iter()
            .map(|(hash, height)| HashToHeightEntry { hash, height })
            .collect()
    }

    fn encode_key(key: &BlockHash) -> Vec<u8> {
        <[u8; 32]>::from(*key).to_vec()
    }

    fn encode_value(value: &BlockHeight) -> Vec<u8> {
        value.value().to_le_bytes().to_vec()
    }

    fn decode_key(bytes: &[u8]) -> Result<BlockHash, SchemaDecodeError> {
        let mut arr = [0u8; 32];
        if bytes.len() != 32 {
            return Err(SchemaDecodeError::Invalid(format!(
                "expected 32 bytes, got {}",
                bytes.len()
            )));
        }
        arr.copy_from_slice(bytes);
        Ok(BlockHash::from(arr))
    }

    fn decode_value(bytes: &[u8]) -> Result<BlockHeight, SchemaDecodeError> {
        let arr: [u8; 8] = bytes.try_into().map_err(|_| {
            SchemaDecodeError::Invalid(format!("expected 8 bytes, got {}", bytes.len()))
        })?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }
}
