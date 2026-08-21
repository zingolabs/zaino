//! Concurrent load test — "how many connections can you support, and how fast
//! can you serve blocks?"
//!
//! Ported from the `zaino-admin concurrent-test` tool on the `hahn/store`
//! branch, with a connection sweep, tail percentiles, and a file-descriptor
//! preflight added: a single point sample cannot say where the knee is, a mean
//! hides the tail that clients actually feel, and a client-side `ulimit` is
//! easily mistaken for a server-side ceiling.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Barrier;

use clap::Args;
use tonic::transport::Channel;
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;

use crate::chain::ChainVerifier;
use crate::error::BenchError;
use crate::grpc_client;
use crate::stats::Summary;

/// Hammer a server with N concurrent `GetBlockRange` clients and report how
/// many succeeded, how long they took, and the aggregate block throughput.
#[derive(Args)]
pub(super) struct ConcurrentArgs {
    /// Server under test (e.g. "http://127.0.0.1:8137").
    #[arg(short, long)]
    server: String,

    /// Lower bound of the height pool connections draw their ranges from.
    #[arg(long)]
    start_height: u64,

    /// Upper bound (inclusive) of the height pool.
    #[arg(long)]
    end_height: u64,

    /// Blocks each connection fetches.
    #[arg(short, long, default_value = "1000")]
    blocks: u64,

    /// Concurrent connections. Ignored when `--sweep` is given.
    #[arg(short, long, default_value = "1000")]
    connections: usize,

    /// Run one round per value and print a comparison table, e.g.
    /// `--sweep 100,250,500,1000,2000`. This is what locates the knee — the
    /// point where the success rate starts falling off.
    #[arg(long, value_delimiter = ',')]
    sweep: Vec<usize>,

    /// Milliseconds between spawning each connection, to avoid a SYN burst that
    /// would measure the kernel's accept backlog rather than the server.
    ///
    /// An upper bound only: the actual gap is whichever is smaller, this or an
    /// even spread across `--spawn-window-ms`. See [`spawn_gap`].
    #[arg(long, default_value = "1")]
    spawn_delay_ms: u64,

    /// Longest the whole round may take to bring every connection up.
    ///
    /// At high connection counts a fixed per-connection delay stops measuring
    /// concurrency: 10,000 connections at 1ms apart take 10s to establish, by
    /// which time the first ones have finished and the peak overlap is nowhere
    /// near 10,000. Capping the total ramp keeps the round a concurrency
    /// measurement instead of a throughput one.
    #[arg(long, default_value = "2000")]
    spawn_window_ms: u64,

    /// Seconds to settle between sweep rounds, so one round's teardown does not
    /// land on the next round's connects.
    #[arg(long, default_value = "5")]
    settle_secs: u64,

    /// Print per-connection timing.
    #[arg(short, long)]
    verbose: bool,
}

/// What one connection did.
struct ConnectionResult {
    index: usize,
    range_start: u64,
    range_end: u64,
    blocks: usize,
    connect_elapsed: Duration,
    fetch_elapsed: Duration,
    error: Option<String>,
    chain_breaks: usize,
}

impl ConnectionResult {
    fn total_elapsed(&self) -> Duration {
        self.connect_elapsed + self.fetch_elapsed
    }
}

/// The headline numbers for one round, kept so the sweep can tabulate them.
struct RoundSummary {
    connections: usize,
    succeeded: usize,
    wall_elapsed: Duration,
    blocks_fetched: usize,
    chain_breaks: usize,
    fetch: Option<Summary>,
}

impl RoundSummary {
    fn success_rate(&self) -> f64 {
        if self.connections == 0 {
            return 0.0;
        }
        self.succeeded as f64 / self.connections as f64 * 100.0
    }

    fn aggregate_throughput(&self) -> f64 {
        let seconds = self.wall_elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        self.blocks_fetched as f64 / seconds
    }
}

