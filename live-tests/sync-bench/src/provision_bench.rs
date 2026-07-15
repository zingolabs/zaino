//! Provision-only benchmark: measures block fetch + context extraction cost
//! without the sync engine. Establishes a throughput baseline for the
//! provisioner pipeline in isolation.
//!
//! Usage:
//!   provision-bench [block_count] [concurrency]
//!
//! Environment:
//!   ZEBRA_STATE_DIR  — Zebra cache dir (required for ReadState).
//!   SYNC_FROM        — Start height (default: tip - block_count).
//!   BENCH_MODE       — What to measure (default: all). Options:
//!     raw_block      — get_block only, drop the Block
//!     headers_only   — get_block + HeadersOnlyContext extraction
//!     headers_spends — get_block + HeadersAndSpendsContext extraction
//!     current_zaino  — get_block + CurrentZainoContext extraction
//!     all            — run all modes sequentially

use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use zaino_primitives::types::Height;
use zaino_source::{GetBlock, GetCompactBlock};
use zaino_source_zebra_readstate::ZebraReadStateAdapter;

const TARGET: &str = "provision_bench";

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
enum BenchMode {
    RawBlock,
    CompactBlock,
    HeadersOnly,
    HeadersSpends,
    CurrentZaino,
    All,
}

struct ModeResult {
    mode: BenchMode,
    block_count: u32,
    elapsed_secs: f64,
    blocks_per_sec: f64,
}

async fn run_provision<F, Fut>(
    fetch: F,
    sync_from: u32,
    sync_to: u32,
    concurrency: usize,
    mode: BenchMode,
) -> ModeResult
where
    F: Fn(u32) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let block_count = sync_to - sync_from + 1;
    let fetch = Arc::new(fetch);
    let start = Instant::now();

    let mut in_flight = futures::stream::FuturesOrdered::new();
    let mut next_to_spawn = sync_from;
    let mut completed = 0u32;

    loop {
        while in_flight.len() < concurrency && next_to_spawn <= sync_to {
            let h = next_to_spawn;
            next_to_spawn += 1;
            let fetch = Arc::clone(&fetch);
            in_flight.push_back(async move { fetch(h).await });
        }

        match in_flight.next().await {
            Some(()) => {
                completed += 1;
                if completed % 5000 == 0 || completed == block_count {
                    let elapsed = start.elapsed().as_secs_f64();
                    let rate = completed as f64 / elapsed;
                    tracing::info!(
                        target: TARGET,
                        mode = %mode,
                        completed,
                        total = block_count,
                        blocks_per_sec = format!("{rate:.0}"),
                        "progress",
                    );
                }
            }
            None => break,
        }
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    ModeResult {
        mode,
        block_count,
        elapsed_secs,
        blocks_per_sec: block_count as f64 / elapsed_secs,
    }
}

