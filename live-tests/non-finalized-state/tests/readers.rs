//! Readers polling straight through a reorg see one branch per answer, never a regression
//!
//! - Every read of the whole trace judged after the fact (a mid-replay race = one bad sample)
//! - Contract C8 in `docs/design/non-finalized-state-tests.md`

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
/// Reads kept going after convergence (a late regression = still a regression)
const AFTERMATH: Duration = Duration::from_secs(10);
const DEPTH: u32 = 10;
const CHAIN: u32 = 3 * DEPTH;
const ORPHANED: u32 = 6;
const REPLACEMENT: u32 = 8;

struct Read {
    tip: Result<BlockTip, Option<tonic::Code>>,
    range: Option<Result<Vec<CompactBlock>, Option<tonic::Code>>>,
    lightd_height: Result<u64, Option<tonic::Code>>,
}

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn readers_see_one_branch_at_a_time_through_a_reorg() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer =
        env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest().finalised_depth(DEPTH));
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    let old_tip = validator.tip().await?;
    indexer.wait_for_tip(old_tip, CONVERGE).await?;

    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (indexer, stop) = (indexer.clone(), stop.clone());
        tokio::spawn(async move {
            let mut reads = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let tip = indexer.latest_block().await.map_err(|e| e.grpc_code());
                let range = match tip {
                    Ok((height, _)) => Some(
                        indexer
                            .get_block_range(1u32.into(), height)
                            .await
                            .map_err(|e| e.grpc_code()),
                    ),
                    Err(_) => None,
                };
                let lightd_height =
                    indexer.indexer_info().await.map(|i| i.block_height).map_err(|e| e.grpc_code());
                reads.push(Read { tip, range, lightd_height });
            }
            reads
        })
    };

    let reorg = validator.reorg(ORPHANED, REPLACEMENT, Some(FILLER_ADDRESS)).await?;
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;
    tokio::time::sleep(AFTERMATH).await;
    stop.store(true, Ordering::Relaxed);
    let reads = reader.await?;

    let fork_parent = u32::from(reorg.fork_parent.0);
    let fork = u64::from(fork_parent) + 1;
    let a: HashSet<BlockHash> = reorg.orphaned.iter().map(|(_, hash)| *hash).collect();
    let mut b = HashSet::new();
    for height in fork_parent + 1..=u32::from(reorg.tip.0) {
        b.insert(validator.get_block(height.into()).await?.1);
    }
    let branch_of = |hash: &BlockHash| match (a.contains(hash), b.contains(hash)) {
        (true, _) => "A",
        (_, true) => "B",
        _ => "neither",
    };

    let mut violations = Vec::new();
    let mut last_branch = "A";
    let mut seen_b = false;
    for (i, read) in reads.iter().enumerate() {
        for code in
            [read.tip.as_ref().err(), read.lightd_height.as_ref().err()].into_iter().flatten()
        {
            if *code != Some(tonic::Code::Unavailable) {
                violations.push(format!("read {i}: refused with {code:?}, not UNAVAILABLE"));
            }
        }
        if let Ok((height, hash)) = &read.tip {
            if u32::from(*height) < fork_parent {
                violations.push(format!("read {i}: tip {} below fork parent", u32::from(*height)));
            }
            if u64::from(u32::from(*height)) >= fork {
                let branch = branch_of(hash);
                seen_b |= branch == "B";
                if branch == "neither" || (last_branch == "B" && branch == "A") {
                    violations
                        .push(format!("read {i}: tip {hash} on {branch} after {last_branch}"));
                }
                last_branch = branch;
            }
        }
        if let Ok(height) = read.lightd_height {
            if height < u64::from(fork_parent) {
                violations
                    .push(format!("read {i}: GetLightdInfo height {height} below fork parent"));
            }
        }
        match &read.range {
            Some(Err(code)) if *code != Some(tonic::Code::Unavailable) => {
                violations.push(format!("read {i}: GetBlockRange refused with {code:?}"));
            }
            Some(Ok(blocks)) => {
                let contiguous = blocks.iter().zip(1u64..).all(|(block, h)| block.height == h);
                let linked = blocks.windows(2).all(|pair| pair[1].prev_hash == pair[0].hash);
                let branches: HashSet<&str> = blocks
                    .iter()
                    .filter(|block| block.height >= fork)
                    .map(|block| {
                        BlockHash::from_wire(&block.hash).map_or("neither", |h| branch_of(&h))
                    })
                    .collect();
                let resurrected = seen_b && branches.contains("A");
                if !contiguous
                    || !linked
                    || resurrected
                    || branches.len() > 1
                    || branches.contains("neither")
                {
                    violations.push(format!(
                        "read {i}: range of {} contiguous={contiguous} linked={linked} \
                         resurrected={resurrected} branches={branches:?}",
                        blocks.len(),
                    ));
                }
            }
            _ => {}
        }
    }

    assert!(
        violations.is_empty(),
        "{} of {} reads violated:\n{}",
        violations.len(),
        reads.len(),
        violations.join("\n")
    );
    assert!(seen_b, "{} reads, none on branch B (reader never saw the reorg land)", reads.len());
    assert_eq!(last_branch, "B", "trace ends on the winning branch");
    Ok(())
}