/// Longest a single connection may take to establish before the round gives up
/// on it. Bounds the barrier: without it one hung dial stalls the whole round.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) async fn run(args: ConcurrentArgs) -> Result<(), BenchError> {
    let pool_size = pool_size(args.start_height, args.end_height)?;

    if args.blocks == 0 {
        return Err(BenchError::Args("--blocks must be at least 1".into()));
    }
    if args.blocks > pool_size {
        return Err(BenchError::Args(format!(
            "--blocks {} is larger than the available pool {}..={} ({pool_size} blocks)",
            args.blocks, args.start_height, args.end_height,
        )));
    }

    let rounds = if args.sweep.is_empty() {
        vec![args.connections]
    } else {
        args.sweep.clone()
    };
    if rounds.contains(&0) {
        return Err(BenchError::Args(
            "connection counts must be at least 1".into(),
        ));
    }

    eprintln!("Server: {}", args.server);
    eprintln!(
        "Pool:   {}..={} ({pool_size} blocks)",
        args.start_height, args.end_height
    );
    let max_connections = rounds.iter().copied().max().unwrap_or(0);
    if let Some(warning) =
        open_file_limit().and_then(|limit| fd_limit_warning(limit, max_connections))
    {
        eprintln!();
        eprintln!("  ⚠ {warning}");
    }
    eprintln!();

    let mut summaries = Vec::with_capacity(rounds.len());
    for (position, connections) in rounds.iter().copied().enumerate() {
        if position > 0 && args.settle_secs > 0 {
            tokio::time::sleep(Duration::from_secs(args.settle_secs)).await;
        }
        summaries.push(round(&args, pool_size, connections).await);
    }

    if summaries.len() > 1 {
        print_sweep_table(&summaries, args.blocks);
    }

    // A sweep is *meant* to push past the knee, so partial failure is a result,
    // not an error. Zero successes anywhere means the harness never reached the
    // server at all — that is a failed run, and it should exit non-zero.
    if summaries.iter().all(|summary| summary.succeeded == 0) {
        return Err(BenchError::AllConnectionsFailed(args.server.clone()));
    }

    Ok(())
}

async fn round(args: &ConcurrentArgs, pool_size: u64, connections: usize) -> RoundSummary {
    let ranges = distribute_ranges(args.start_height, pool_size, args.blocks, connections);
    let overlap = args.blocks.saturating_mul(connections as u64) > pool_size;

    eprintln!("──────────────────────────────────────────");
    eprintln!(
        "{connections} connections × {} blocks each{}",
        args.blocks,
        if overlap { " (ranges overlap)" } else { "" }
    );
    eprintln!();

    let gap = spawn_gap(args.spawn_delay_ms, args.spawn_window_ms, connections);
    eprintln!(
        "  ramp: {:.0}µs between connects, {:.1}s to bring all {connections} up",
        gap.as_secs_f64() * 1e6,
        gap.as_secs_f64() * connections as f64
    );

    // `connections + 1`: every connection, plus this task, so the round's
    // wall-clock starts the instant the last socket is established rather than
    // when the first one was created. The ramp is then a setup cost outside the
    // measurement, not part of it.
    let barrier = Arc::new(Barrier::new(connections + 1));

    let mut handles = Vec::with_capacity(connections);
    for (index, (range_start, range_end)) in ranges.into_iter().enumerate() {
        let server = args.server.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            fetch_one(index, &server, range_start, range_end, barrier).await
        }));
        if !gap.is_zero() {
            tokio::time::sleep(gap).await;
        }
    }

    barrier.wait().await;
    eprintln!("  all {connections} connected; starting fetch");
    eprintln!();
    let wall_start = Instant::now();

    // A task that panicked has no timing to contribute; dropping it here keeps
    // it out of the statistics, and it still shows in the success/total counts.
    let results: Vec<ConnectionResult> = futures::future::join_all(handles)
        .await
        .into_iter()
        .filter_map(Result::ok)
        .collect();

    let wall_elapsed = wall_start.elapsed();

    if args.verbose {
        for result in &results {
            eprintln!("{}", verbose_line(result));
        }
        eprintln!();
    }

    summarise(connections, args.blocks, wall_elapsed, &results)
}

