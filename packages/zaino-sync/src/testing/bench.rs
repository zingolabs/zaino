//! Benchmark scenarios for the sync engine.
//!
//! Each test is `#[ignore]` so it doesn't run in the normal test suite.
//! Run with:
//!   `cargo test --package zaino-sync --features tracing -- bench --ignored --nocapture`
//!
//! Tracing output is controlled by `RUST_LOG`. Examples:
//!   `RUST_LOG=info` — batch commits and totals only
//!   `RUST_LOG=debug` — per-dispatch detail
//!   `RUST_LOG=off` — pure timing (default if unset)

#[cfg(test)]
mod tests {
    use std::sync::Once;
    use std::time::{Duration, Instant};

    use crate::engine::{EngineConfig, SyncEngine};
    use crate::index_set::IndexSet;
    use crate::primitives::BlockHeight;
    use crate::testing::toy_indexes::count_index::CountIndex;
    use crate::testing::toy_indexes::cumulative_sum_index::CumulativeSumIndex;
    use crate::testing::toy_indexes::running_sum_index::RunningSumIndex;
    use crate::testing::toy_indexes::value_index::ValueIndex;
    use crate::testing::{InMemoryBackend, SlowBackend, TestBlockContext};

    static INIT_TRACING: Once = Once::new();

