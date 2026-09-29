//! Restart = replay from durable: identical to living through the reorg, and follows one missed
//!
//! - Pre-commit memory only → a restart rebuilds the window from the validator's current chain
//! - Down across a reorg = `freeze` then kill (kubelet restarts a first crash at once)
//! - Contracts C10, C13 in `docs/design/non-finalized-state-tests.md`

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use serde_json::json;
use ztest::backends::zainod::family;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
/// kubelet crash backoff (first restart immediate, 10 s after) + zainod reopen
const RESTART: Duration = Duration::from_secs(120);
const HALT_WINDOW: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_secs(1);
const SCRAPE: Duration = Duration::from_secs(10);
const DEPTH: u32 = 10;
const CHAIN: u32 = 3 * DEPTH;
const ORPHANED: u32 = 4;
const REPLACEMENT: u32 = 6;
/// `PoolType` wire enum: transparent, sapling, orchard, ironwood
const ALL_POOLS: [i32; 4] = [1, 2, 3, 4];

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn restart_after_a_reorg_serves_identically() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile", features = ["prometheus"])
            .regtest()
            .finalised_depth(DEPTH)
            .restartable(),
    );
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    indexer.wait_for_tip(validator.tip().await?, CONVERGE).await?;
    let reorg = validator.reorg(ORPHANED, REPLACEMENT, Some(FILLER_ADDRESS)).await?;
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;
    let tip = u32::from(reorg.tip.0);

    let lived_blocks =
        indexer.get_block_range_with_pools(1u32.into(), reorg.tip.0, ALL_POOLS.to_vec()).await?;
    let mut lived_trees = Vec::new();
    for height in 1..=tip {
        lived_trees.push(indexer.get_tree_state(height.into()).await?);
    }
    let lived_durable = indexer.finalized_height(ZainoIndex::CompactBlock).await?;

    let restart = indexer.kill_and_await_restart(RESTART).await?;
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;

    let replayed_blocks =
        indexer.get_block_range_with_pools(1u32.into(), reorg.tip.0, ALL_POOLS.to_vec()).await?;
    let mut replayed_trees = Vec::new();
    for height in 1..=tip {
        replayed_trees.push(indexer.get_tree_state(height.into()).await?);
    }
    assert!(replayed_blocks == lived_blocks, "blocks [1, {tip}] identical after {restart:?}");
    assert_eq!(replayed_trees, lived_trees, "tree states [1, {tip}] after {restart:?}");
    assert_eq!(
        indexer.finalized_height(ZainoIndex::CompactBlock).await?,
        lived_durable,
        "durable extent reopened as committed",
    );
    for (_, hash) in &reorg.orphaned {
        let code = indexer.get_block_by_hash(*hash).await.err().and_then(|e| e.grpc_code());
        assert_eq!(code, Some(tonic::Code::NotFound), "orphan {hash} stays gone after restart");
    }
    Ok(())
}

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn reorg_while_down_is_followed_on_restart() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile", features = ["prometheus"])
            .regtest()
            .finalised_depth(DEPTH)
            .restartable(),
    );
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    indexer.wait_for_tip(validator.tip().await?, CONVERGE).await?;

    let pod = indexer.pod().await?;
    pod.freeze().await?;
    let reorg = validator.reorg(ORPHANED, REPLACEMENT, Some(FILLER_ADDRESS)).await?;
    let restart = pod.kill_and_await_restart(RESTART).await?;
    ensure!(restart.restart_count >= 1, "zainod restarted after the frozen reorg: {restart:?}");
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;
    let (fork_parent, tip) = (u32::from(reorg.fork_parent.0), u32::from(reorg.tip.0));

    let vrpc = validator.json_rpc().await?;
    let served = indexer.get_block_range(1u32.into(), reorg.tip.0).await?;
    assert_eq!(served.len(), tip as usize, "one block per height in [1, {tip}]");
    for (block, height) in served.iter().zip(1u32..) {
        let truth = vrpc.call_value("getblockhash", json!([height])).await?;
        let hash = BlockHash::from_wire(&block.hash).context("served hash is 32 bytes")?;
        assert_eq!(Some(hash.to_string().as_str()), truth.as_str(), "block at {height}");
    }
    let empty = |tree: &str| if tree.is_empty() { "000000".to_owned() } else { tree.to_owned() };
    for height in fork_parent..=tip {
        let zaino = indexer.get_tree_state(height.into()).await?;
        let zebra = vrpc.call_value("z_gettreestate", json!([height.to_string()])).await?;
        let truth = zebra["ironwood"]["commitments"]["finalState"].as_str().unwrap_or_default();
        assert_eq!(empty(&zaino.ironwood_tree), empty(truth), "ironwood tree at {height}");
    }
    for (_, hash) in &reorg.orphaned {
        let code = indexer.get_block_by_hash(*hash).await.err().and_then(|e| e.grpc_code());
        assert_eq!(code, Some(tonic::Code::NotFound), "orphan {hash} never served");
    }
    let reorgs = indexer.read(SCRAPE).await?.counter_total(family::REORGS);
    assert_eq!(reorgs, Some(0), "replay from durable, no rollback (restart = reorg path)");
    Ok(())
}

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn reorg_below_durable_while_down_halts_on_restart() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile", features = ["prometheus"])
            .regtest()
            .finalised_depth(DEPTH)
            .restartable(),
    );
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    indexer.wait_for_tip(validator.tip().await?, CONVERGE).await?;
    let durable = indexer.finalized_height(ZainoIndex::CompactBlock).await?.unwrap_or(0);

    let pod = indexer.pod().await?;
    pod.freeze().await?;
    let tip = u32::from(validator.tip().await?.0);
    let reorg = validator.reorg(tip - durable + 1, tip - durable + 2, Some(FILLER_ADDRESS)).await?;
    let fork = u32::from(reorg.fork_parent.0) + 1;
    ensure!(fork <= durable, "fixture: fork {fork} must rewrite durable height {durable}");
    let mut winners = HashSet::new();
    for height in fork..=u32::from(reorg.tip.0) {
        winners.insert(validator.get_block(height.into()).await?.1);
    }
    pod.kill().await?;

    let mut trace = Vec::new();
    let started = tokio::time::Instant::now();
    while started.elapsed() < HALT_WINDOW {
        let tip = indexer.latest_block().await.map_err(|e| e.to_string());
        let at_fork = indexer.get_block(fork.into()).await.is_ok();
        trace.push((started.elapsed(), tip, at_fork));
        tokio::time::sleep(POLL).await;
    }
    let served = trace.iter().find(|(_, tip, at_fork)| tip.is_ok() || *at_fork);
    let winner =
        trace.iter().find(|(_, tip, _)| tip.as_ref().is_ok_and(|(_, h)| winners.contains(h)));
    assert_eq!(winner, None, "served the branch that rewrote durable height {fork}");
    assert_eq!(served, None, "restarted over a rewritten durable chain and served anyway");
    Ok(())
}