/// Connect, wait for every other connection, then fetch and verify.
///
/// The barrier is what makes the round a concurrency measurement. Without it a
/// connection whose work is shorter than the spawn ramp finishes before its
/// successors are even created, so the number open at once never approaches the
/// nominal count — the round silently becomes a throughput test. Holding every
/// connection open until all of them have connected means the fetch phase starts
/// with exactly `connections` sockets established, whatever the work costs.
///
/// A connection that fails to connect still waits, so one failure cannot strand
/// the rest; it just skips the fetch.
async fn fetch_one(
    index: usize,
    server: &str,
    range_start: u64,
    range_end: u64,
    barrier: Arc<Barrier>,
) -> ConnectionResult {
    let connect_start = Instant::now();
    // Bound the connect so a single hung dial cannot hold the barrier — and
    // with it the whole round — open indefinitely.
    let client =
        match tokio::time::timeout(CONNECT_TIMEOUT, grpc_client::connect_eager(server)).await {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(_) => Err(format!(
                "connect timed out after {}s",
                CONNECT_TIMEOUT.as_secs()
            )),
        };
    let connect_elapsed = connect_start.elapsed();

    barrier.wait().await;

    let fetch_start = Instant::now();
    let failed = |error: String| ConnectionResult {
        index,
        range_start,
        range_end,
        blocks: 0,
        connect_elapsed,
        fetch_elapsed: fetch_start.elapsed(),
        error: Some(error),
        chain_breaks: 0,
    };

    let mut client: CompactTxStreamerClient<Channel> = match client {
        Ok(client) => client,
        Err(error) => return failed(error),
    };

    match grpc_client::fetch_block_range(&mut client, range_start, range_end).await {
        Ok(blocks) => {
            let mut verifier = ChainVerifier::new();
            for block in &blocks {
                verifier.push(block);
            }
            ConnectionResult {
                index,
                range_start,
                range_end,
                blocks: blocks.len(),
                connect_elapsed,
                fetch_elapsed: fetch_start.elapsed(),
                error: None,
                chain_breaks: verifier.total_errors(),
            }
        }
        Err(error) => failed(error.to_string()),
    }
}

/// Spreads `connections` windows of `blocks` evenly across the pool.
///
/// The first window starts at `start_height` and the last ends at the pool's
/// end. When `blocks × connections` exceeds the pool the windows overlap, which
/// is fine: this is a load test, not a coverage sweep.
fn distribute_ranges(
    start_height: u64,
    pool_size: u64,
    blocks: u64,
    connections: usize,
) -> Vec<(u64, u64)> {
    let end_height = start_height + pool_size - 1;
    let step = if connections > 1 {
        (pool_size - blocks) as f64 / (connections - 1) as f64
    } else {
        0.0
    };

    (0..connections)
        .map(|index| {
            let range_start = start_height + (step * index as f64).round() as u64;
            (range_start, (range_start + blocks - 1).min(end_height))
        })
        .collect()
}

fn pool_size(start_height: u64, end_height: u64) -> Result<u64, BenchError> {
    end_height
        .checked_sub(start_height)
        .map(|span| span + 1)
        .ok_or_else(|| {
            BenchError::Args(format!(
                "--end-height {end_height} is below --start-height {start_height}"
            ))
        })
}

fn summarise(
    connections: usize,
    blocks_each: u64,
    wall_elapsed: Duration,
    results: &[ConnectionResult],
) -> RoundSummary {
    let ok: Vec<&ConnectionResult> = results.iter().filter(|r| r.error.is_none()).collect();
    let failed = connections - ok.len();

    let seconds = |extract: fn(&ConnectionResult) -> Duration| -> Vec<f64> {
        ok.iter().map(|r| extract(r).as_secs_f64()).collect()
    };
    let connect = Summary::new(&seconds(|r| r.connect_elapsed));
    let fetch = Summary::new(&seconds(|r| r.fetch_elapsed));
    let total = Summary::new(&seconds(ConnectionResult::total_elapsed));

    let summary = RoundSummary {
        connections,
        succeeded: ok.len(),
        wall_elapsed,
        blocks_fetched: ok.iter().map(|r| r.blocks).sum(),
        chain_breaks: ok.iter().map(|r| r.chain_breaks).sum(),
        fetch,
    };

    eprintln!("Results:");
    eprintln!(
        "  Connections: {}/{failed}/{connections}  (success / failed / total) — {:.1}% success",
        summary.succeeded,
        summary.success_rate(),
    );
    eprintln!(
        "  Per connection: {blocks_each} blocks ({} total fetched)",
        summary.blocks_fetched,
    );
    eprintln!(
        "  Chain breaks: {}{}",
        summary.chain_breaks,
        if summary.chain_breaks > 0 { " ⚠" } else { "" }
    );
    eprintln!(
        "  Wall-clock time: {:.2}s",
        summary.wall_elapsed.as_secs_f64()
    );

    if let (Some(connect), Some(fetch), Some(total)) = (connect, fetch, total) {
        eprintln!();
        eprintln!("{}", connect.line("Connect time (s):"));
        eprintln!("{}", fetch.line("Fetch time (s):"));
        eprintln!("{}", total.line("Per-connection total (s):"));
        eprintln!();
        eprintln!(
            "  Aggregate throughput: {:.0} blocks/s across {connections} connections",
            summary.aggregate_throughput(),
        );
        if total.mean > 0.0 {
            eprintln!(
                "  Per-connection throughput: {:.0} blocks/s (mean)",
                blocks_each as f64 / total.mean,
            );
        }
    } else {
        eprintln!();
        eprintln!("  All {connections} connections failed — no timings to report.");
        if let Some(first) = results.iter().find_map(|r| r.error.as_deref()) {
            eprintln!("  First error: {first}");
        }
    }
    eprintln!();

    summary
}

