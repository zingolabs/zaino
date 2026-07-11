//! Demo: sync Zcash indexes from a live Zebra validator.
//!
//! Usage:
//!   sync-headers [block_count] [concurrency] [batch_size]
//!
//! Environment:
//!   ZEBRA_RPC_URL  — RPC endpoint (default: http://127.0.0.1:8232)
//!   ZAINO_DB_PATH  — LMDB path. If set, uses LMDB. If unset, in-memory.

use std::sync::Arc;
use std::time::Instant;

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, Namespace};
use zaino_primitives::types::{Block, BlockHash, BlockTime, CompactDifficulty, Height};
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::{GetBlock, GetChainTip};
use zaino_source_zebra_rpc::ZebraRpcAdapter;
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::index_set::IndexSet;
use zaino_sync::primitives::{BlockHeight, IndexId};
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
// HeadersIndex (L,A)
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
        let arr: [u8; 8] = bytes.try_into().map_err(|_| {
            SchemaDecodeError::Invalid(format!("expected 8 bytes, got {}", bytes.len()))
        })?;
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
// Generic sync runner
// ---------------------------------------------------------------------------

struct RunResult {
    elapsed_secs: f64,
    blocks_per_sec: f64,
    db_size_bytes: Option<u64>,
}

async fn run_sync<B: Backend + 'static>(
    backend: B,
    adapter: Arc<ZebraRpcAdapter>,
    sync_from: u32,
    sync_to: u32,
    concurrency: usize,
    batch_size: u32,
    db_path: Option<&str>,
) -> RunResult
where
    B::Reader: 'static,
    B::Writer: 'static,
{
    let block_count = sync_to - sync_from + 1;
    let set = IndexSet::new().with::<HeadersIndex>();
    let config = EngineConfig {
        batch_size,
        start_height: BlockHeight::new(u64::from(sync_from)),
    };
    let mut engine = SyncEngine::from_index_set(set, backend, config).expect("valid index set");

    let (tx, rx) = tokio::sync::mpsc::channel(concurrency * 2);
    let start = Instant::now();

    tokio::spawn(async move {
        let mut in_flight = futures::stream::FuturesOrdered::new();
        let mut next_to_spawn = sync_from;
        let mut sent = 0u32;

        loop {
            while in_flight.len() < concurrency && next_to_spawn <= sync_to {
                let h = next_to_spawn;
                next_to_spawn += 1;
                let adapter = Arc::clone(&adapter);
                in_flight.push_back(async move {
                    let height = Height::try_from(h).expect("valid height");
                    let block = adapter.get_block(height).await.expect("get_block failed");
                    context_from_block(&block)
                });
            }

            use futures::StreamExt;
            match in_flight.next().await {
                Some(ctx) => {
                    tx.send(ctx).await.expect("engine channel open");
                    sent += 1;

                    if sent % 500 == 0 || sent == block_count {
                        let elapsed = start.elapsed().as_secs_f64();
                        let rate = sent as f64 / elapsed;
                        println!("  progress: {sent}/{block_count} ({rate:.0} blocks/s)");
                    }
                }
                None => break,
            }
        }
    });

    engine.sync_channel(rx).await.expect("sync failed");

    let elapsed_secs = start.elapsed().as_secs_f64();
    let blocks_per_sec = block_count as f64 / elapsed_secs;

    let db_size_bytes = db_path.and_then(|p| {
        std::fs::read_dir(p)
            .ok()?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok().map(|m| m.len()))
            .reduce(|a, b| a + b)
    });

    RunResult {
        elapsed_secs,
        blocks_per_sec,
        db_size_bytes,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

const COMMIT_HASH: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    let rpc_url = std::env::var("ZEBRA_RPC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8232".to_string());
    let db_path = std::env::var("ZAINO_DB_PATH").ok();

    let rpc = RpcClient::new(RpcClientConfig {
        url: rpc_url.clone(),
        auth: None,
        ..Default::default()
    })
    .expect("RPC client creation failed");

    let adapter = Arc::new(ZebraRpcAdapter::new(rpc));

    let (tip_hash, tip_height) = adapter
        .get_chain_tip()
        .await
        .expect("get_chain_tip failed");
    let tip_u32 = u32::from(tip_height);

    let args: Vec<String> = std::env::args().collect();
    let n_blocks: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let concurrency: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let batch_size: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(50);

    let sync_from = tip_u32.saturating_sub(n_blocks - 1);
    let sync_to = tip_u32;
    let block_count = sync_to - sync_from + 1;

    let backend_name = db_path.as_deref().unwrap_or("in-memory");
    let index_set = "headers";
    let provisioner = "zebra-rpc";

    println!("════════════════════════════════════════════");
    println!("  zaino-sync-demo");
    println!("════════════════════════════════════════════");
    println!("  version:      {COMMIT_HASH}");
    println!("  rpc:          {rpc_url}");
    println!("  provisioner:  {provisioner}");
    println!("  backend:      {backend_name}");
    println!("  index_set:    {index_set}");
    println!("  chain_tip:    {tip_height} ({tip_hash})");
    println!("  block_range:  {sync_from}..={sync_to} ({block_count} blocks)");
    println!("  concurrency:  {concurrency}");
    println!("  batch_size:   {batch_size}");
    println!("────────────────────────────────────────────");

    let ns_headers: Namespace = HEADERS_ID.into();
    let ns_meta = Namespace::new("_engine_meta");

    let result = match db_path.as_deref() {
        Some(path) => {
            let backend = LmdbBackend::open(LmdbConfig {
                path: path.into(),
                map_size_bytes: 1 << 30,
                namespaces: vec![ns_headers, ns_meta],
            })
            .expect("LMDB open failed");

            run_sync(
                backend,
                adapter,
                sync_from,
                sync_to,
                concurrency,
                batch_size,
                Some(path),
            )
            .await
        }
        None => {
            let backend = InMemoryBackend::new();
            run_sync(
                backend,
                adapter,
                sync_from,
                sync_to,
                concurrency,
                batch_size,
                None,
            )
            .await
        }
    };

    println!("════════════════════════════════════════════");
    println!("  RESULTS");
    println!("════════════════════════════════════════════");
    println!("  total_blocks: {block_count}");
    println!("  total_time:   {:.2}s", result.elapsed_secs);
    println!("  blocks/s:     {:.1}", result.blocks_per_sec);
    if let Some(size) = result.db_size_bytes {
        let mb = size as f64 / (1024.0 * 1024.0);
        println!("  db_size:      {:.2} MB", mb);
        let bytes_per_block = size as f64 / block_count as f64;
        println!("  bytes/block:  {:.0}", bytes_per_block);
    }
    println!("════════════════════════════════════════════");
}
