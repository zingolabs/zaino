//! Single-stream serve rate — "how fast can you serve blocks?"
//!
//! Ported from the `zaino-admin check` tool on the `hahn/store` branch. Streams
//! one large `GetBlockRange` and verifies the chain links as blocks arrive, so
//! the run reports serve rate and correctness from the same pass: a fast answer
//! that does not link up is not an answer.

use std::time::Instant;

use clap::Args;
use futures::StreamExt;
use zaino_proto::proto::compact_formats::CompactBlock;

use crate::chain::ChainVerifier;
use crate::error::BenchError;
use crate::grpc_client;

/// Stream a height range from one connection and report blocks/s and bytes/s.
#[derive(Args)]
pub(super) struct ServeArgs {
    /// Server under test (e.g. "http://127.0.0.1:8137").
    #[arg(short, long)]
    server: String,

    /// Height to start streaming from.
    #[arg(long, default_value = "0")]
    start_height: u64,

    /// Height to stop at. Defaults to the server's chain tip.
    #[arg(long)]
    end_height: Option<u64>,

    /// Print progress every N blocks.
    #[arg(long, default_value = "100000")]
    progress_interval: u64,

    /// Stop after this many chain errors.
    #[arg(long, default_value = "10")]
    max_errors: usize,
}

/// What the stream delivered.
struct Delivered {
    blocks: u64,
    bytes: u64,
    verifier: ChainVerifier,
    stream_error: Option<String>,
}

pub(super) async fn run(args: ServeArgs) -> Result<(), BenchError> {
    let mut client = grpc_client::connect_eager(&args.server).await?;
    let tip = grpc_client::get_latest_height(&mut client).await?;
    let end_height = args.end_height.unwrap_or(tip);

    if args.start_height > end_height {
        return Err(BenchError::Args(format!(
            "--start-height {} is above --end-height {end_height}",
            args.start_height
        )));
    }

    eprintln!("Server:    {}", args.server);
    eprintln!("Chain tip: {tip}");
    eprintln!(
        "Streaming: {}..={end_height} ({} blocks)",
        args.start_height,
        end_height - args.start_height + 1,
    );
    eprintln!();

    // The clock starts at the request, not at the connect: connect cost is the
    // load test's concern, and folding it in here would understate serve rate on
    // short ranges.
    let started = Instant::now();
    let stream =
        grpc_client::block_range_stream(&mut client, args.start_height, end_height).await?;
    let delivered = drain(stream, args.progress_interval.max(1), args.max_errors).await;
    let elapsed = started.elapsed();

    report(&delivered, elapsed);

    let total_errors = delivered.verifier.total_errors();
    if total_errors > 0 {
        return Err(BenchError::InvalidChain(total_errors));
    }
    Ok(())
}

async fn drain(
    mut stream: tonic::Streaming<CompactBlock>,
    progress_interval: u64,
    max_errors: usize,
) -> Delivered {
    let mut delivered = Delivered {
        blocks: 0,
        bytes: 0,
        verifier: ChainVerifier::new(),
        stream_error: None,
    };

    while let Some(item) = stream.next().await {
        let block = match item {
            Ok(block) => block,
            Err(status) => {
                // A mid-stream failure is itself a result worth reporting — the
                // `hahn/store` numbers show exactly this under load — so record
                // it and summarise what did arrive rather than bailing out.
                delivered.stream_error = Some(status.to_string());
                break;
            }
        };

        delivered.blocks += 1;
        delivered.bytes += wire_size(&block);
        delivered.verifier.push(&block);

        if delivered.verifier.breaks().len() >= max_errors {
            break;
        }
        if delivered.blocks.is_multiple_of(progress_interval) {
            eprintln!("  ... {} blocks streamed ...", delivered.blocks);
        }
    }

    delivered
}