fn print_sweep_table(summaries: &[RoundSummary], blocks_each: u64) {
    eprintln!("══════════════════════════════════════════");
    eprintln!("  Connection Sweep — {blocks_each} blocks per connection");
    eprintln!("══════════════════════════════════════════");
    eprintln!(
        "  {:>6}  {:>9}  {:>8}  {:>10}  {:>12}  {:>8}",
        "conns", "success", "wall (s)", "mean fetch", "blocks/s", "breaks"
    );

    for summary in summaries {
        eprintln!(
            "  {:>6}  {:>8.1}%  {:>8.2}  {:>10}  {:>12.0}  {:>8}",
            summary.connections,
            summary.success_rate(),
            summary.wall_elapsed.as_secs_f64(),
            summary
                .fetch
                .map(|fetch| format!("{:.3}s", fetch.mean))
                .unwrap_or_else(|| "—".to_string()),
            summary.aggregate_throughput(),
            summary.chain_breaks,
        );
    }
    eprintln!();
    eprintln!("  The supported connection count is the last row still at 100% success.");
}

fn verbose_line(result: &ConnectionResult) -> String {
    match &result.error {
        Some(error) => format!(
            "  Connection {:>4}: {}..={} → ERROR (connect {:.2}s) — {error}",
            result.index,
            result.range_start,
            result.range_end,
            result.connect_elapsed.as_secs_f64(),
        ),
        None => format!(
            "  Connection {:>4}: {}..={} → {} blocks, connect {:.2}s, fetch {:.2}s{}",
            result.index,
            result.range_start,
            result.range_end,
            result.blocks,
            result.connect_elapsed.as_secs_f64(),
            result.fetch_elapsed.as_secs_f64(),
            if result.chain_breaks > 0 {
                format!(", {} chain break(s) ⚠", result.chain_breaks)
            } else {
                String::new()
            },
        ),
    }
}

/// Warns when this process cannot open enough sockets for the largest round.
///
/// Without this, a client-side `ulimit -n` of 1024 reads as "the server tops out
/// near 1000 connections", which is the wrong answer to the question being asked.
/// Gap between two connects: the per-connection delay, or an even spread across
/// the ramp window, whichever is smaller.
///
/// Sub-millisecond by design at high counts — 10,000 connections across a 2s
/// window is 200µs apart, which still avoids a SYN burst but keeps every
/// connection up inside the window rather than 10s later.
fn spawn_gap(delay_ms: u64, window_ms: u64, connections: usize) -> Duration {
    let requested = Duration::from_millis(delay_ms);
    let connections = connections.max(1) as u32;
    let spread = Duration::from_millis(window_ms) / connections;
    requested.min(spread)
}

fn fd_limit_warning(limit: u64, max_connections: usize) -> Option<String> {
    let needed = (max_connections as u64).saturating_mul(2);

    (limit < needed).then(|| {
        format!(
            "open-file limit is {limit}, but {max_connections} connections need roughly \
             {needed}. Raise it (`ulimit -n {needed}`) or the ceiling you measure will \
             be this client's, not the server's."
        )
    })
}

/// This process's soft `RLIMIT_NOFILE`, read from procfs.
///
/// Procfs rather than a `libc` dependency: the harness only needs to *warn*, and
/// on a platform without procfs it simply declines to.
fn open_file_limit() -> Option<u64> {
    let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
    parse_open_file_limit(&limits)
}

