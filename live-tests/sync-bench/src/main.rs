//! Sync benchmark: measures index build performance against a live Zebra.
//!
//! Usage:
//!   sync-headers [block_count] [concurrency] [batch_size]
//!
//! Environment:
//!   ZEBRA_RPC_URL    — RPC endpoint (default: http://127.0.0.1:8232)
//!   ZEBRA_STATE_DIR  — Zebra cache dir. If set, uses ReadState (direct DB).
//!   ZAINO_DB_PATH    — LMDB path. If set, uses LMDB. If unset, in-memory.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use tracing::info;

const BENCH_TARGET: &str = "sync_bench";

static TRACER_PROVIDER: OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> = OnceLock::new();
use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_indexes::indexes::headers::ID as HEADERS_ID;
use zaino_indexes::indexes::txids::ID as TXIDS_ID;
use zaino_indexes::indexes::hash_to_height::ID as HASH_HEIGHT_ID;
use zaino_indexes::indexes::txid_location::ID as TXID_LOC_ID;
use zaino_indexes::indexes::transparent_data::ID as TDATA_ID;
use zaino_indexes::indexes::sapling::ID as SAPLING_ID;
use zaino_indexes::indexes::orchard::ID as ORCHARD_ID;
use zaino_indexes::indexes::transparent_spends::ID as SPENDS_ID;
use zaino_indexes::sets::current_zaino::{self, CurrentZainoContext};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, Namespace};
use zaino_primitives::types::{BlockHash, Height};
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::{GetBlock, GetChainTip};
use zaino_source_zebra_readstate::ZebraReadStateAdapter;
use zaino_source_zebra_rpc::ZebraRpcAdapter;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;

// ---------------------------------------------------------------------------
// Generic sync runner — takes a block-fetching closure
// ---------------------------------------------------------------------------

struct RunResult {
    elapsed_secs: f64,
    blocks_per_sec: f64,
    db_size_bytes: Option<u64>,
}