    fn init_tracing() {
        INIT_TRACING.call_once(|| {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
                )
                .with_target(false)
                .compact()
                .init();
        });
    }

    /// Build blocks 0..n with value = height.
    fn make_blocks(n: u64) -> Vec<TestBlockContext> {
        (0..n)
            .map(|h| TestBlockContext {
                height: h,
                value: h as u32,
            })
            .collect()
    }

    /// Build the full 4-index set (3 BlockLocal + 1 SelfCumulative).
    fn full_index_set() -> IndexSet<TestBlockContext> {
        IndexSet::new()
            .with::<ValueIndex>()
            .with::<CountIndex>()
            .with::<RunningSumIndex>()
            .with::<CumulativeSumIndex>()
    }

    /// Report throughput.
    fn report(label: &str, block_count: u64, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        let blocks_per_sec = block_count as f64 / secs;
        println!(
            "  {label:<40} {block_count:>8} blocks in {secs:>8.3}s  ({blocks_per_sec:>10.0} blocks/sec)"
        );
    }

    // -----------------------------------------------------------------------
    // Baseline: instant IO, measure pure engine overhead
    // -----------------------------------------------------------------------

    #[test]
    #[ignore]
    fn baseline_sync_range() {
        init_tracing();
        println!("\n=== Baseline (in-memory, no IO delay) ===");
        for &n in &[1_000, 10_000, 100_000, 500_000] {
            let blocks = make_blocks(n);
            let backend = InMemoryBackend::new();
            let config = EngineConfig {
                batch_size: 1_000,
                start_height: BlockHeight::new(0),
            };
            let mut engine =
                SyncEngine::from_index_set(full_index_set(), backend, config)
                    .expect("valid index set");

            let start = Instant::now();
            engine.sync_range(blocks).expect("sync succeeds");
            report("sync_range", n, start.elapsed());
        }
    }

    #[test]
    #[ignore]
    fn baseline_sync_streaming() {
        init_tracing();
        println!("\n=== Baseline streaming (in-memory, no IO delay) ===");
        for &n in &[1_000, 10_000, 100_000, 500_000] {
            let backend = InMemoryBackend::new();
            let config = EngineConfig {
                batch_size: 1_000,
                start_height: BlockHeight::new(0),
            };
            let mut engine =
                SyncEngine::from_index_set(full_index_set(), backend, config)
                    .expect("valid index set");

            let blocks = (0..n).map(|h| TestBlockContext {
                height: h,
                value: h as u32,
            });

            let start = Instant::now();
            engine.sync_streaming(blocks).expect("sync succeeds");
            report("sync_streaming", n, start.elapsed());
        }
    }

    // -----------------------------------------------------------------------
    // Batch size sensitivity
    // -----------------------------------------------------------------------

    #[test]
    #[ignore]
    fn batch_size_sensitivity() {
        init_tracing();
        println!("\n=== Batch size sensitivity (100k blocks, no IO delay) ===");
        let n = 100_000u64;
        for &batch_size in &[10, 50, 100, 500, 1_000, 5_000, 10_000] {
            let blocks = make_blocks(n);
            let backend = InMemoryBackend::new();
            let config = EngineConfig {
                batch_size,
                start_height: BlockHeight::new(0),
            };
            let mut engine =
                SyncEngine::from_index_set(full_index_set(), backend, config)
                    .expect("valid index set");

            let start = Instant::now();
            engine.sync_range(blocks).expect("sync succeeds");
            report(&format!("batch_size={batch_size}"), n, start.elapsed());
        }
    }

    // -----------------------------------------------------------------------
    // Backend-bound: slow commits
    // -----------------------------------------------------------------------

    #[test]
    #[ignore]
    fn slow_backend_commit() {
        init_tracing();
        println!("\n=== Slow backend (1ms commit delay) ===");
        for &n in &[1_000, 10_000] {
            for &batch_size in &[100, 500, 1_000, 5_000] {
                let blocks = make_blocks(n);
                let inner = InMemoryBackend::new();
                let backend = SlowBackend::new(inner, Duration::from_millis(1));
                let config = EngineConfig {
                    batch_size,
                    start_height: BlockHeight::new(0),
                };
                let mut engine =
                    SyncEngine::from_index_set(full_index_set(), backend, config)
                        .expect("valid index set");

                let start = Instant::now();
                engine.sync_range(blocks).expect("sync succeeds");
                report(
                    &format!("n={n}, batch={batch_size}, commit=1ms"),
                    n,
                    start.elapsed(),
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn slow_backend_heavy_commit() {
        init_tracing();
        println!("\n=== Slow backend (5ms commit delay, simulating fsync) ===");
        for &n in &[1_000, 5_000] {
            for &batch_size in &[500, 1_000, 2_500] {
                let blocks = make_blocks(n);
                let inner = InMemoryBackend::new();
                let backend = SlowBackend::new(inner, Duration::from_millis(5));
                let config = EngineConfig {
                    batch_size,
                    start_height: BlockHeight::new(0),
                };
                let mut engine =
                    SyncEngine::from_index_set(full_index_set(), backend, config)
                        .expect("valid index set");

                let start = Instant::now();
                engine.sync_range(blocks).expect("sync succeeds");
                report(
                    &format!("n={n}, batch={batch_size}, commit=5ms"),
                    n,
                    start.elapsed(),
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Channel-based provisioner with delay (simulated RPC latency)
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore]
    async fn slow_provisioner_channel() {
        init_tracing();
        println!("\n=== Slow provisioner (50μs/block via channel) ===");
        for &n in &[1_000u64, 10_000] {
            let backend = InMemoryBackend::new();
            let config = EngineConfig {
                batch_size: 1_000,
                start_height: BlockHeight::new(0),
            };
            let mut engine =
                SyncEngine::from_index_set(full_index_set(), backend, config)
                    .expect("valid index set");

            let (tx, rx) = tokio::sync::mpsc::channel(256);
            let delay = Duration::from_micros(50);

            tokio::spawn(async move {
                for h in 0..n {
                    let ctx = TestBlockContext {
                        height: h,
                        value: h as u32,
                    };
                    tx.send(ctx).await.expect("channel open");
                    tokio::time::sleep(delay).await;
                }
            });

            let start = Instant::now();
            engine.sync_channel(rx).await.expect("sync succeeds");
            report(&format!("n={n}, prov_delay=50μs"), n, start.elapsed());
        }
    }

    #[tokio::test]
    #[ignore]
    async fn fast_provisioner_channel() {
        init_tracing();
        println!("\n=== Fast provisioner (no delay, channel backpressure only) ===");
        for &n in &[10_000u64, 100_000, 500_000] {
            let backend = InMemoryBackend::new();
            let config = EngineConfig {
                batch_size: 1_000,
                start_height: BlockHeight::new(0),
            };
            let mut engine =
                SyncEngine::from_index_set(full_index_set(), backend, config)
                    .expect("valid index set");

            let (tx, rx) = tokio::sync::mpsc::channel(1_024);

            tokio::spawn(async move {
                for h in 0..n {
                    let ctx = TestBlockContext {
                        height: h,
                        value: h as u32,
                    };
                    tx.send(ctx).await.expect("channel open");
                }
            });

            let start = Instant::now();
            engine.sync_channel(rx).await.expect("sync succeeds");
            report(&format!("channel, n={n}"), n, start.elapsed());
        }
    }

    // -----------------------------------------------------------------------
    // Combined: slow provisioner + slow backend
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore]
    async fn combined_slow_provisioner_and_backend() {
        init_tracing();
        println!("\n=== Combined: 50μs/block provisioner + 1ms commit ===");
        for &batch_size in &[100, 500, 1_000] {
            let n = 5_000u64;
            let inner = InMemoryBackend::new();
            let backend = SlowBackend::new(inner, Duration::from_millis(1));
            let config = EngineConfig {
                batch_size,
                start_height: BlockHeight::new(0),
            };
            let mut engine =
                SyncEngine::from_index_set(full_index_set(), backend, config)
                    .expect("valid index set");

            let (tx, rx) = tokio::sync::mpsc::channel(256);
            let delay = Duration::from_micros(50);

            tokio::spawn(async move {
                for h in 0..n {
                    let ctx = TestBlockContext {
                        height: h,
                        value: h as u32,
                    };
                    tx.send(ctx).await.expect("channel open");
                    tokio::time::sleep(delay).await;
                }
            });

            let start = Instant::now();
            engine.sync_channel(rx).await.expect("sync succeeds");
            report(
                &format!("batch={batch_size}, prov=50μs, commit=1ms"),
                n,
                start.elapsed(),
            );
        }
    }
}
