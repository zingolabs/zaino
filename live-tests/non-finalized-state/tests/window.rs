//! The non-final window's edges: the deepest legal reorg, and one block deeper
//!
//! - `depth` orphans = fork parent on the window floor (legal); `depth + 1` = below it (fatal)
//! - Contracts C9, C10 in `docs/design/non-finalized-state-tests.md`

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
/// Observation span after a fatal reorg (covers kubelet's first 10 s crash backoff twice)
const HALT_WINDOW: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_secs(1);
const DEPTH: u32 = 10;
const CHAIN: u32 = 3 * DEPTH;
/// `PoolType` wire enum: transparent, sapling, orchard, ironwood
const ALL_POOLS: [i32; 4] = [1, 2, 3, 4];

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn deepest_legal_reorg_never_rewrites_durable_data() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer =
        env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest().finalised_depth(DEPTH));
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    let old_tip = validator.tip().await?;
    indexer.wait_for_tip(old_tip, CONVERGE).await?;
    let old_chain =
        indexer.get_block_range_with_pools(1u32.into(), old_tip.0, ALL_POOLS.to_vec()).await?;

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (indexer, stop) = (indexer.clone(), stop.clone());
        tokio::spawn(async move {
            let mut samples = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let mut row = Vec::new();
                for index in ZainoIndex::ALL {
                    row.push(indexer.finalized_height(index).await.ok().flatten());
                }
                samples.push(row);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            samples
        })
    };

    let reorg = validator.reorg(DEPTH, DEPTH + 1, Some(FILLER_ADDRESS)).await?;
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;
    stop.store(true, Ordering::Relaxed);
    let samples = sampler.await?;
    let fork_parent = u32::from(reorg.fork_parent.0);

    assert!(samples.len() > 1, "durable extents sampled through the reorg");
    for (i, index) in ZainoIndex::ALL.iter().enumerate() {
        let trace: Vec<u32> = samples.iter().filter_map(|row| row[i]).collect();
        let regressed = trace.windows(2).find(|pair| pair[1] < pair[0]);
        assert_eq!(regressed, None, "{index:?} durable extent monotone: {trace:?}");
        // A: final < tip + 1 − depth = fork parent; B's first block may land at fork once replayed
        let (first, max) = (trace.first().copied(), trace.iter().max().copied());
        assert!(
            first.is_some_and(|d| d <= fork_parent) && max.is_some_and(|d| d <= fork_parent + 1),
            "{index:?} durable first {first:?} / max {max:?} vs fork parent {fork_parent}",
        );
    }
    let durable = indexer.finalized_height(ZainoIndex::CompactBlock).await?.unwrap_or(0) as usize;
    let common = durable.min(fork_parent as usize);
    let served =
        indexer.get_block_range_with_pools(1u32.into(), reorg.tip.0, ALL_POOLS.to_vec()).await?;
    assert!(common > 0, "fixture leaves a durable band below the window");
    assert!(
        served[..common] == old_chain[..common],
        "durable blocks [1, {common}] byte-identical across the reorg",
    );
    let replaced: Vec<_> = served[fork_parent as usize..].iter().map(|b| b.hash.clone()).collect();
    let orphaned: Vec<_> =
        old_chain[fork_parent as usize..].iter().map(|b| b.hash.clone()).collect();
    assert_eq!(orphaned.len(), DEPTH as usize, "the whole window orphaned");
    assert!(replaced.iter().all(|h| !orphaned.contains(h)), "every window block replaced");
    Ok(())
}

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn reorg_below_the_window_halts_instead_of_serving() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile").regtest().finalised_depth(DEPTH).restartable(),
    );
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    let old_tip = validator.tip().await?;
    indexer.wait_for_tip(old_tip, CONVERGE).await?;
    let pod = indexer.pod().await?;
    let restarts_before = pod.sample().await?.restarts;

    let reorg = validator.reorg(DEPTH + 1, DEPTH + 2, Some(FILLER_ADDRESS)).await?;
    let fork = u32::from(reorg.fork_parent.0) + 1;
    let mut winners = HashSet::new();
    for height in fork..=u32::from(reorg.tip.0) {
        winners.insert(validator.get_block(height.into()).await?.1);
    }

    let mut trace = Vec::new();
    let started = tokio::time::Instant::now();
    while started.elapsed() < HALT_WINDOW {
        let tip = indexer.latest_block().await;
        let at_fork = indexer.get_block(fork.into()).await;
        trace.push((started.elapsed(), tip.map_err(|e| e.to_string()), at_fork.is_ok()));
        tokio::time::sleep(POLL).await;
    }
    let restarts = pod.sample().await.map(|s| s.restarts).ok();

    let served_winner =
        trace.iter().find(|(_, tip, _)| tip.as_ref().is_ok_and(|(_, h)| winners.contains(h)));
    assert_eq!(served_winner, None, "served the branch forking below the window floor");
    let first_refusal = trace.iter().position(|(_, tip, _)| tip.is_err());
    let refusal = first_refusal.unwrap_or(trace.len());
    let resumed = trace[refusal..].iter().find(|(_, tip, at_fork)| tip.is_ok() || *at_fork);
    assert!(
        first_refusal.is_some() && resumed.is_none(),
        "halted for good: first refusal {first_refusal:?}, served again {resumed:?} \
         (restarts {restarts_before} → {restarts:?})",
    );
    Ok(())
}
