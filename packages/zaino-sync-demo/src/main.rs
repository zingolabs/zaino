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

use std::sync::Arc;
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
    let rpc_url = std::env::var("ZEBRA_RPC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8232".to_string());

    let rpc = RpcClient::new(RpcClientConfig {
        url: rpc_url.clone(),
        auth: None,
        ..Default::default()
    })
    .expect("RPC client creation failed");

    println!("RPC: {rpc_url}");

    let adapter = ZebraRpcAdapter::new(rpc);

    // Get chain tip.
    let (tip_hash, tip_height) = adapter
        .get_chain_tip()
        .await
        .expect("get_chain_tip failed");
    let tip_u32 = u32::from(tip_height);

    println!("Chain tip: height={tip_height}, hash={tip_hash}");

    // Parse args: sync-headers [block_count] [concurrency] [batch_size]
    // RPC URL via env: ZEBRA_RPC_URL (default: http://127.0.0.1:8232)
    let args: Vec<String> = std::env::args().collect();
    let n_blocks: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let concurrency: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let batch_size: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(50);

    let sync_from = tip_u32.saturating_sub(n_blocks - 1);
    let sync_to = tip_u32;
    let block_count = sync_to - sync_from + 1;

    println!("Syncing blocks {sync_from}..={sync_to} ({block_count} blocks, concurrency={concurrency}, batch_size={batch_size})");

    let backend = InMemoryBackend::new();
    let set = IndexSet::new().with::<HeadersIndex>();
    let config = EngineConfig {
        batch_size,
        start_height: BlockHeight::new(u64::from(sync_from)),
    };
    let mut engine =
        SyncEngine::from_index_set(set, backend.clone(), config).expect("valid index set");

    // Provisioner: fetch blocks concurrently, send in order.
    let (tx, rx) = tokio::sync::mpsc::channel(concurrency * 2);
    let start = Instant::now();

    let adapter = Arc::new(adapter);

    tokio::spawn(async move {
        // Fetch blocks in a sliding window: up to `concurrency` in-flight,
        // issued in height order so results arrive roughly in order.
        let mut in_flight = futures::stream::FuturesOrdered::new();
        let mut next_to_spawn = sync_from;
        let mut sent = 0u32;

        loop {
            // Fill the window.
            while in_flight.len() < concurrency && next_to_spawn <= sync_to {
                let h = next_to_spawn;
                next_to_spawn += 1;
                let adapter = Arc::clone(&adapter);
                in_flight.push_back(async move {
                    let height = Height::try_from(h).expect("valid height");
                    let block = adapter.get_block(height).await.expect("get_block failed");
                    (h, context_from_block(&block))
                });
            }

            // Wait for the next result (in submission order).
            use futures::StreamExt;
            match in_flight.next().await {
                Some((h, ctx)) => {
                    tx.send(ctx).await.expect("engine channel open");
                    sent += 1;
                    let _ = h; // used implicitly via ordering

                    if sent % 50 == 0 || sent == block_count {
                        let elapsed = start.elapsed().as_secs_f64();
                        let rate = sent as f64 / elapsed;
                        eprintln!("  sent {sent}/{block_count} blocks ({rate:.1} blocks/s)");
                    }
                }
                None => break, // all done
            }
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
        if let Some(val) = backend.get_value(HEADERS_ID.into(), &key) {
            let header = HeadersIndex::decode_value(&val).expect("valid encoding");
            println!(
                "  height={h} hash={} prev={} time={} bits={:#010x}",
                header.hash, header.prev_hash, header.time, header.bits
            );
        }
    }
}
