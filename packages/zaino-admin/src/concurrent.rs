use std::time::Instant;

use clap::Args;

use crate::{boxed_error, grpc_client, AdminResult};

/// Concurrent load test for a lightwalletd server.
///
/// Each of `--connections` clients fetches `--blocks` blocks starting at a
/// progressively offset height within `--start-height..--end-height`. When
/// block count × connections exceeds the available range, the windows slide
/// past each other (ranges overlap) — this is fine for load testing.
#[derive(Args)]
pub(super) struct ConcurrentTestArgs {
    /// Lightwalletd server URL (e.g. "http://127.0.0.1:9067").
    #[arg(short, long)]
    server: String,

    /// Start height (inclusive) — lower bound of the available block pool.
    #[arg(long)]
    start_height: u64,

    /// End height (inclusive) — upper bound of the available block pool.
    #[arg(long)]
    end_height: u64,

    /// Number of blocks each connection fetches.
    #[arg(short, long)]
    blocks: u64,

    /// Number of concurrent connections.
    #[arg(short, long)]
    connections: usize,

    /// Delay (ms) between spawning each connection to avoid SYN bursts.
    #[arg(long, default_value = "1")]
    spawn_delay_ms: u64,

    /// Print per-connection timing.
    #[arg(short, long)]
    verbose: bool,
}

struct ConnectionResult {
    index: usize,
    range_start: u64,
    range_end: u64,
    blocks: usize,
    connect_elapsed: std::time::Duration,
    fetch_elapsed: std::time::Duration,
    error: Option<String>,
    chain_breaks: usize,
}

pub(super) async fn run(args: ConcurrentTestArgs) -> AdminResult<()> {
    if args.end_height < args.start_height {
        return Err(boxed_error(format!(
            "end height {} is less than start height {}",
            args.end_height, args.start_height
        )));
    }

    if args.connections == 0 {
        return Err(boxed_error("--connections must be at least 1"));
    }

    if args.blocks == 0 {
        return Err(boxed_error("--blocks must be at least 1"));
    }

    let pool_size = args.end_height - args.start_height + 1;
    if args.blocks > pool_size {
        return Err(boxed_error(format!(
            "--blocks {} is larger than the available pool {}..{} ({} blocks)",
            args.blocks, args.start_height, args.end_height, pool_size
        )));
    }

    let overlap = args.blocks * args.connections as u64 > pool_size;
    eprintln!(
        "Pool: {}..{} ({} blocks)",
        args.start_height, args.end_height, pool_size
    );
    eprintln!(
        "{} connections × {} blocks each{}",
        args.connections,
        args.blocks,
        if overlap { " (ranges overlap)" } else { "" }
    );
    eprintln!();

    let wall_start = Instant::now();

    // Distribute ranges evenly across the pool: first connection starts at
    // `start`, last connection ends at `end`. When blocks × connections
    // exceeds the pool, ranges overlap — that's fine for load testing.
    let ranges: Vec<(u64, u64)> = {
        let n = args.connections;
        let step = if n > 1 {
            (pool_size - args.blocks) as f64 / (n - 1) as f64
        } else {
            0.0
        };
        (0..n)
            .map(|i| {
                let range_start = args.start_height + (step * i as f64).round() as u64;
                let range_end = (range_start + args.blocks - 1).min(args.end_height);
                (range_start, range_end)
            })
            .collect()
    };

    // Spawn connections with a small delay between them to avoid SYN floods.
    let mut handles = Vec::with_capacity(args.connections);
    for (i, (range_start, range_end)) in ranges.into_iter().enumerate() {
        let server = args.server.clone();
        handles.push(tokio::spawn(async move {
            let result_start = Instant::now();
            match fetch_one(&server, range_start, range_end).await {
                Ok((connect_elapsed, blocks, chain_breaks)) => ConnectionResult {
                    index: i,
                    range_start,
                    range_end,
                    blocks: blocks.len(),
                    connect_elapsed,
                    fetch_elapsed: result_start.elapsed() - connect_elapsed,
                    error: None,
                    chain_breaks,
                },
                Err((connect_elapsed, e)) => ConnectionResult {
                    index: i,
                    range_start,
                    range_end,
                    blocks: 0,
                    connect_elapsed,
                    fetch_elapsed: result_start.elapsed() - connect_elapsed,
                    error: Some(e.to_string()),
                    chain_breaks: 0,
                },
            }
        }));
        if args.spawn_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(args.spawn_delay_ms)).await;
        }
    }

    let results: Vec<ConnectionResult> = futures::future::join_all(handles)
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    let wall_elapsed = wall_start.elapsed();

    let errors: Vec<_> = results.iter().filter(|r| r.error.is_some()).collect();
    let ok: Vec<_> = results.iter().filter(|r| r.error.is_none()).collect();

    if args.verbose {
        for r in &results {
            let chain_info = if r.chain_breaks > 0 {
                format!(", {} chain break(s) ⚠", r.chain_breaks)
            } else {
                String::new()
            };
            match &r.error {
                None => {
                    eprintln!(
                        "  Connection {:>3}: {}..{} → {} blocks, connect {:.2}s, fetch {:.2}s{}",
                        r.index,
                        r.range_start,
                        r.range_end,
                        r.blocks,
                        r.connect_elapsed.as_secs_f64(),
                        r.fetch_elapsed.as_secs_f64(),
                        chain_info,
                    );
                }
                Some(e) => {
                    eprintln!(
                        "  Connection {:>3}: {}..{} → ERROR (connect {:.2}s) — {}",
                        r.index,
                        r.range_start,
                        r.range_end,
                        r.connect_elapsed.as_secs_f64(),
                        e
                    );
                }
            }
        }
        eprintln!();
    }

    if ok.is_empty() {
        eprintln!("All {} connections failed.", errors.len());
        return Ok(());
    }

    let connect_times: Vec<f64> = ok.iter().map(|r| r.connect_elapsed.as_secs_f64()).collect();
    let fetch_times: Vec<f64> = ok.iter().map(|r| r.fetch_elapsed.as_secs_f64()).collect();
    let total_times: Vec<f64> = ok
        .iter()
        .map(|r| (r.connect_elapsed + r.fetch_elapsed).as_secs_f64())
        .collect();
    let total_blocks_fetched: usize = ok.iter().map(|r| r.blocks).sum();
    let total_chain_breaks: usize = ok.iter().map(|r| r.chain_breaks).sum();

    fn stats(times: &[f64]) -> (f64, f64, f64) {
        let min = times.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = times.iter().cloned().fold(0.0_f64, f64::max);
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        (min, max, mean)
    }

    let (c_min, c_max, c_mean) = stats(&connect_times);
    let (f_min, f_max, f_mean) = stats(&fetch_times);
    let (t_min, t_max, t_mean) = stats(&total_times);

    eprintln!("Results:");
    eprintln!(
        "  Connections: {}/{}/{}  (success / failed / total)",
        ok.len(),
        errors.len(),
        args.connections,
    );
    eprintln!(
        "  Per connection: {} blocks ({} total fetched)",
        args.blocks, total_blocks_fetched,
    );
    eprintln!(
        "  Chain breaks: {}{}",
        total_chain_breaks,
        if total_chain_breaks > 0 { " ⚠" } else { "" }
    );
    eprintln!(
        "  Wall-clock time:        {:.2}s",
        wall_elapsed.as_secs_f64()
    );
    eprintln!();
    eprintln!(
        "  Connect time (s):           min {:>8.3}  mean {:>8.3}  max {:>8.3}",
        c_min, c_mean, c_max
    );
    eprintln!(
        "  Fetch time (s):             min {:>8.3}  mean {:>8.3}  max {:>8.3}",
        f_min, f_mean, f_max
    );
    eprintln!(
        "  Per-connection total (s):   min {:>8.3}  mean {:>8.3}  max {:>8.3}",
        t_min, t_mean, t_max
    );
    eprintln!();
    eprintln!(
        "  Aggregate throughput: {:.0} blocks/s across {} connections",
        total_blocks_fetched as f64 / wall_elapsed.as_secs_f64(),
        args.connections,
    );
    eprintln!(
        "  Per-connection throughput: {:.0} blocks/s (mean)",
        args.blocks as f64 / t_mean,
    );

    Ok(())
}

