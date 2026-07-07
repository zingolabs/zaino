use clap::Args;

use crate::block_compare;
use crate::grpc_client;
use crate::{boxed_error, AdminResult};

/// Compare CompactBlock output from two lightwalletd servers.
#[derive(Args)]
pub(super) struct CompareArgs {
    /// First LWD server URL (e.g. "http://127.0.0.1:9067" or "https://zec.rocks:443").
    #[arg(long)]
    server_a: String,

    /// Second LWD server URL (e.g. "http://127.0.0.1:9067" or "https://zec.rocks:443").
    #[arg(long)]
    server_b: String,

    /// Start height (inclusive).
    #[arg(long, default_value = "0")]
    start_height: u64,

    /// End height (inclusive). Defaults to the latest block height from server A.
    #[arg(long)]
    end_height: Option<u64>,

    /// Print per-block comparison status instead of just the summary.
    #[arg(short, long)]
    verbose: bool,
}

pub(super) async fn run(args: CompareArgs) -> AdminResult<()> {
    tracing::info!("Connecting to {} and {}", args.server_a, args.server_b);

    let (mut client_a, mut client_b) =
        tokio::try_join!(async { grpc_client::connect_lazy(&args.server_a) }, async {
            grpc_client::connect_lazy(&args.server_b)
        },)?;

    tracing::info!("Connected to both servers.");

    let end_height = match args.end_height {
        Some(height) => height,
        None => {
            let latest = grpc_client::get_latest_height(&mut client_a).await?;
            tracing::info!("Latest block height from {}: {}", args.server_a, latest);
            latest
        }
    };

    if end_height < args.start_height {
        return Err(boxed_error(format!(
            "invalid block range: end height {} is less than start height {}",
            end_height, args.start_height
        )));
    }

    let block_count = end_height - args.start_height + 1;
    tracing::info!(
        "Fetching {} blocks ({}..{}) from both servers...",
        block_count,
        args.start_height,
        end_height
    );

    let (blocks_a, blocks_b) = tokio::try_join!(
        grpc_client::fetch_block_range(&mut client_a, args.start_height, end_height),
        grpc_client::fetch_block_range(&mut client_b, args.start_height, end_height),
    )?;

    let count_a = blocks_a.len();
    let count_b = blocks_b.len();

    if args.verbose {
        eprintln!(
            "Received {} blocks from {}, {} blocks from {}",
            count_a, args.server_a, count_b, args.server_b
        );
    }

    let result = block_compare::compare_blocks(blocks_a, blocks_b);

    eprintln!();
    eprintln!("Comparing blocks {}..{}", args.start_height, end_height);
    eprintln!("  Server A: {}", args.server_a);
    eprintln!("  Server B: {}", args.server_b);
    eprintln!();

    let total = result.matched as usize
        + result.mismatched.len()
        + result.missing_in_a.len()
        + result.missing_in_b.len();

    if result.matched == total as u64 {
        eprintln!("Matched: {}/{}", result.matched, total);
        eprintln!("All blocks identical.");
        return Ok(());
    }

    eprintln!("Matched: {}/{}", result.matched, total);

    if !result.missing_in_a.is_empty() {
        eprintln!("Missing from A ({} blocks):", result.missing_in_a.len());
        for height in &result.missing_in_a {
            eprintln!("  Height {}", height);
        }
    }

    if !result.missing_in_b.is_empty() {
        eprintln!("Missing from B ({} blocks):", result.missing_in_b.len());
        for height in &result.missing_in_b {
            eprintln!("  Height {}", height);
        }
    }

    if !result.mismatched.is_empty() {
        eprintln!("Mismatched: {}", result.mismatched.len());
        let mut current_height: Option<u64> = None;
        for diff in &result.mismatched {
            if current_height != Some(diff.height) {
                eprintln!("  Height {}:", diff.height);
                current_height = Some(diff.height);
            }
            eprintln!("    {}: A={}, B={}", diff.field, diff.value_a, diff.value_b);
        }
    }

    eprintln!();
    eprintln!(
        "Summary: {} matched, {} mismatched, {} missing from A, {} missing from B",
        result.matched,
        result.mismatched.len(),
        result.missing_in_a.len(),
        result.missing_in_b.len(),
    );

    Ok(())
}
