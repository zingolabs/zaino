//! Integration test: MockChain → async provisioner → sync engine → index → backend.
//!
//! Two indexes running in parallel from the same provisioner:
//! - TxCountIndex (L,A): stores height → transaction count
//! - HeadersIndex (L,A): stores height → (hash, prev_hash, time, bits)

use crate::descriptor::{Append, BlockLocal};
use crate::encode::{Decode, DecodeError, Encode};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, ProvideContext, Schema};

use zaino_primitives::types::{BlockHash, BlockTime, CompactDifficulty, Height};

// ---------------------------------------------------------------------------
// Encode/Decode impls for domain types (test-only, will move to index crate)
// ---------------------------------------------------------------------------

impl Encode for BlockHash {
    fn encode(&self) -> Vec<u8> {
        <[u8; 32]>::from(*self).to_vec()
    }
}

impl Decode for BlockHash {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| DecodeError::InvalidLength {
            expected: 32,
            got: bytes.len(),
        })?;
        Ok(BlockHash::from(arr))
    }
}

// ---------------------------------------------------------------------------
// Set-wide context: carries everything the provisioner extracts from a Block
// ---------------------------------------------------------------------------

/// Block context produced by the source-backed provisioner.
#[derive(Debug, Clone)]
pub struct SourceBlockContext {
    /// Block height (sync-engine type).
    pub height: BlockHeight,
    /// Number of transactions.
    pub tx_count: u32,
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Block timestamp.
    pub time: BlockTime,
    /// Compact difficulty.
    pub bits: CompactDifficulty,
}

// ---------------------------------------------------------------------------
// TxCountIndex (BlockLocal × Append): height → transaction count
// ---------------------------------------------------------------------------

/// Per-index context for TxCountIndex.
pub struct TxCountCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Number of transactions.
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

/// TxCount delta.
pub struct TxCountEntry {
    /// Block height.
    pub height: BlockHeight,
    /// Transaction count.
    pub tx_count: u32,
}

/// TxCount index definition.
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
        entries.into_iter().map(|e| (e.height, e.tx_count)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TxCountEntry> {
        entries
            .into_iter()
            .map(|(height, tx_count)| TxCountEntry { height, tx_count })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// HeadersIndex (BlockLocal × Append): height → header data
// ---------------------------------------------------------------------------

/// Per-index context for HeadersIndex.
pub struct HeaderCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Timestamp.
    pub time: BlockTime,
    /// Compact difficulty.
    pub bits: CompactDifficulty,
}

impl ProvideContext<HeaderCtx> for SourceBlockContext {
    fn context(&self) -> HeaderCtx {
        HeaderCtx {
            height: self.height,
            hash: self.hash,
            prev_hash: self.prev_hash,
            time: self.time,
            bits: self.bits,
        }
    }
}

/// Header delta.
pub struct HeaderEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Header data (value).
    pub value: HeaderValue,
}

/// Persisted header value: hash(32) + prev_hash(32) + time(4) + bits(4) = 72 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderValue {
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Timestamp.
    pub time: BlockTime,
    /// Compact difficulty.
    pub bits: CompactDifficulty,
}

impl Encode for HeaderValue {
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(72);
        buf.extend_from_slice(&self.hash.encode());
        buf.extend_from_slice(&self.prev_hash.encode());
        buf.extend_from_slice(&self.time.to_le_bytes());
        buf.extend_from_slice(&self.bits.to_le_bytes());
        buf
    }
}

impl Decode for HeaderValue {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() != 72 {
            return Err(DecodeError::InvalidLength {
                expected: 72,
                got: bytes.len(),
            });
        }
        Ok(Self {
            hash: BlockHash::decode(&bytes[0..32])?,
            prev_hash: BlockHash::decode(&bytes[32..64])?,
            time: u32::from_le_bytes(bytes[64..68].try_into().expect("4 bytes")),
            bits: u32::from_le_bytes(bytes[68..72].try_into().expect("4 bytes")),
        })
    }
}

/// Headers index definition.
pub struct HeadersIndex;

const HEADERS_ID: IndexId = IndexId::new("headers");

impl IndexDef for HeadersIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = HeaderEntry;
    type BlockContext = HeaderCtx;

    const NAME: IndexId = HEADERS_ID;
}

