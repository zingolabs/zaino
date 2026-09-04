//! Reads that cross zaino's finalised / non-finalised boundary.
//!
//! Heights at or below `tip - MAX_NONFINALISED_DEPTH` are served from the finalised
//! index, everything above from an in-memory cache. A range spanning the boundary is
//! stitched from both, so the seam is a real join in the code — the only place a served
//! chain can acquire a gap, a duplicate, or two mismatched halves.
//!
//! The operational depth is 1001 blocks, which no regtest fixture reaches, so every
//! indexer here is built with `fast-test-seam` (100). That is a separate image with its
//! own tag and is not an operational build.
//!
//! `finalised_index_advances_past_the_seam` is load-bearing: without the feature nothing
//! finalises and every assertion below holds without crossing anything.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use zaino_testutils::wait_for_finalised;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const FINALISE_TIMEOUT: Duration = Duration::from_secs(300);
/// Leaves a 60-block finalised band below the seam and the 100-block window above it.
const CHAIN_LEN: u32 = 160;
/// `FAST_TEST_MAX_NONFINALISED_DEPTH` under `fast-test-seam`. Mirrored rather than
/// imported: this workspace links no production crate.
const FAST_SEAM: u32 = 100;

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn finalised_index_advances_past_the_seam() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(
        dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = [
                "no_tls_use_unencrypted_traffic",
                "allow_unencrypted_public_json_rpc_bind",
                "prometheus",
                "fast-test-seam",
            ]
        )
        .regtest(),
    );
    env.build().await?;

    let tip = validator.generate_blocks(CHAIN_LEN).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    let tip = u32::from(tip);
    let floor = tip.saturating_sub(FAST_SEAM);

    assert!(
        floor > 0,
        "fixture too short to have a finalised band: tip {tip}, seam depth {FAST_SEAM}"
    );

    let frontier = wait_for_finalised(&indexer, floor, FINALISE_TIMEOUT).await?;

    // Finalising the tip would claim reorg-stability for blocks consensus can still roll back.
    assert!(
        frontier < tip,
        "the finalised index must trail the tip by the reorg buffer, but frontier \
         {frontier} reached tip {tip}"
    );
    Ok(())
}

/// The stitch must produce one chain: no gap, no duplicate, every `prev_hash` pointing at
/// its predecessor. Height contiguity alone would not catch a stitch that served the
/// right heights off the wrong chain.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn range_across_the_seam_is_one_unbroken_chain() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(
        dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = [
                "no_tls_use_unencrypted_traffic",
                "allow_unencrypted_public_json_rpc_bind",
                "prometheus",
                "fast-test-seam",
            ]
        )
        .regtest(),
    );
    env.build().await?;

    let tip = validator.generate_blocks(CHAIN_LEN).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    let tip = u32::from(tip);
    let floor = tip.saturating_sub(FAST_SEAM);
    wait_for_finalised(&indexer, floor, FINALISE_TIMEOUT).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), BlockHeight::from(tip))
        .await?;
    assert_eq!(
        blocks.len(),
        tip as usize,
        "a range spanning the seam must serve every height in [1, {tip}]"
    );

    for (offset, block) in blocks.iter().enumerate() {
        assert_eq!(
            block.height,
            (offset + 1) as u64,
            "served heights must ascend without gap or repeat across the seam (floor {floor})"
        );
    }
    for pair in blocks.windows(2) {
        assert_eq!(
            pair[1].prev_hash, pair[0].hash,
            "block {} does not link to block {} — the seam stitch joined two different \
             chains (floor {floor})",
            pair[1].height, pair[0].height
        );
    }

    Ok(())
}

