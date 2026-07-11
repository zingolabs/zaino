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
    /// Block height (sync-engine type).
    pub height: BlockHeight,
    /// Number of transactions in the block.
    pub tx_count: u32,
}

// ---------------------------------------------------------------------------
// Toy index: TxCountIndex (BlockLocal × Append)
//   Stores (height → transaction_count) for each block.
// ---------------------------------------------------------------------------

/// Per-index context.
pub struct TxCountCtx {
    pub height: BlockHeight,
    pub tx_count: u32,
}

impl ProvideContext<TxCountCtx> for SourceBlockContext {
    fn context(&self) -> TxCountCtx {
        TxCountCtx {
            height: self.height,
            tx_count: self.tx_count,
        }
    }
}

/// Delta: one height → tx_count entry.
pub struct TxCountEntry {
    pub height: BlockHeight,
    pub tx_count: u32,
}

/// Index definition.
pub struct TxCountIndex;

const TX_COUNT_ID: IndexId = IndexId::new("tx_count");

impl IndexDef for TxCountIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = TxCountEntry;
    type BlockContext = TxCountCtx;

    const NAME: IndexId = TX_COUNT_ID;
}

impl ExtractLocal for TxCountIndex {
    fn extract(ctx: &TxCountCtx) -> Result<Self::Delta, ExtractError> {
        Ok(TxCountEntry {
            height: ctx.height,
            tx_count: ctx.tx_count,
        })
    }
}

impl MergeAppend for TxCountIndex {}

impl Schema<Vec<TxCountEntry>> for TxCountIndex {
    type Key = BlockHeight;
    type Value = u32;

    fn into_entries(entries: Vec<TxCountEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries
            .into_iter()
            .map(|e| (e.height, e.tx_count))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TxCountEntry> {
        entries
            .into_iter()
            .map(|(height, tx_count)| TxCountEntry { height, tx_count })
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

    use zaino_primitives::types::{
        Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, Height, MerkleRoot,
        Transaction, TransactionHash, TransparentData, SaplingData, OrchardData,
    };
    use zaino_source::mock::MockChain;
    use zaino_source::GetBlock;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid")
    }

    fn hash(b: u8) -> BlockHash {
        BlockHash::from([b; 32])
    }

    /// Build a test block with `tx_count` empty transactions.
    fn test_block(h: u32, tx_count: u32) -> Block {
        let txs = (0..tx_count)
            .map(|i| Transaction {
                txid: TransactionHash::from([i as u8; 32]),
                index: i,
                transparent: TransparentData::default(),
                sapling: SaplingData::default(),
                orchard: OrchardData::default(),
            })
            .collect();

        Block {
            header: BlockHeader {
                hash: hash(h as u8),
                prev_hash: BlockHash::ZERO,
                height: height(h),
                time: 0,
                merkle_root: MerkleRoot::from([0; 32]),
                block_commitments: BlockCommitments::from([0; 32]),
                bits: 0,
                nonce: [0; 32],
            },
            transactions: txs,
            chain_metadata: ChainMetadata {
                sapling_tree_size: 0,
                orchard_tree_size: 0,
            },
        }
    }

    /// Build a MockChain with `n` blocks, block h has h+1 transactions.
    fn mock_chain(n: u32) -> MockChain {
        let mut chain = MockChain::new();
        for i in 0..n {
            chain = chain.with_block(test_block(i, i + 1));
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
            let block = source
                .get_block(height(h))
                .await
                .expect("block exists in mock");
            let ctx = SourceBlockContext {
                height: BlockHeight::new(u64::from(block.header.height)),
                tx_count: block.transactions.len() as u32,
            };
            tx.send(ctx).await.expect("channel open");
        }
    }

    #[tokio::test]
    async fn source_to_engine_end_to_end() {
        let chain = mock_chain(5);
        let backend = InMemoryBackend::new();

        let set = IndexSet::new().with::<TxCountIndex>();
        let config = EngineConfig {
            batch_size: 10,
            start_height: BlockHeight::new(0),
        };
        let mut engine =
            SyncEngine::from_index_set(set, backend.clone(), config).expect("valid set");

        let (tx, rx) = tokio::sync::mpsc::channel(16);

        tokio::spawn(async move {
            provision(chain, 0, 4, tx).await;
        });

        engine.sync_channel(rx).await.expect("sync succeeds");

        // Verify: block h has h+1 transactions.
        for h in 0..5u32 {
            let key = BlockHeight::new(u64::from(h)).encode();
            let val = backend
                .get_value(TX_COUNT_ID, &key)
                .expect("entry exists");
            let count = u32::from_le_bytes(val.as_slice().try_into().expect("4 bytes"));
            assert_eq!(count, h + 1, "block {h} should have {} txs", h + 1);
        }
    }

    #[tokio::test]
    async fn multi_batch_backpressure() {
        let n = 100u32;
        let chain = mock_chain(n);
        let backend = InMemoryBackend::new();

        let set = IndexSet::new().with::<TxCountIndex>();
        let config = EngineConfig {
            batch_size: 10,
            start_height: BlockHeight::new(0),
        };
        let mut engine =
            SyncEngine::from_index_set(set, backend.clone(), config).expect("valid set");

        let (tx, rx) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            provision(chain, 0, n - 1, tx).await;
        });

        engine.sync_channel(rx).await.expect("sync succeeds");

        for h in 0..n {
            let key = BlockHeight::new(u64::from(h)).encode();
            let val = backend
                .get_value(TX_COUNT_ID, &key)
                .unwrap_or_else(|| panic!("entry for block {h} missing"));
            let count = u32::from_le_bytes(val.as_slice().try_into().expect("4 bytes"));
            assert_eq!(count, h + 1, "block {h} tx count mismatch");
        }
    }
}