/// Approximate served bytes for a compact block.
///
/// `prost::Message::encoded_len` would need a `prost` dependency purely to
/// re-measure what the server already sent; summing the variable-length fields
/// tracks the payload closely enough for a throughput figure, and the report
/// labels it as approximate.
fn wire_size(block: &CompactBlock) -> u64 {
    let header = (block.hash.len() + block.prev_hash.len() + block.header.len()) as u64;
    let transactions: u64 = block
        .vtx
        .iter()
        .map(|tx| {
            let spends: usize = tx.spends.iter().map(|spend| spend.nf.len()).sum();
            let outputs: usize = tx
                .outputs
                .iter()
                .map(|output| {
                    output.cmu.len() + output.ephemeral_key.len() + output.ciphertext.len()
                })
                .sum();
            let actions: usize = tx
                .actions
                .iter()
                .map(|action| {
                    action.nullifier.len()
                        + action.cmx.len()
                        + action.ephemeral_key.len()
                        + action.ciphertext.len()
                })
                .sum();
            (tx.txid.len() + spends + outputs + actions) as u64
        })
        .sum();

    header + transactions
}

fn report(delivered: &Delivered, elapsed: std::time::Duration) {
    let seconds = elapsed.as_secs_f64();

    eprintln!();
    if let Some(error) = &delivered.stream_error {
        eprintln!("  Stream error after {} blocks: {error}", delivered.blocks);
        eprintln!();
    }

    eprintln!("══════════════════════════════════════════");
    eprintln!("  Single-Stream Serve Rate — Summary");
    eprintln!("══════════════════════════════════════════");
    eprintln!("  Blocks streamed:    {}", delivered.blocks);
    eprintln!("  Wall-clock time:    {seconds:.2}s");
    if seconds > 0.0 {
        eprintln!(
            "  Serve rate:         {:.0} blocks/s",
            delivered.blocks as f64 / seconds
        );
        eprintln!(
            "  Payload rate:       {:.1} MB/s (approx)",
            delivered.bytes as f64 / seconds / 1_000_000.0
        );
    }
    eprintln!(
        "  Payload:            {:.1} MB (approx)",
        delivered.bytes as f64 / 1_000_000.0
    );
    eprintln!(
        "  Chain breaks:       {}",
        delivered.verifier.breaks().len()
    );
    eprintln!(
        "  Hash length errors: {}",
        delivered.verifier.hash_length_errors()
    );
    eprintln!(
        "  Total errors:       {}",
        delivered.verifier.total_errors()
    );
    eprintln!();

    for chain_break in delivered.verifier.breaks() {
        eprintln!(
            "  CHAIN BREAK at height {}: {}",
            chain_break.height, chain_break.detail
        );
    }

    if delivered.verifier.total_errors() == 0 {
        eprintln!(
            "  ✅ Chain is VALID — all {} blocks link correctly.",
            delivered.blocks
        );
    } else {
        eprintln!(
            "  ❌ Chain is INVALID — {} error(s) found.",
            delivered.verifier.total_errors()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_proto::proto::compact_formats::{CompactSaplingOutput, CompactTx};

    #[test]
    fn wire_size_counts_header_and_transaction_payload() {
        let block = CompactBlock {
            height: 1,
            hash: vec![0u8; 32],
            prev_hash: vec![0u8; 32],
            header: vec![0u8; 100],
            vtx: vec![CompactTx {
                txid: vec![0u8; 32],
                outputs: vec![CompactSaplingOutput {
                    cmu: vec![0u8; 32],
                    ephemeral_key: vec![0u8; 32],
                    ciphertext: vec![0u8; 52],
                }],
                ..CompactTx::default()
            }],
            ..CompactBlock::default()
        };

        // 32 + 32 + 100 header, then 32 + (32 + 32 + 52) for the one transaction.
        assert_eq!(wire_size(&block), 164 + 148);
    }

    #[test]
    fn an_empty_block_has_only_its_header() {
        let block = CompactBlock {
            hash: vec![0u8; 32],
            prev_hash: vec![0u8; 32],
            ..CompactBlock::default()
        };
        assert_eq!(wire_size(&block), 64);
    }
}
