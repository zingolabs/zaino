//! ChainMetadataIndex (SelfCumulative × Append): height → [`ChainMetadata`] (the
//! commitment-tree sizes a compact block carries).
//!
//! **Self-cumulative because the indexer computes the sizes.** A tree size at
//! height `h` is the size at `h-1` plus the note commitments this block adds, so
//! extraction threads a **carry** (the running sizes) and each height emits its
//! own disjoint entry — the `(SelfCumulative, Append)` cell of the sync model
//! (§3.2). The per-block context carries only the *counts* this block commits;
//! the source's own cumulative sizes are not trusted (a `PreIndexCompactBlock`
//! source strips them, and a full-block source's are redundant with what we
//! compute). The carry seeds at [`ChainMetadata::ZERO`] (genesis: empty trees).
//!
//! All three pools are treated uniformly: the per-block context reports the
//! sapling, orchard, and ironwood commitments the block adds (ironwood shares
//! orchard's action shape), and each accumulates into its own cumulative size.

use zaino_persistence_codec::{DecodeError, EntryCodec};
use zaino_primitives::types::{ChainMetadata, TreeSize, TreeSizeOutOfRange};
use zaino_sync::descriptor::{Append, SelfCumulative};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{CumulativeAppend, ExtractCumulative, IndexDef, MergeAppend, Schema};

/// Per-index context: the block's height and the note commitments it adds to
/// each pool (not the cumulative sizes — those are computed here).
pub struct ChainMetadataCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Sapling note commitments this block adds.
    pub sapling_added: u64,
    /// Orchard note commitments this block adds.
    pub orchard_added: u64,
    /// Ironwood note commitments this block adds.
    pub ironwood_added: u64,
}

/// One height's entry: the cumulative tree sizes after this block.
#[derive(Debug)]
pub struct ChainMetadataEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Cumulative tree sizes after this block (value, and the carry to the next).
    pub value: ChainMetadata,
}

/// ChainMetadata index definition.
pub struct ChainMetadataIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("chain_metadata");

impl IndexDef for ChainMetadataIndex {
    type Scope = SelfCumulative;
    type Composition = Append;
    type Delta = ChainMetadataEntry;
    type BlockContext = ChainMetadataCtx;

    const NAME: IndexId = ID;
}

impl ExtractCumulative for ChainMetadataIndex {
    type PriorState = ChainMetadata;
    // Extraction can fail one way: a pool's size leaving the compact protocol's
    // u32 range (#549). The type carries that exactly — no boxing at this layer.
    type Error = TreeSizeOutOfRange;

    fn extract(ctx: &ChainMetadataCtx, prior: &ChainMetadata) -> Result<Self::Delta, Self::Error> {
        // Each pool grows by this block's added commitments. TreeSize owns the
        // growth and its #549 boundary; a total that leaves the range is the one
        // typed failure, propagated directly.
        let value = ChainMetadata {
            sapling_tree_size: prior.sapling_tree_size.checked_add(ctx.sapling_added)?,
            orchard_tree_size: prior.orchard_tree_size.checked_add(ctx.orchard_added)?,
            ironwood_tree_size: prior.ironwood_tree_size.checked_add(ctx.ironwood_added)?,
        };
        Ok(ChainMetadataEntry {
            height: ctx.height,
            value,
        })
    }
}

impl MergeAppend for ChainMetadataIndex {}

impl CumulativeAppend for ChainMetadataIndex {
    fn initial_carry() -> ChainMetadata {
        ChainMetadata::ZERO
    }

    fn carry(delta: &ChainMetadataEntry) -> ChainMetadata {
        delta.value.clone()
    }
}

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
        // Fix the width up front: a `[u8; 12]` makes each 4-byte window exact by
        // construction, so the per-field reads are panic-free without asserting.
        let arr: [u8; 12] = bytes
            .try_into()
            .map_err(|_| DecodeError::Invalid(format!("expected 12 bytes, got {}", bytes.len())))?;
        let field = |offset: usize| {
            let mut word = [0u8; 4];
            word.copy_from_slice(&arr[offset..offset + 4]);
            TreeSize::from(u32::from_le_bytes(word))
        };
        Ok(ChainMetadata {
            sapling_tree_size: field(0),
            orchard_tree_size: field(4),
            ironwood_tree_size: field(8),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(height: u64, sapling: u64, orchard: u64) -> ChainMetadataCtx {
        ChainMetadataCtx {
            height: BlockHeight::new(height),
            sapling_added: sapling,
            orchard_added: orchard,
            ironwood_added: 0,
        }
    }

    #[test]
    fn extract_accumulates_per_pool_from_the_carry() {
        // Block 0 adds 2 sapling, 1 orchard to the empty trees.
        let d0 = ChainMetadataIndex::extract(&ctx(0, 2, 1), &ChainMetadata::ZERO)
            .expect("extract block 0");
        assert_eq!(d0.value.sapling_tree_size, TreeSize::from(2));
        assert_eq!(d0.value.orchard_tree_size, TreeSize::from(1));

        // Block 1 adds 3 sapling, 0 orchard — cumulative from block 0's carry.
        let carry = ChainMetadataIndex::carry(&d0);
        let d1 = ChainMetadataIndex::extract(&ctx(1, 3, 0), &carry).expect("extract block 1");
        assert_eq!(d1.value.sapling_tree_size, TreeSize::from(5));
        assert_eq!(d1.value.orchard_tree_size, TreeSize::from(1));
        assert_eq!(d1.height, BlockHeight::new(1));
    }

    #[test]
    fn initial_carry_is_empty_trees() {
        assert_eq!(ChainMetadataIndex::initial_carry(), ChainMetadata::ZERO);
    }

    #[test]
    fn extract_propagates_the_typed_tree_size_overflow() {
        // The overflow boundary is TreeSize's (asserted in its own tests); here we
        // check only that extract *propagates* it as its own typed error — matched
        // exhaustively, no downcast.
        let prior = ChainMetadata::new(u32::MAX, 0u32, 0u32);
        let err = ChainMetadataIndex::extract(&ctx(1, 1, 0), &prior)
            .expect_err("one more sapling commitment leaves the u32 range");
        assert_eq!(err, TreeSizeOutOfRange { got: 1u64 << 32 });
    }
}
