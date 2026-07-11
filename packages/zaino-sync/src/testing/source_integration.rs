//! Integration test: MockChain → async provisioner → sync engine → index → backend.
//!
//! Proves the full pipeline composes across the zaino-source / zaino-sync boundary.

use crate::descriptor::{Append, BlockLocal};
use crate::encode::Encode;
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, ProvideContext, Schema};

// ---------------------------------------------------------------------------
// Set-wide context: what the provisioner produces per block
// ---------------------------------------------------------------------------

/// Block context produced by the source-backed provisioner.
#[derive(Debug, Clone)]
pub struct SourceBlockContext {
    /// Block height.
    pub height: BlockHeight,
    /// Raw block bytes (opaque payload from the source).
    pub raw_bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Toy index: BlockSizeIndex (BlockLocal × Append)
//   Stores (height → block_size_in_bytes) for each block.
// ---------------------------------------------------------------------------

/// Per-index context: just needs height and the byte count.
pub struct BlockSizeContext {
    /// Block height.
    pub height: BlockHeight,
    /// Size of the raw block in bytes.
    pub size: u32,
}

impl ProvideContext<BlockSizeContext> for SourceBlockContext {
    fn context(&self) -> BlockSizeContext {
        BlockSizeContext {
            height: self.height,
            size: self.raw_bytes.len() as u32,
        }
    }
}

/// Delta: one height → size entry.
pub struct BlockSizeEntry {
    pub height: BlockHeight,
    pub size: u32,
}

/// Index definition.
pub struct BlockSizeIndex;

const BLOCK_SIZE_ID: IndexId = IndexId::new("block_size");

impl IndexDef for BlockSizeIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = BlockSizeEntry;
    type BlockContext = BlockSizeContext;

    const NAME: IndexId = BLOCK_SIZE_ID;
}

impl ExtractLocal for BlockSizeIndex {
    fn extract(ctx: &BlockSizeContext) -> Result<Self::Delta, ExtractError> {
        Ok(BlockSizeEntry {
            height: ctx.height,
            size: ctx.size,
        })
    }
}

impl MergeAppend for BlockSizeIndex {}

impl Schema<Vec<BlockSizeEntry>> for BlockSizeIndex {
    type Key = BlockHeight;
    type Value = u32;

    fn into_entries(entries: Vec<BlockSizeEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.size)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<BlockSizeEntry> {
        entries
            .into_iter()
            .map(|(height, size)| BlockSizeEntry { height, size })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineConfig, SyncEngine};
    use crate::index_set::IndexSet;
    use crate::testing::InMemoryBackend;

    use zaino_primitives::types::{BlockHash, Height};
    use zaino_source::mock::MockChain;
    use zaino_source::GetBlockBytes;

    /// Build a MockChain with `n` blocks, each containing `height + 1` bytes.
    fn mock_chain(n: u32) -> MockChain {
        let mut chain = MockChain::new();
        for i in 0..n {
            let height = Height::try_from(i).expect("valid");
            let hash = BlockHash::from([i as u8; 32]);
            let bytes = vec![0xAB; (i + 1) as usize]; // size = height + 1
            chain = chain.with_block(height, hash, bytes);
        }
        chain
    }

    /// Async provisioner: reads blocks from source, sends contexts into channel.
    async fn provision(
        source: MockChain,
        from: u32,
        to: u32,
        tx: tokio::sync::mpsc::Sender<SourceBlockContext>,
    ) {
        for h in from..=to {
            let height = Height::try_from(h).expect("valid");
            let raw_bytes = source
                .get_block_bytes(height)
                .await
                .expect("block exists in mock");
            let ctx = SourceBlockContext {
                height: BlockHeight::new(u64::from(height)),
                raw_bytes,
            };
            tx.send(ctx).await.expect("channel open");
        }
    }

    #[tokio::test]
    async fn source_to_engine_end_to_end() {
        let chain = mock_chain(5);
        let backend = InMemoryBackend::new();

        let set = IndexSet::new().with::<BlockSizeIndex>();
        let config = EngineConfig {
            batch_size: 10,
            start_height: BlockHeight::new(0),
        };
        let mut engine =
            SyncEngine::from_index_set(set, backend.clone(), config).expect("valid set");

        let (tx, rx) = tokio::sync::mpsc::channel(16);

        // Spawn provisioner
        tokio::spawn(async move {
            provision(chain, 0, 4, tx).await;
        });

        // Run engine
        engine.sync_channel(rx).await.expect("sync succeeds");

        // Verify: block at height 0 has 1 byte, height 4 has 5 bytes.
        for h in 0..5u32 {
            let key = BlockHeight::new(h as u64).encode();
            let val = backend
                .get_value(BLOCK_SIZE_ID, &key)
                .expect("entry exists");
            let size = u32::from_le_bytes(val.as_slice().try_into().expect("4 bytes"));
            assert_eq!(size, h + 1, "block {h} should have {} bytes", h + 1);
        }
    }

    /// 100 blocks, batch size 10, channel capacity 8.
    ///
    /// The provisioner outruns the engine: channel fills at block 8,
    /// provisioner suspends until the engine drains a batch. Exercises
    /// backpressure across 10 batch cycles.
    #[tokio::test]
    async fn multi_batch_backpressure() {
        let n = 100u32;
        let chain = mock_chain(n);
        let backend = InMemoryBackend::new();

        let set = IndexSet::new().with::<BlockSizeIndex>();
        let config = EngineConfig {
            batch_size: 10,
            start_height: BlockHeight::new(0),
        };
        let mut engine =
            SyncEngine::from_index_set(set, backend.clone(), config).expect("valid set");

        // Channel smaller than batch size — provisioner must wait.
        let (tx, rx) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            provision(chain, 0, n - 1, tx).await;
        });

        engine.sync_channel(rx).await.expect("sync succeeds");

        // Verify every block was indexed.
        for h in 0..n {
            let key = BlockHeight::new(u64::from(h)).encode();
            let val = backend
                .get_value(BLOCK_SIZE_ID, &key)
                .unwrap_or_else(|| panic!("entry for block {h} missing"));
            let size = u32::from_le_bytes(val.as_slice().try_into().expect("4 bytes"));
            assert_eq!(size, h + 1, "block {h} size mismatch");
        }
    }
}