/// A height must not depend on which side of the seam the request fell on: each
/// straddling height is fetched inside a spanning range, inside a range confined to its
/// own side, and as a single read, and all three must agree exactly.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn seam_blocks_are_identical_whichever_side_serves_them() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(
        dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = [
                "no_tls_use_unencrypted_traffic",
                "allow_unencrypted_public_json_rpc_bind",
                "prometheus",
                "fast-test-seam",
            ]
        )
        .regtest(),
    );
    env.build().await?;

    let tip = validator.generate_blocks(CHAIN_LEN).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    let floor = u32::from(tip).saturating_sub(FAST_SEAM);
    wait_for_finalised(&indexer, floor, FINALISE_TIMEOUT).await?;

    let straddling = [floor - 1, floor, floor + 1, floor + 2];
    let spanning = indexer
        .get_block_range(BlockHeight::from(floor - 1), BlockHeight::from(floor + 2))
        .await?;
    assert_eq!(
        spanning.len(),
        straddling.len(),
        "the spanning request must serve all four straddling heights"
    );

    for (height, from_span) in straddling.iter().zip(spanning.iter()) {
        assert_eq!(from_span.height, u64::from(*height), "spanning range order");

        let confined = indexer
            .get_block_range(BlockHeight::from(*height), BlockHeight::from(*height))
            .await?;
        assert_eq!(confined.len(), 1, "single-height range at {height}");
        assert_eq!(
            &confined[0], from_span,
            "height {height} differs between a seam-spanning request and one confined \
             to its own side (floor {floor})"
        );

        let single = indexer.get_block(BlockHeight::from(*height)).await?;
        assert_eq!(
            &single, from_span,
            "height {height} differs between GetBlock and a seam-spanning GetBlockRange \
             (floor {floor})"
        );
    }
    Ok(())
}

/// The descending branch of the range split is separate code from the ascending one.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn descending_range_across_the_seam_reverses_the_ascending_one() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(
        dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = [
                "no_tls_use_unencrypted_traffic",
                "allow_unencrypted_public_json_rpc_bind",
                "prometheus",
                "fast-test-seam",
            ]
        )
        .regtest(),
    );
    env.build().await?;

    let tip = validator.generate_blocks(CHAIN_LEN).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    let floor = u32::from(tip).saturating_sub(FAST_SEAM);
    wait_for_finalised(&indexer, floor, FINALISE_TIMEOUT).await?;

    let (low, high) = (floor - 5, floor + 5);
    let ascending = indexer
        .get_block_range(BlockHeight::from(low), BlockHeight::from(high))
        .await?;
    let descending = indexer
        .get_block_range(BlockHeight::from(high), BlockHeight::from(low))
        .await?;

    assert_eq!(
        descending.len(),
        ascending.len(),
        "descending and ascending requests over [{low}, {high}] must serve the same count"
    );
    let reversed: Vec<_> = descending.iter().rev().cloned().collect();
    assert_eq!(
        reversed, ascending,
        "a descending range across the seam must be the ascending one reversed (floor {floor})"
    );
    Ok(())
}

/// Keeps the validator as the oracle for the finalised half, which is otherwise only ever
/// compared against zaino itself.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn served_blocks_match_the_validator_on_both_sides_of_the_seam() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(
        dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = [
                "no_tls_use_unencrypted_traffic",
                "allow_unencrypted_public_json_rpc_bind",
                "prometheus",
                "fast-test-seam",
            ]
        )
        .regtest(),
    );
    env.build().await?;

    let tip = validator.generate_blocks(CHAIN_LEN).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    let floor = u32::from(tip).saturating_sub(FAST_SEAM);
    wait_for_finalised(&indexer, floor, FINALISE_TIMEOUT).await?;

    let vrpc = validator.json_rpc().await?;
    let blocks = indexer
        .get_block_range_with_pools(
            BlockHeight::from(floor - 3),
            BlockHeight::from(floor + 3),
            // zaino's `PoolType` wire enum, every pool — matches `PoolTypeFilter::default`.
            vec![1, 2, 3, 4],
        )
        .await?;

    for block in &blocks {
        let hash = vrpc
            .call_value("getblockhash", json!([block.height]))
            .await?;
        // Wire carries internal byte order; JSON-RPC uses the reverse.
        let mut served = block.hash.clone();
        served.reverse();
        assert_eq!(
            zaino_testutils::hex::encode(&served),
            hash.as_str().context("getblockhash returns a hex string")?,
            "served block at height {} is not the validator's block at that height \
             (seam floor {floor})",
            block.height
        );
    }
    Ok(())
}
