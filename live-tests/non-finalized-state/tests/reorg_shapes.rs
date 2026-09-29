//! Every index serves the validator's chain after a reorg, whatever the reorg's shape
//!
//! - Branch A → configured Orchard miner (Ironwood commitments), B → `FILLER_ADDRESS` (none)
//! - Contracts C1–C5, C11, C12 in `docs/design/non-finalized-state-tests.md`

use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::json;
use ztest::backends::zainod::family;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
const SCRAPE: Duration = Duration::from_secs(10);
const DEPTH: u32 = 10;
/// Durable band of `2 × DEPTH` below the window
const CHAIN: u32 = 3 * DEPTH;
/// `PoolType` wire enum: transparent, sapling, orchard, ironwood
const ALL_POOLS: [i32; 4] = [1, 2, 3, 4];

#[rstest]
#[case::tip_swap(1, 1)]
#[case::to_a_longer_branch(3, 5)]
#[case::to_a_lower_tip(5, 2)]
#[case::retreat(3, 0)]
#[case::retreat_onto_the_durable_tip(DEPTH, 0)]
#[case::whole_window(DEPTH, DEPTH + 1)]
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn every_index_serves_the_validators_chain_after_a_reorg(
    #[case] depth: u32,
    #[case] len: u32,
) -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile", features = ["prometheus"])
            .regtest()
            .finalised_depth(DEPTH),
    );
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    let old_tip = validator.tip().await?;
    indexer.wait_for_tip(old_tip, CONVERGE).await?;
    let old_branch =
        indexer.get_block_range_with_pools(1u32.into(), old_tip.0, ALL_POOLS.to_vec()).await?;
    let old_trees = indexer.get_latest_tree_state().await?;
    let reorgs = indexer.read(SCRAPE).await?.counter_total(family::REORGS);
    assert_eq!(reorgs, Some(0), "zaino_reorgs_total published at 0 before any reorg");

    let reorg = validator.reorg(depth, len, Some(FILLER_ADDRESS)).await?;
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;
    let (fork_parent, tip) = (u32::from(reorg.fork_parent.0), u32::from(reorg.tip.0));
    let fork = fork_parent + 1;

    let served =
        indexer.get_block_range_with_pools(1u32.into(), reorg.tip.0, ALL_POOLS.to_vec()).await?;
    let vrpc = validator.json_rpc().await?;
    assert_eq!(served.len(), tip as usize, "one block per height in [1, {tip}] (fork {fork})");
    for (block, height) in served.iter().zip(1u32..) {
        let truth = vrpc.call_value("getblockhash", json!([height])).await?;
        let hash = BlockHash::from_wire(&block.hash).context("served hash is 32 bytes")?;
        assert_eq!(block.height, u64::from(height), "range order (fork {fork})");
        assert_eq!(Some(hash.to_string().as_str()), truth.as_str(), "{height} (fork {fork})");
    }
    for pair in served.windows(2) {
        assert_eq!(pair[1].prev_hash, pair[0].hash, "{} unlinked (fork {fork})", pair[1].height);
    }
    let below = fork_parent as usize;
    assert_eq!(served[..below], old_branch[..below], "blocks below fork {fork} byte-identical");

    for (height, hash) in &reorg.orphaned {
        let refused = indexer.get_block_by_hash(*hash).await.err().map(|e| e.grpc_code());
        let height = u32::from(*height);
        assert_eq!(refused, Some(Some(tonic::Code::NotFound)), "orphan {hash} at {height}");
    }
    for height in tip + 1..=u32::from(old_tip.0) {
        let answer = indexer.get_block(height.into()).await;
        let code = answer.as_ref().err().and_then(RpcError::grpc_code);
        assert!(
            answer.is_err() && code != Some(tonic::Code::Unavailable),
            "{height} above lowered tip {tip} refused as out of range, got {:?}",
            answer.map(|b| b.hash).map_err(|e| e.to_string()),
        );
    }

    let empty = |tree: &str| if tree.is_empty() { "000000".to_owned() } else { tree.to_owned() };
    for height in fork_parent..=tip {
        let zaino = indexer.get_tree_state(height.into()).await?;
        let zebra = vrpc.call_value("z_gettreestate", json!([height.to_string()])).await?;
        let final_state = |pool: &str| {
            empty(zebra[pool]["commitments"]["finalState"].as_str().unwrap_or_default())
        };
        let served = [
            zaino.hash.clone(),
            zaino.time.to_string(),
            empty(&zaino.sapling_tree),
            empty(&zaino.orchard_tree),
            empty(&zaino.ironwood_tree),
        ];
        let truth = [
            zebra["hash"].as_str().unwrap_or_default().to_owned(),
            zebra["time"].to_string(),
            final_state("sapling"),
            final_state("orchard"),
            final_state("ironwood"),
        ];
        assert_eq!(served, truth, "[hash, time, sapling, orchard, ironwood] at {height}");
    }
    let latest = indexer.get_latest_tree_state().await?;
    let parent_trees = indexer.get_tree_state(fork_parent.into()).await?;
    assert_eq!(latest.height, u64::from(tip), "GetLatestTreeState on the new tip");
    assert_ne!(
        old_trees.ironwood_tree, parent_trees.ironwood_tree,
        "branch A appended Ironwood commitments past the fork (else the reorg changes no tree)",
    );
    assert_eq!(
        latest.ironwood_tree, parent_trees.ironwood_tree,
        "branch B appends none → tree rolled back to the fork parent's",
    );

    let scrape = indexer.read(SCRAPE).await?;
    assert_eq!(scrape.counter_total(family::REORGS), Some(1), "one rollback counted");
    assert_eq!(scrape.height(family::BEST_TIP), Some(tip), "zaino_best_tip");
    for index in ZainoIndex::ALL {
        assert_eq!(indexer.synced(index).await?, Some(true), "{index:?} gate reopened");
    }
    let durable = indexer.finalized_height(ZainoIndex::CompactBlock).await?;
    let old_durable = u32::from(old_tip.0) - DEPTH;
    assert!(old_durable < fork, "pre-reorg durable {old_durable} below fork {fork} (in window)");
    let expected = Some(old_durable.max(tip - DEPTH));
    assert_eq!(durable, expected, "durable = max(old_tip, tip) − DEPTH, monotone (fork {fork})");
    assert_eq!(indexer.indexer_info().await?.block_height, u64::from(tip), "GetLightdInfo");
    Ok(())
}
