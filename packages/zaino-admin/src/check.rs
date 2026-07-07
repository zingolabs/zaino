//! Zaino chain integrity checker.
//!
//! Connects to a zainod gRPC server, streams every block in the requested
//! height range via `GetBlockRange`, and verifies that each block's `prevHash`
//! equals the previous block's `hash` — i.e. the chain is unbroken.
//!
//! ## How it works
//!
//! 1. Connects to the gRPC server and calls `GetLatestBlock` to discover the
//!    chain tip.
//! 2. Streams blocks via `GetBlockRange` in a single streaming call.
//! 3. Verifies `block.prev_hash == previous_block.hash` for every block.
//! 4. Reports every mismatch and prints a summary.

use clap::Args;
use futures::StreamExt;
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::service::{BlockId, BlockRange, ChainSpec};

use crate::{boxed_error, grpc_client, AdminResult};

/// Chain integrity checker — validates prev_hash links across all blocks.
#[derive(Args)]
pub(super) struct CheckArgs {
    /// Zainod gRPC server URL (e.g. "http://127.0.0.1:9067").
    #[arg(short, long)]
    server: String,

    /// Height to start checking from (default: 0).
    #[arg(long, default_value = "0")]
    start_height: u64,

    /// Height to stop checking at (default: chain tip).
    #[arg(long)]
    end_height: Option<u64>,

    /// Print progress every N blocks.
    #[arg(long, default_value = "100000")]
    progress_interval: u64,

    /// Stop after this many chain errors.
    #[arg(long, default_value = "10")]
    max_errors: usize,
}

pub(super) async fn run(args: CheckArgs) -> AdminResult<()> {
    // ----- Connect ----------------------------------------------------------
    let mut client = grpc_client::connect_eager(&args.server).await?;

    // ----- Discover tip -----------------------------------------------------
    let tip = client
        .get_latest_block(ChainSpec {})
        .await
        .map_err(|e| boxed_error(format!("GetLatestBlock: {e}")))?
        .into_inner()
        .height;

    let range_end = args.end_height.unwrap_or(tip);

    if args.start_height > range_end {
        eprintln!(
            "start_height {} > end_height {} — nothing to check",
            args.start_height, range_end
        );
        return Ok(());
    }

    eprintln!("Server:    {}", args.server);
    eprintln!("Chain tip: {tip}");
    eprintln!(
        "Checking range: {}..={} ({} blocks)",
        args.start_height,
        range_end,
        range_end - args.start_height + 1
    );
    eprintln!();

    // ----- Stream blocks and verify -----------------------------------------
    let request = BlockRange {
        start: Some(BlockId {
            height: args.start_height,
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: range_end,
            hash: Vec::new(),
        }),
        pool_types: Vec::new(),
    };

    let response = client
        .get_block_range(request)
        .await
        .map_err(|e| boxed_error(format!("GetBlockRange: {e}")))?;

    let mut stream = response.into_inner();

    let mut prev_block_hash: Option<[u8; 32]> = None;
    let mut checked: u64 = 0;
    let mut last_height: i64 = -1;
    let mut chain_breaks: Vec<ChainBreak> = Vec::new();
    let mut hash_len_errors: u64 = 0;

    let progress_interval = args.progress_interval.max(1);
    let genesis_hash = [0u8; 32];

    while let Some(result) = stream.next().await {
        let block: CompactBlock = match result {
            Ok(b) => b,
            Err(e) => {
                eprintln!("  Stream error: {e}");
                break;
            }
        };

        checked += 1;

        // ----- Validate block ordering --------------------------------------
        let height = block.height;
        if (height as i64) <= last_height {
            chain_breaks.push(ChainBreak {
                height,
                expected_prev_hash: [0u8; 32],
                actual_prev_hash: [0u8; 32],
                detail: format!(
                    "block height {} is not strictly after previous height {} — blocks out of order",
                    height, last_height
                ),
            });
            if chain_breaks.len() >= args.max_errors {
                break;
            }
        }
        last_height = height as i64;

        // ----- Validate hash fields have correct length ---------------------
        let block_hash: [u8; 32] = match grpc_client::copy_hash(&block.hash) {
            Some(h) => h,
            None => {
                hash_len_errors += 1;
                if hash_len_errors <= 3 {
                    eprintln!(
                        "  Bad hash length at height {height}: {} bytes (expected 32)",
                        block.hash.len()
                    );
                }
                prev_block_hash = None;
                maybe_progress(checked, progress_interval);
                continue;
            }
        };

        let prev_hash: [u8; 32] = match grpc_client::copy_hash(&block.prev_hash) {
            Some(h) => h,
            None => {
                hash_len_errors += 1;
                if hash_len_errors <= 3 {
                    eprintln!(
                        "  Bad prevHash length at height {height}: {} bytes (expected 32)",
                        block.prev_hash.len()
                    );
                }
                prev_block_hash = None;
                maybe_progress(checked, progress_interval);
                continue;
            }
        };

        // ----- Genesis invariant --------------------------------------------
        if height == 0 && prev_hash != genesis_hash {
            chain_breaks.push(ChainBreak {
                height: 0,
                expected_prev_hash: genesis_hash,
                actual_prev_hash: prev_hash,
                detail: "genesis block (height 0) must have prevHash = all-zeros".into(),
            });
            if chain_breaks.len() >= args.max_errors {
                break;
            }
        }

        // ----- Chain-link check ---------------------------------------------
        if let Some(expected_prev) = prev_block_hash {
            if prev_hash != expected_prev {
                chain_breaks.push(ChainBreak {
                    height,
                    expected_prev_hash: expected_prev,
                    actual_prev_hash: prev_hash,
                    detail: format!("prevHash does not match previous block's hash"),
                });
                if chain_breaks.len() >= args.max_errors {
                    break;
                }
            }
        }

        // Advance — next block must link to *this* block's hash.
        prev_block_hash = Some(block_hash);
        maybe_progress(checked, progress_interval);
    }

    eprintln!();

    // ----- Report ----------------------------------------------------------
    let total_errors = chain_breaks.len() + hash_len_errors as usize;

    eprintln!("══════════════════════════════════════════");
    eprintln!("  Chain Integrity Check — Summary");
    eprintln!("══════════════════════════════════════════");
    eprintln!("  Blocks checked:     {checked}");
    eprintln!("  Chain breaks:       {}", chain_breaks.len());
    eprintln!("  Hash length errors: {hash_len_errors}");
    eprintln!("  Total errors:       {total_errors}");
    eprintln!();

    for cb in &chain_breaks {
        eprintln!(
            "  CHAIN BREAK at height {}: expected {:?}, got {:?} — {}",
            cb.height, cb.expected_prev_hash, cb.actual_prev_hash, cb.detail
        );
    }

    if total_errors > 0 {
        eprintln!();
        eprintln!("  ❌ Chain is INVALID — {total_errors} error(s) found.");
        return Err(boxed_error(format!(
            "chain is invalid: {total_errors} error(s) found"
        )));
    }

    eprintln!("  ✅ Chain is VALID — all {checked} blocks link correctly.");
    Ok(())
}

// =============================================================================
// Helpers
// =============================================================================

fn maybe_progress(checked: u64, interval: u64) {
    if checked % interval == 0 && checked > 0 {
        eprintln!("  ... {checked} blocks checked ...");
    }
}

// =============================================================================
// Error detail types
// =============================================================================

#[derive(Debug)]
struct ChainBreak {
    height: u64,
    expected_prev_hash: [u8; 32],
    actual_prev_hash: [u8; 32],
    detail: String,
}