impl ExtractLocal for HeadersIndex {
    fn extract(ctx: &HeaderCtx) -> Result<Self::Delta, ExtractError> {
        Ok(HeaderEntry {
            height: ctx.height,
            value: HeaderValue {
                hash: ctx.hash,
                prev_hash: ctx.prev_hash,
                time: ctx.time,
                bits: ctx.bits,
            },
        })
    }
}

impl MergeAppend for HeadersIndex {}

impl Schema<Vec<HeaderEntry>> for HeadersIndex {
    type Key = BlockHeight;
    type Value = HeaderValue;

    fn into_entries(entries: Vec<HeaderEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<HeaderEntry> {
        entries
            .into_iter()
            .map(|(height, value)| HeaderEntry { height, value })
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
        Block, BlockCommitments, BlockHeader, ChainMetadata, MerkleRoot, OrchardData, SaplingData,
        Transaction, TransactionHash, TransparentData,
    };
    use zaino_source::mock::MockChain;
    use zaino_source::GetBlock;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid")
    }

    fn hash(b: u8) -> BlockHash {
        BlockHash::from([b; 32])
    }

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
                prev_hash: if h == 0 { BlockHash::ZERO } else { hash((h - 1) as u8) },
                height: height(h),
                time: 1_000_000 + h,
                merkle_root: MerkleRoot::from([0; 32]),
                block_commitments: BlockCommitments::from([0; 32]),
                bits: 0x1d00ffff + h,
                nonce: [0; 32],
            },
            transactions: txs,
            chain_metadata: ChainMetadata {
                sapling_tree_size: 0,
                orchard_tree_size: 0,
            },
        }
    }

    fn mock_chain(n: u32) -> MockChain {
        let mut chain = MockChain::new();
        for i in 0..n {
            chain = chain.with_block(test_block(i, i + 1));
        }
        chain
    }

    fn context_from_block(block: &Block) -> SourceBlockContext {
        SourceBlockContext {
            height: BlockHeight::new(u64::from(block.header.height)),
            tx_count: block.transactions.len() as u32,
            hash: block.header.hash,
            prev_hash: block.header.prev_hash,
            time: block.header.time,
            bits: block.header.bits,
        }
    }

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
            tx.send(context_from_block(&block))
                .await
                .expect("channel open");
        }
    }

    #[tokio::test]
    async fn headers_and_tx_count_end_to_end() {
        let chain = mock_chain(5);
        let backend = InMemoryBackend::new();

        let set = IndexSet::new()
            .with::<TxCountIndex>()
            .with::<HeadersIndex>();
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

        // Verify tx counts.
        for h in 0..5u32 {
            let key = BlockHeight::new(u64::from(h)).encode();
            let val = backend
                .get_value(TX_COUNT_ID, &key)
                .expect("tx_count entry exists");
            let count = u32::from_le_bytes(val.as_slice().try_into().expect("4 bytes"));
            assert_eq!(count, h + 1, "block {h} tx count");
        }

        // Verify headers.
        for h in 0..5u32 {
            let key = BlockHeight::new(u64::from(h)).encode();
            let val = backend
                .get_value(HEADERS_ID, &key)
                .expect("header entry exists");
            let header = HeaderValue::decode(&val).expect("valid header encoding");

            assert_eq!(header.hash, hash(h as u8), "block {h} hash");
            let expected_prev = if h == 0 { BlockHash::ZERO } else { hash((h - 1) as u8) };
            assert_eq!(header.prev_hash, expected_prev, "block {h} prev_hash");
            assert_eq!(header.time, 1_000_000 + h, "block {h} time");
            assert_eq!(header.bits, 0x1d00ffff + h, "block {h} bits");
        }
    }

    #[tokio::test]
    async fn multi_batch_with_two_indexes() {
        let n = 100u32;
        let chain = mock_chain(n);
        let backend = InMemoryBackend::new();

        let set = IndexSet::new()
            .with::<TxCountIndex>()
            .with::<HeadersIndex>();
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
            backend
                .get_value(TX_COUNT_ID, &key)
                .unwrap_or_else(|| panic!("tx_count for block {h} missing"));
            backend
                .get_value(HEADERS_ID, &key)
                .unwrap_or_else(|| panic!("header for block {h} missing"));
        }
    }
}