/// Connect, then fetch a sub-range. Returns (connect_duration, blocks, chain_breaks).
async fn fetch_one(
    server: &str,
    start: u64,
    end: u64,
) -> Result<
    (
        std::time::Duration,
        Vec<zaino_proto::proto::compact_formats::CompactBlock>,
        usize,
    ),
    (std::time::Duration, grpc_client::Error),
> {
    let connect_start = Instant::now();
    let mut client = match grpc_client::connect_eager(server).await {
        Ok(c) => c,
        Err(e) => return Err((connect_start.elapsed(), e)),
    };
    let connect_elapsed = connect_start.elapsed();

    match grpc_client::fetch_block_range(&mut client, start, end).await {
        Ok(blocks) => {
            let breaks = verify_chain(&blocks);
            Ok((connect_elapsed, blocks, breaks))
        }
        Err(e) => Err((connect_elapsed, e)),
    }
}

/// Verify that consecutive blocks in a sorted list have consistent prevHash
/// links. Returns the number of chain breaks found.
///
/// Blocks must be sorted by height (ascending). The genesis block (height 0)
/// must have `prevHash == [0u8; 32]`. For all other blocks, `prevHash` must
/// equal the previous block's `hash`.
fn verify_chain(blocks: &[zaino_proto::proto::compact_formats::CompactBlock]) -> usize {
    let mut breaks = 0;
    let mut prev_hash: Option<[u8; 32]> = None;
    let genesis_hash = [0u8; 32];

    for block in blocks {
        // Copy the 32-byte hashes; skip blocks with malformed hash lengths.
        let block_hash: [u8; 32] = match grpc_client::copy_hash(&block.hash) {
            Some(h) => h,
            None => {
                breaks += 1;
                prev_hash = None;
                continue;
            }
        };
        let block_prev_hash: [u8; 32] = match grpc_client::copy_hash(&block.prev_hash) {
            Some(h) => h,
            None => {
                breaks += 1;
                prev_hash = None;
                continue;
            }
        };

        // Genesis invariant.
        if block.height == 0 && block_prev_hash != genesis_hash {
            breaks += 1;
        }

        // Chain-link check.
        if let Some(expected_prev) = prev_hash {
            if block_prev_hash != expected_prev {
                breaks += 1;
            }
        }

        prev_hash = Some(block_hash);
    }

    breaks
}
