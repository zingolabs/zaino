//! A → B → A → B …: each return to a branch serves it byte-identically
//!
//! - `reconsiderblock` revives A (more work → best again), `invalidateblock` hands back to B
//! - Contracts C7, C12 in `docs/design/non-finalized-state-tests.md`

use std::time::Duration;

use anyhow::{ensure, Result};
use ztest::backends::zainod::family;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
const SETTLE: Duration = Duration::from_secs(60);
const SCRAPE: Duration = Duration::from_secs(10);
const DEPTH: u32 = 10;
const CHAIN: u32 = 3 * DEPTH;
/// B = 2 blocks on the parent of A's top 4 → shorter, so reconsidered A outworks it
const ORPHANED: u32 = 4;
const REPLACEMENT: u32 = 2;
const ROUNDS: u64 = 3;
/// `PoolType` wire enum: transparent, sapling, orchard, ironwood
const ALL_POOLS: [i32; 4] = [1, 2, 3, 4];

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn flip_flopping_between_two_branches_serves_each_byte_identically() -> Result<()> {
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
    let a_tip = validator.tip().await?;
    indexer.wait_for_tip(a_tip, CONVERGE).await?;
    let fork_parent = u32::from(a_tip.0) - ORPHANED;
    let a_blocks =
        indexer.get_block_range_with_pools(1u32.into(), a_tip.0, ALL_POOLS.to_vec()).await?;
    let mut a_trees = Vec::new();
    for height in fork_parent..=u32::from(a_tip.0) {
        a_trees.push(indexer.get_tree_state(height.into()).await?);
    }

    let reorg = validator.reorg(ORPHANED, REPLACEMENT, Some(FILLER_ADDRESS)).await?;
    let (b_tip, a_root) = (reorg.tip, reorg.orphaned[0].1);
    indexer.wait_for_tip(b_tip, CONVERGE).await?;
    let b_blocks =
        indexer.get_block_range_with_pools(1u32.into(), b_tip.0, ALL_POOLS.to_vec()).await?;
    let mut b_trees = Vec::new();
    for height in fork_parent..=u32::from(b_tip.0) {
        b_trees.push(indexer.get_tree_state(height.into()).await?);
    }
    let b_hashes: Vec<BlockHash> = b_blocks[fork_parent as usize..]
        .iter()
        .filter_map(|block| BlockHash::from_wire(&block.hash))
        .collect();
    let a_hashes: Vec<BlockHash> = reorg.orphaned.iter().map(|(_, hash)| *hash).collect();
    assert_eq!(b_hashes.len(), REPLACEMENT as usize, "B's replacement hashes");
    assert_ne!(a_trees.last(), b_trees.last(), "branches differ in tree content");

    let mut reorgs = 1;
    for round in 1..=ROUNDS {
        for to_a in [true, false] {
            let (branch, want, blocks, trees, dead) = if to_a {
                validator.reconsider_block(&a_root).await?;
                ("A", a_tip, &a_blocks, &a_trees, &b_hashes)
            } else {
                validator.invalidate_block(&a_root).await?;
                ("B", b_tip, &b_blocks, &b_trees, &a_hashes)
            };
            let started = tokio::time::Instant::now();
            while validator.tip().await? != want {
                ensure!(
                    started.elapsed() < SETTLE,
                    "round {round}: zebrad never settled on {branch}"
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            indexer.wait_for_tip(want, CONVERGE).await?;
            reorgs += 1;

            let served =
                indexer.get_block_range_with_pools(1u32.into(), want.0, ALL_POOLS.to_vec()).await?;
            assert!(
                served == *blocks,
                "round {round}: {branch} blocks byte-identical to first visit"
            );
            let mut served_trees = Vec::new();
            for height in fork_parent..=u32::from(want.0) {
                served_trees.push(indexer.get_tree_state(height.into()).await?);
            }
            assert_eq!(&served_trees, trees, "round {round}: {branch} tree states");
            for hash in dead {
                let code = indexer.get_block_by_hash(*hash).await.err().and_then(|e| e.grpc_code());
                assert_eq!(code, Some(tonic::Code::NotFound), "round {round} on {branch}: {hash}");
            }
            let counted = indexer.read(SCRAPE).await?.counter_total(family::REORGS);
            assert_eq!(counted, Some(reorgs), "round {round} on {branch}: one rollback per switch");
        }
    }
    Ok(())
}