fn parse_open_file_limit(limits: &str) -> Option<u64> {
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))?;
    line.split_whitespace()
        .find_map(|field| field.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_span_the_pool_end_to_end() {
        let ranges = distribute_ranges(1_000, 1_000, 100, 4);
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges[0], (1_000, 1_099));
        assert_eq!(ranges[3], (1_900, 1_999));
        assert!(ranges.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    }

    #[test]
    fn a_single_connection_starts_at_the_pool_start() {
        assert_eq!(distribute_ranges(1_000, 1_000, 100, 1), [(1_000, 1_099)]);
    }

    #[test]
    fn ranges_overlap_rather_than_overrun_when_demand_exceeds_the_pool() {
        let pool_start = 1_000;
        let pool_size = 500;
        let ranges = distribute_ranges(pool_start, pool_size, 400, 10);

        assert_eq!(ranges.len(), 10);
        assert!(
            ranges.iter().all(|&(_, end)| end < pool_start + pool_size),
            "no range may run past the pool: {ranges:?}"
        );
        assert!(
            ranges.windows(2).any(|pair| pair[1].0 <= pair[0].1),
            "windows should overlap once demand exceeds the pool: {ranges:?}"
        );
    }

    #[test]
    fn every_range_holds_the_requested_block_count() {
        for &(start, end) in &distribute_ranges(1_000, 10_000, 250, 16) {
            assert_eq!(end - start + 1, 250);
        }
    }

    #[test]
    fn an_inverted_pool_is_rejected() {
        assert!(pool_size(2_000, 1_000).is_err());
        assert_eq!(pool_size(1_000, 1_000).ok(), Some(1));
        assert_eq!(pool_size(1_000, 1_999).ok(), Some(1_000));
    }

    /// At low counts the per-connection delay governs and the ramp is short. At
    /// high counts the window governs, so the round stays a concurrency
    /// measurement: 10,000 connections come up inside the window rather than
    /// 10s apart, which is what a fixed 1ms delay would have done.
    #[test]
    fn the_spawn_ramp_is_bounded_by_the_window() {
        let ramp =
            |connections: usize| spawn_gap(1, 2000, connections).as_secs_f64() * connections as f64;

        assert_eq!(
            spawn_gap(1, 2000, 100),
            Duration::from_millis(1),
            "at 100 connections the per-connection delay is the smaller bound"
        );
        assert!(
            ramp(100) < 0.2,
            "and the whole ramp is a fraction of a second"
        );

        assert_eq!(
            spawn_gap(1, 2000, 10_000),
            Duration::from_micros(200),
            "at 10,000 the window is the smaller bound: 2s / 10,000"
        );
        assert!(
            (ramp(10_000) - 2.0).abs() < 0.01,
            "so the ramp is the window, not the 10s a fixed 1ms delay would give"
        );
    }

    /// `Barrier::new(connections + 1)` would panic on a zero-party barrier, and
    /// the gap arithmetic divides by the connection count.
    #[test]
    fn a_degenerate_connection_count_is_handled() {
        assert_eq!(spawn_gap(1, 2000, 0), Duration::from_millis(1));
        assert_eq!(spawn_gap(1, 2000, 1), Duration::from_millis(1));
    }

    /// A zero window means "no ramp": spawn as fast as the loop allows.
    #[test]
    fn a_zero_window_removes_the_ramp() {
        assert!(spawn_gap(1, 0, 100).is_zero());
    }

    #[test]
    fn the_fd_warning_fires_only_below_the_requirement() {
        let limits = "Max open files            1024                 4096                 files\n";
        assert_eq!(parse_open_file_limit(limits), Some(1024));

        // 1024 fds cannot cover 1000 connections, but comfortably covers 100.
        let warning = fd_limit_warning(1024, 1000).expect("1024 fds is short of 2000");
        assert!(
            warning.contains("2000"),
            "warning should name the requirement: {warning}"
        );
        assert!(fd_limit_warning(1024, 100).is_none());
    }

    #[test]
    fn an_unparseable_limits_file_yields_no_limit() {
        assert_eq!(parse_open_file_limit("Max open files unlimited"), None);
        assert_eq!(parse_open_file_limit(""), None);
    }
}