async fn run_sync<B, F, Fut>(
    backend: B,
    fetch_block: F,
    sync_from: u32,
    sync_to: u32,
    concurrency: usize,
    batch_size: u32,
    db_path: Option<&str>,
) -> RunResult
where
    B: Backend + 'static,
    B::Reader: 'static,
    B::Writer: 'static,
    F: Fn(u32) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = CurrentZainoContext> + Send,
{
    let block_count = sync_to - sync_from + 1;
    let set = current_zaino::index_set();
    let config = EngineConfig {
        batch_size,
        start_height: BlockHeight::new(u64::from(sync_from)),
    };
    let mut engine = SyncEngine::from_index_set(set, backend, config).expect("valid index set");

    let (tx, rx) = tokio::sync::mpsc::channel::<CurrentZainoContext>(concurrency * 2);
    let start = Instant::now();

    let fetch_block = Arc::new(fetch_block);

    tokio::spawn(async move {
        let mut in_flight = futures::stream::FuturesOrdered::new();
        let mut next_to_spawn = sync_from;
        let mut sent = 0u32;

        loop {
            while in_flight.len() < concurrency && next_to_spawn <= sync_to {
                let h = next_to_spawn;
                next_to_spawn += 1;
                let fetch = Arc::clone(&fetch_block);
                in_flight.push_back(async move { fetch(h).await });
            }

            use futures::StreamExt;
            match in_flight.next().await {
                Some(ctx) => {
                    tx.send(ctx).await.expect("engine channel open");
                    sent += 1;

                    if sent % 500 == 0 || sent == block_count {
                        let elapsed = start.elapsed().as_secs_f64();
                        let rate = sent as f64 / elapsed;
                        info!(
                            target: BENCH_TARGET,
                            sent,
                            total = block_count,
                            elapsed_secs = format!("{elapsed:.1}"),
                            blocks_per_sec = format!("{rate:.0}"),
                            "provisioner progress",
                        );
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

#[tokio::main]
async fn main() {
    // Always initialize tracing. RUST_LOG controls verbosity (default: sync_bench=info).
    // JSON output when ZAINO_LOG_JSON=1 (for Loki/Grafana), human-readable otherwise.
    // OTLP export to Tempo when OTEL_EXPORTER_OTLP_ENDPOINT is set.
    {
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::Layer;

        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("sync_bench=info"));

        let fmt_layer = if std::env::var("ZAINO_LOG_JSON").as_deref() == Ok("1") {
            tracing_subscriber::fmt::layer()
                .json()
                .with_span_events(FmtSpan::CLOSE)
                .boxed()
        } else {
            tracing_subscriber::fmt::layer()
                .with_span_events(FmtSpan::CLOSE)
                .with_target(false)
                .boxed()
        };

        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer);

        if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
            use opentelemetry::trace::TracerProvider;

            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .build()
                .expect("OTLP exporter");

            let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(
                    opentelemetry_sdk::Resource::builder()
                        .with_service_name("sync-bench")
                        .build(),
                )
                .build();

            let otel_layer = tracing_opentelemetry::layer()
                .with_tracer(tracer_provider.tracer("sync-bench"));

            registry.with(otel_layer).init();

            // Stash provider so we can flush on shutdown.
            TRACER_PROVIDER.set(tracer_provider).ok();
        } else {
            registry.init();
        }
    }

    let rpc_url = std::env::var("ZEBRA_RPC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8232".to_string());
    let state_dir = std::env::var("ZEBRA_STATE_DIR").ok().map(PathBuf::from);
    let db_path = std::env::var("ZAINO_DB_PATH").ok();

    let args: Vec<String> = std::env::args().collect();
    let n_blocks: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let concurrency: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let batch_size: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(50);

    // Determine provisioner and get chain tip.
    let (provisioner_name, tip_hash, tip_height): (&str, BlockHash, Height) =
        if let Some(ref dir) = state_dir {
            let adapter = ZebraReadStateAdapter::open(
                dir,
                &zebra_chain::parameters::Network::Mainnet,
            )
            .expect("open zebra readstate failed");
            let (hash, height) = adapter.get_chain_tip().await.expect("get_chain_tip");
            ("zebra-readstate", hash, height)
        } else {
            let rpc = RpcClient::new(RpcClientConfig {
                url: rpc_url.clone(),
                auth: None,
                ..Default::default()
            })
            .expect("RPC client creation failed");
            let adapter = ZebraRpcAdapter::new(rpc);
            let (hash, height) = adapter.get_chain_tip().await.expect("get_chain_tip");
            ("zebra-rpc", hash, height)
        };

    let tip_u32 = u32::from(tip_height);
    let sync_from = tip_u32.saturating_sub(n_blocks - 1);
    let sync_to = tip_u32;
    let block_count = sync_to - sync_from + 1;
    let backend_name = db_path.as_deref().unwrap_or("in-memory");

    // Log configuration.
    let state_dir_display = state_dir.as_ref().map(|d| d.display().to_string());
    info!(
        target: BENCH_TARGET,
        provisioner = provisioner_name,
        backend = backend_name,
        chain_tip_height = %tip_height,
        chain_tip_hash = %tip_hash,
        sync_from,
        sync_to,
        block_count,
        concurrency,
        batch_size,
        state_dir = state_dir_display.as_deref().unwrap_or("n/a"),
        rpc_url = if state_dir.is_none() { rpc_url.as_str() } else { "n/a" },
        "sync-bench configuration",
    );

    // Log index set description.
    let set = current_zaino::index_set();
    let descriptions = set.describe();
    info!(
        target: BENCH_TARGET,
        count = descriptions.len(),
        indexes = ?descriptions,
        "index set: current_zaino",
    );

    let ns_headers: Namespace = HEADERS_ID.into();
    let ns_txids: Namespace = TXIDS_ID.into();
    let ns_hash_height: Namespace = HASH_HEIGHT_ID.into();
    let ns_txid_loc: Namespace = TXID_LOC_ID.into();
    let ns_tdata: Namespace = TDATA_ID.into();
    let ns_sapling: Namespace = SAPLING_ID.into();
    let ns_orchard: Namespace = ORCHARD_ID.into();
    let ns_spends: Namespace = SPENDS_ID.into();
    let ns_meta = Namespace::new("_engine_meta");

    // Build the fetch closure based on provisioner type.
    let result = match (state_dir.as_ref(), db_path.as_deref()) {
        // ReadState + LMDB
        (Some(dir), Some(lmdb_path)) => {
            let adapter = Arc::new(
                ZebraReadStateAdapter::open(dir, &zebra_chain::parameters::Network::Mainnet)
                    .expect("open readstate"),
            );
            let backend = LmdbBackend::open(LmdbConfig {
                path: lmdb_path.into(),
                map_size_bytes: 120 << 30, // 120 GB
                namespaces: vec![ns_headers, ns_spends, ns_txids, ns_hash_height, ns_txid_loc, ns_tdata, ns_sapling, ns_orchard, ns_meta],
            })
            .expect("LMDB open failed");

            let fetch = move |h: u32| {
                let adapter = Arc::clone(&adapter);
                async move {
                    let height = Height::try_from(h).expect("valid");
                    let block = adapter.get_block(height).await.expect("get_block");
                    current_zaino::context_from_block(&block)
                }
            };
            run_sync(backend, fetch, sync_from, sync_to, concurrency, batch_size, Some(lmdb_path))
                .await
        }
        // ReadState + in-memory
        (Some(dir), None) => {
            let adapter = Arc::new(
                ZebraReadStateAdapter::open(dir, &zebra_chain::parameters::Network::Mainnet)
                    .expect("open readstate"),
            );
            let backend = InMemoryBackend::new();
            let fetch = move |h: u32| {
                let adapter = Arc::clone(&adapter);
                async move {
                    let height = Height::try_from(h).expect("valid");
                    let block = adapter.get_block(height).await.expect("get_block");
                    current_zaino::context_from_block(&block)
                }
            };
            run_sync(backend, fetch, sync_from, sync_to, concurrency, batch_size, None).await
        }
        // RPC + LMDB
        (None, Some(lmdb_path)) => {
            let rpc = RpcClient::new(RpcClientConfig {
                url: rpc_url.clone(),
                auth: None,
                ..Default::default()
            })
            .expect("rpc client");
            let adapter = Arc::new(ZebraRpcAdapter::new(rpc));
            let backend = LmdbBackend::open(LmdbConfig {
                path: lmdb_path.into(),
                map_size_bytes: 120 << 30, // 120 GB
                namespaces: vec![ns_headers, ns_spends, ns_txids, ns_hash_height, ns_txid_loc, ns_tdata, ns_sapling, ns_orchard, ns_meta],
            })
            .expect("LMDB open failed");

            let fetch = move |h: u32| {
                let adapter = Arc::clone(&adapter);
                async move {
                    let height = Height::try_from(h).expect("valid");
                    let block = adapter.get_block(height).await.expect("get_block");
                    current_zaino::context_from_block(&block)
                }
            };
            run_sync(backend, fetch, sync_from, sync_to, concurrency, batch_size, Some(lmdb_path))
                .await
        }
        // RPC + in-memory
        (None, None) => {
            let rpc = RpcClient::new(RpcClientConfig {
                url: rpc_url.clone(),
                auth: None,
                ..Default::default()
            })
            .expect("rpc client");
            let adapter = Arc::new(ZebraRpcAdapter::new(rpc));
            let backend = InMemoryBackend::new();
            let fetch = move |h: u32| {
                let adapter = Arc::clone(&adapter);
                async move {
                    let height = Height::try_from(h).expect("valid");
                    let block = adapter.get_block(height).await.expect("get_block");
                    current_zaino::context_from_block(&block)
                }
            };
            run_sync(backend, fetch, sync_from, sync_to, concurrency, batch_size, None).await
        }
    };

    info!(
        target: BENCH_TARGET,
        total_blocks = block_count,
        total_time_secs = format!("{:.2}", result.elapsed_secs),
        blocks_per_sec = format!("{:.1}", result.blocks_per_sec),
        db_size_mb = result.db_size_bytes.map(|s| format!("{:.2}", s as f64 / (1024.0 * 1024.0))),
        bytes_per_block = result.db_size_bytes.map(|s| format!("{:.0}", s as f64 / block_count as f64)),
        "sync-bench results",
    );

    // Flush any pending OTLP spans before exit.
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(e) = provider.shutdown() {
            eprintln!("OTLP shutdown error: {e}");
        }
    }
}