#[tokio::main]
async fn main() {
    // Always init tracing.
    {
        use tracing_subscriber::fmt::format::FmtSpan;
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("provision_bench=info"));

        if std::env::var("ZAINO_LOG_JSON").as_deref() == Ok("1") {
            tracing_subscriber::fmt()
                .json()
                .with_span_events(FmtSpan::CLOSE)
                .with_env_filter(filter)
                .init();
        } else {
            tracing_subscriber::fmt()
                .with_span_events(FmtSpan::CLOSE)
                .with_target(false)
                .with_env_filter(filter)
                .init();
        }
    }

    let state_dir = std::env::var("ZEBRA_STATE_DIR")
        .expect("ZEBRA_STATE_DIR is required for provision-bench");
    let args: Vec<String> = std::env::args().collect();
    let n_blocks: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let concurrency: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(16);
    let bench_mode: BenchMode = std::env::var("BENCH_MODE")
        .unwrap_or_else(|_| "all".to_string())
        .parse()
        .expect("invalid BENCH_MODE; options: raw_block, headers_only, headers_spends, current_zaino, all");

    let adapter = Arc::new(
        ZebraReadStateAdapter::open(
            state_dir.as_ref(),
            &zebra_chain::parameters::Network::Mainnet,
        )
        .expect("open zebra readstate failed"),
    );

    let (_, tip_height) = zaino_source::GetChainTip::get_chain_tip(adapter.as_ref())
        .await
        .expect("get_chain_tip");
    let tip_u32 = u32::from(tip_height);

    let sync_from = std::env::var("SYNC_FROM")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or_else(|| tip_u32.saturating_sub(n_blocks - 1));
    let sync_to = sync_from + n_blocks - 1;
    let block_count = sync_to - sync_from + 1;

    tracing::info!(
        target: TARGET,
        sync_from,
        sync_to,
        block_count,
        concurrency,
        bench_mode = %bench_mode,
        chain_tip = tip_u32,
        "provision-bench configuration",
    );

    let modes = match bench_mode {
        BenchMode::All => vec![
            BenchMode::RawBlock,
            BenchMode::CompactBlock,
            BenchMode::HeadersOnly,
            BenchMode::HeadersSpends,
            BenchMode::CurrentZaino,
        ],
        single => vec![single],
    };

    let mut results = Vec::new();

    for mode in modes {
        tracing::info!(target: TARGET, mode = %mode, "starting mode");

        let result = match mode {
            BenchMode::RawBlock => {
                let a = Arc::clone(&adapter);
                run_provision(
                    move |h| {
                        let a = Arc::clone(&a);
                        async move {
                            let height = Height::try_from(h).expect("valid");
                            let _block = a.get_block(height).await.expect("get_block");
                        }
                    },
                    sync_from, sync_to, concurrency, BenchMode::RawBlock,
                ).await
            }
            BenchMode::CompactBlock => {
                let a = Arc::clone(&adapter);
                run_provision(
                    move |h| {
                        let a = Arc::clone(&a);
                        async move {
                            let height = Height::try_from(h).expect("valid");
                            let _compact = a.get_compact_block(height).await.expect("get_compact_block");
                        }
                    },
                    sync_from, sync_to, concurrency, BenchMode::CompactBlock,
                ).await
            }
            BenchMode::HeadersOnly => {
                // Uses BlockHeader request — no transaction deserialization.
                let a = Arc::clone(&adapter);
                run_provision(
                    move |h| {
                        let a = Arc::clone(&a);
                        async move {
                            let height = Height::try_from(h).expect("valid");
                            let _header = a.get_block_header(height).await.expect("get_block_header");
                        }
                    },
                    sync_from, sync_to, concurrency, BenchMode::HeadersOnly,
                ).await
            }
            BenchMode::HeadersSpends => {
                // Uses CompactBlock — has transparent outpoints, no proofs/scripts.
                let a = Arc::clone(&adapter);
                run_provision(
                    move |h| {
                        let a = Arc::clone(&a);
                        async move {
                            let height = Height::try_from(h).expect("valid");
                            let _compact = a.get_compact_block(height).await.expect("get_compact_block");
                        }
                    },
                    sync_from, sync_to, concurrency, BenchMode::HeadersSpends,
                ).await
            }
            BenchMode::CurrentZaino => {
                // Uses CompactBlock — has everything the current index set needs.
                let a = Arc::clone(&adapter);
                run_provision(
                    move |h| {
                        let a = Arc::clone(&a);
                        async move {
                            let height = Height::try_from(h).expect("valid");
                            let _compact = a.get_compact_block(height).await.expect("get_compact_block");
                        }
                    },
                    sync_from, sync_to, concurrency, BenchMode::CurrentZaino,
                ).await
            }
            BenchMode::All => unreachable!("expanded above"),
        };

        tracing::info!(
            target: TARGET,
            mode = %result.mode,
            block_count = result.block_count,
            elapsed_secs = format!("{:.2}", result.elapsed_secs),
            blocks_per_sec = format!("{:.1}", result.blocks_per_sec),
            "mode complete",
        );
        results.push(result);
    }

    // Summary
    tracing::info!(target: TARGET, "provision-bench summary");
    for r in &results {
        tracing::info!(
            target: TARGET,
            mode = %r.mode,
            blocks_per_sec = format!("{:.1}", r.blocks_per_sec),
            elapsed_secs = format!("{:.2}", r.elapsed_secs),
            "result",
        );
    }
}
