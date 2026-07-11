//! Demo: sync HeadersIndex from a live Zebra validator.
//!
//! Usage:
//!   # Port-forward first:
//!   kubectl --context zingo-infra port-forward -n golden-mainnet svc/zebra 8232:8232
//!
//!   # Then run:
//!   cargo run -p zaino-sync-demo
//!
//! Syncs recent blocks (last 100 from tip) into an InMemoryBackend
//! and prints progress + results.

use std::time::Instant;

use zaino_primitives::types::{Block, BlockHash, BlockTime, CompactDifficulty, Height};
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::{GetBlock, GetChainTip};
use zaino_source_zebra_rpc::ZebraRpcAdapter;
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::index_set::IndexSet;
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::testing::InMemoryBackend;
use zaino_sync::traits::{
    ExtractError, ExtractLocal, IndexDef, MergeAppend, ProvideContext, Schema, SchemaDecodeError,
};

// ---------------------------------------------------------------------------
// Set-wide context
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ZcashBlockContext {
    height: BlockHeight,
    hash: BlockHash,
    prev_hash: BlockHash,
    time: BlockTime,
    bits: CompactDifficulty,
}

fn context_from_block(block: &Block) -> ZcashBlockContext {
    ZcashBlockContext {
        height: BlockHeight::new(u64::from(block.header.height)),
        hash: block.header.hash,
        prev_hash: block.header.prev_hash,
        time: block.header.time,
        bits: block.header.bits,
    }
}

// ---------------------------------------------------------------------------
// HeadersIndex (L,A): height → (hash, prev_hash, time, bits)
// ---------------------------------------------------------------------------

struct HeaderCtx {
    height: BlockHeight,
    hash: BlockHash,
    prev_hash: BlockHash,
    time: BlockTime,
    bits: CompactDifficulty,
}

impl ProvideContext<HeaderCtx> for ZcashBlockContext {
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

struct HeaderEntry {
    height: BlockHeight,
    value: HeaderValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderValue {
    hash: BlockHash,
    prev_hash: BlockHash,
    time: BlockTime,
    bits: CompactDifficulty,
}

struct HeadersIndex;

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

    fn encode_key(key: &BlockHeight) -> Vec<u8> {
        key.value().to_le_bytes().to_vec()
    }

    fn encode_value(value: &HeaderValue) -> Vec<u8> {
        let mut buf = Vec::with_capacity(72);
        buf.extend_from_slice(&<[u8; 32]>::from(value.hash));
        buf.extend_from_slice(&<[u8; 32]>::from(value.prev_hash));
        buf.extend_from_slice(&value.time.to_le_bytes());
        buf.extend_from_slice(&value.bits.to_le_bytes());
        buf
    }

    fn decode_key(bytes: &[u8]) -> Result<BlockHeight, SchemaDecodeError> {
        let arr: [u8; 8] = bytes
            .try_into()
            .map_err(|_| SchemaDecodeError::Invalid(format!("expected 8 bytes, got {}", bytes.len())))?;
        Ok(BlockHeight::new(u64::from_le_bytes(arr)))
    }

    fn decode_value(bytes: &[u8]) -> Result<HeaderValue, SchemaDecodeError> {
        if bytes.len() != 72 {
            return Err(SchemaDecodeError::Invalid(format!(
                "expected 72 bytes, got {}",
                bytes.len()
            )));
        }
        let mut hash = [0u8; 32];
        let mut prev_hash = [0u8; 32];
        hash.copy_from_slice(&bytes[0..32]);
        prev_hash.copy_from_slice(&bytes[32..64]);
        Ok(HeaderValue {
            hash: BlockHash::from(hash),
            prev_hash: BlockHash::from(prev_hash),
            time: u32::from_le_bytes(bytes[64..68].try_into().expect("4 bytes")),
            bits: u32::from_le_bytes(bytes[68..72].try_into().expect("4 bytes")),
        })
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let rpc = RpcClient::new(RpcClientConfig {
        url: "http://127.0.0.1:8232".to_string(),
        auth: None,
        ..Default::default()
    })
    .expect("RPC client creation failed");

    let adapter = ZebraRpcAdapter::new(rpc);

    // Get chain tip.
    let (tip_hash, tip_height) = adapter
        .get_chain_tip()
        .await
        .expect("get_chain_tip failed");
    let tip_u32 = u32::from(tip_height);

    println!("Chain tip: height={tip_height}, hash={tip_hash}");

    // Sync last 100 blocks.
    let sync_from = tip_u32.saturating_sub(99);
    let sync_to = tip_u32;
    let block_count = sync_to - sync_from + 1;

    println!("Syncing blocks {sync_from}..={sync_to} ({block_count} blocks)");

    let backend = InMemoryBackend::new();
    let set = IndexSet::new().with::<HeadersIndex>();
    let config = EngineConfig {
        batch_size: 50,
        start_height: BlockHeight::new(u64::from(sync_from)),
    };
    let mut engine =
        SyncEngine::from_index_set(set, backend.clone(), config).expect("valid index set");

    // Provisioner: fetch blocks, send through channel.
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let start = Instant::now();

    tokio::spawn(async move {
        for h in sync_from..=sync_to {
            let height = Height::try_from(h).expect("valid height");
            let block = adapter.get_block(height).await.expect("get_block failed");

            if h % 10 == 0 || h == sync_to {
                let elapsed = start.elapsed().as_secs_f64();
                let done = h - sync_from + 1;
                let rate = done as f64 / elapsed;
                eprintln!(
                    "  fetched {done}/{block_count} blocks ({rate:.1} blocks/s)"
                );
            }

            tx.send(context_from_block(&block))
                .await
                .expect("channel open");
        }
    });

    // Run engine.
    engine.sync_channel(rx).await.expect("sync failed");

    let elapsed = start.elapsed();
    println!(
        "\nDone in {:.2}s ({:.1} blocks/s)",
        elapsed.as_secs_f64(),
        block_count as f64 / elapsed.as_secs_f64()
    );

    // Print some results.
    println!("\nSample indexed headers:");
    for h in [sync_from, sync_from + block_count / 2, sync_to] {
        let key = HeadersIndex::encode_key(&BlockHeight::new(u64::from(h)));
        if let Some(val) = backend.get_value(HEADERS_ID, &key) {
            let header = HeadersIndex::decode_value(&val).expect("valid encoding");
            println!(
                "  height={h} hash={} prev={} time={} bits={:#010x}",
                header.hash, header.prev_hash, header.time, header.bits
            );
        }
    }
}
