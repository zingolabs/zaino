//! Reads that cross zaino's final / non-final boundary
//!
//! - Below `tip + 1 − finalised_depth` → durable index, above → non-finalized state
//! - Spanning range = stitched from both (only place a served chain can gain a gap, a duplicate,
//!   or two mismatched halves)
//! - Shipped depth (1000) out of any regtest fixture's reach → ztest's regtest zainod runs at
//!   `REGTEST_FINALISED_DEPTH` (100)

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const FINALISE_TIMEOUT: Duration = Duration::from_secs(300);
/// Leaves a 60-block finalised band below the seam and the 100-block window above it.
const CHAIN_LEN: u32 = 160;
const FAST_SEAM: u32 = ztest::backends::zainod::REGTEST_FINALISED_DEPTH;

/// Finalised past the seam, then every read shape across it = one chain = the validator's
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn reads_across_the_seam_serve_the_validators_chain() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(CHAIN_LEN).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    let tip = u32::from(tip);
    let floor = tip.saturating_sub(FAST_SEAM);
    assert!(floor > 0, "fixture too short for a finalised band: tip {tip}, seam {FAST_SEAM}");

    // - Load-bearing: nothing finalised → every read below stays on one side (vacuous)
    // - Unpublished until the first commit → keep polling
    let deadline = tokio::time::Instant::now() + FINALISE_TIMEOUT;
    let frontier = loop {
        let finalised = indexer.finalized_height(ZainoIndex::CompactBlock).await?;
        if let Some(finalised) = finalised.filter(|f| *f >= floor) {
            break finalised;
        }
        let expired = tokio::time::Instant::now() >= deadline;
        anyhow::ensure!(!expired, "finalised {finalised:?}, never {floor} in {FINALISE_TIMEOUT:?}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    // Finalised tip = reorg-stability claimed for blocks consensus can still roll back
    assert!(frontier < tip, "finalised frontier {frontier} must trail tip {tip}");

    // `prev_hash` links (height contiguity alone passes the right heights off the wrong chain)
    let blocks = indexer.get_block_range(BlockHeight::from(1u32), BlockHeight::from(tip)).await?;
    assert_eq!(blocks.len(), tip as usize, "every height in [1, {tip}]");
    for (offset, block) in blocks.iter().enumerate() {
        assert_eq!(block.height, (offset + 1) as u64, "no gap or repeat (floor {floor})");
    }
    for pair in blocks.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        assert_eq!(next.prev_hash, prev.hash, "{} unlinked (floor {floor})", next.height);
    }

    let straddling = [floor - 1, floor, floor + 1, floor + 2];
    let spanning =
        indexer.get_block_range(BlockHeight::from(floor - 1), BlockHeight::from(floor + 2)).await?;
    assert_eq!(spanning.len(), straddling.len(), "all four straddling heights");
    for (height, from_span) in straddling.iter().zip(spanning.iter()) {
        assert_eq!(from_span.height, u64::from(*height), "spanning range order");

        let confined =
            indexer.get_block_range(BlockHeight::from(*height), BlockHeight::from(*height)).await?;
        assert_eq!(confined.len(), 1, "single-height range at {height}");
        assert_eq!(&confined[0], from_span, "{height}: confined vs spanning (floor {floor})");

        let single = indexer.get_block(BlockHeight::from(*height)).await?;
        assert_eq!(&single, from_span, "{height}: GetBlock vs spanning (floor {floor})");
    }

    // Descending split = separate code from the ascending one
    let (low, high) = (floor - 5, floor + 5);
    let ascending =
        indexer.get_block_range(BlockHeight::from(low), BlockHeight::from(high)).await?;
    let descending =
        indexer.get_block_range(BlockHeight::from(high), BlockHeight::from(low)).await?;
    assert_eq!(descending.len(), ascending.len(), "count over [{low}, {high}]");
    let reversed: Vec<_> = descending.iter().rev().cloned().collect();
    assert_eq!(reversed, ascending, "descending = ascending reversed (floor {floor})");

    // Validator = oracle for the finalised half too (else only ever compared against zaino)
    let vrpc = validator.json_rpc().await?;
    let all_pools = vec![1, 2, 3, 4];
    let (low, high) = (BlockHeight::from(floor - 3), BlockHeight::from(floor + 3));
    for block in indexer.get_block_range_with_pools(low, high, all_pools).await? {
        let hash = vrpc.call_value("getblockhash", json!([block.height])).await?;
        // Wire = internal byte order, JSON-RPC = reversed
        let mut served = block.hash.clone();
        served.reverse();
        let expected = hash.as_str().context("getblockhash returns a hex string")?;
        let height = block.height;
        assert_eq!(hex::encode(&served), expected, "{height} (floor {floor})");
    }
    Ok(())
}
