//! ChainMetadataIndex (BlockLocal × Append): height → [`ChainMetadata`] (the
//! commitment-tree sizes a compact block carries).
//!
//! **Block-local because the source provides it.** A full block carries its
//! cumulative tree sizes in `ChainMetadata`; this index stores what the source
//! gives, per height. A source that emits [`ChainMetadata::ZERO`] instead — e.g.
//! one sourcing from `PreIndexCompactBlock`, which strips the sizes — would
//! require the indexer to *compute* them cumulatively; that is a separate
//! follow-on, not this index.

use zaino_persistence_codec::{DecodeError, EntryCodec};
use zaino_primitives::types::{ChainMetadata, TreeSize};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema};

/// Per-index context: the block's height and its commitment-tree sizes.
pub struct ChainMetadataCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Commitment-tree sizes after this block.
    pub chain_metadata: ChainMetadata,
}

/// ChainMetadata delta.
pub struct ChainMetadataEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Tree sizes (value).
    pub value: ChainMetadata,
}

/// ChainMetadata index definition.
pub struct ChainMetadataIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("chain_metadata");

impl IndexDef for ChainMetadataIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = ChainMetadataEntry;
    type BlockContext = ChainMetadataCtx;

    const NAME: IndexId = ID;
}

impl ExtractLocal for ChainMetadataIndex {
    fn extract(ctx: &ChainMetadataCtx) -> Result<Self::Delta, ExtractError> {
        Ok(ChainMetadataEntry {
            height: ctx.height,
            value: ctx.chain_metadata.clone(),
        })
    }
}

impl MergeAppend for ChainMetadataIndex {}

impl Schema<Vec<ChainMetadataEntry>> for ChainMetadataIndex {
    fn into_entries(entries: Vec<ChainMetadataEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<ChainMetadataEntry> {
        entries
            .into_iter()
            .map(|(height, value)| ChainMetadataEntry { height, value })
            .collect()
    }
}

impl EntryCodec for ChainMetadataIndex {
    type Key = BlockHeight;
    type Value = ChainMetadata;

    fn fingerprint_samples() -> Vec<(BlockHeight, ChainMetadata)> {
        vec![(
            BlockHeight::new(1),
            ChainMetadata {
                sapling_tree_size: TreeSize::from(2),
                orchard_tree_size: TreeSize::from(3),
                ironwood_tree_size: TreeSize::from(4),
            },
        )]
    }

    fn encode_key(key: &BlockHeight) -> Vec<u8> {
        key.value().to_le_bytes().to_vec()
    }

    fn encode_value(value: &ChainMetadata) -> Vec<u8> {
        // sapling(4) + orchard(4) + ironwood(4) = 12 bytes.
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&u32::from(value.sapling_tree_size).to_le_bytes());
        buf.extend_from_slice(&u32::from(value.orchard_tree_size).to_le_bytes());
        buf.extend_from_slice(&u32::from(value.ironwood_tree_size).to_le_bytes());
        buf
    }

    fn decode_key(bytes: &[u8]) -> Result<BlockHeight, DecodeError> {
        let arr: [u8; 8] = bytes
            .try_into()
            .map_err(|_| DecodeError::Invalid(format!("expected 8 bytes, got {}", bytes.len())))?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }

    fn decode_value(bytes: &[u8]) -> Result<ChainMetadata, DecodeError> {
        if bytes.len() != 12 {
            return Err(DecodeError::Invalid(format!(
                "expected 12 bytes, got {}",
                bytes.len()
            )));
        }
        // The 12-byte length is checked above, so each 4-byte window is exact.
        let field = |offset: usize| {
            TreeSize::from(u32::from_le_bytes(
                bytes[offset..offset + 4].try_into().expect("4 bytes"),
            ))
        };
        Ok(ChainMetadata {
            sapling_tree_size: field(0),
            orchard_tree_size: field(4),
            ironwood_tree_size: field(8),
        })
    }
}
