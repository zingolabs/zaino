//! Back-to-back reorgs fired during catch-up, never awaited, converge on the validator's chain
//!
//! - Zaino may observe any subset of the intermediate tips (reorg mid-replay, mid-bulk-sync)
//! - Contract C17 in `docs/design/non-finalized-state-tests.md`

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(240);
/// Wide enough that no observable intermediate tip forks below the floor (`STORM` bound)
const DEPTH: u32 = 20;
const CHAIN: u32 = 3 * DEPTH;
/// (orphaned, replacement): tips within +3 of the start, lowest fork parent 4 below it (≪ `DEPTH`)
const STORM: [(u32, u32); 8] = [(3, 4), (2, 3), (4, 2), (1, 2), (5, 6), (2, 3), (3, 1), (1, 3)];

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn reorg_storm_during_catch_up_converges_on_the_validators_chain() -> Result<()> {
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
    let restarts = indexer.pod().await?.sample().await?.restarts;
    let mut lowest_fork_parent = u32::MAX;
    let mut tip = validator.tip().await?;
    for (orphaned, replacement) in STORM {
        let reorg = validator.reorg(orphaned, replacement, Some(FILLER_ADDRESS)).await?;
        lowest_fork_parent = lowest_fork_parent.min(u32::from(reorg.fork_parent.0));
        tip = reorg.tip;
    }
    indexer.wait_for_tip(tip, CONVERGE).await?;
    let tip = u32::from(tip.0);

    let vrpc = validator.json_rpc().await?;
    let served = indexer.get_block_range(1u32.into(), tip.into()).await?;
    assert_eq!(served.len(), tip as usize, "one block per height in [1, {tip}]");
    for (block, height) in served.iter().zip(1u32..) {
        let truth = vrpc.call_value("getblockhash", json!([height])).await?;
        let hash = BlockHash::from_wire(&block.hash).context("served hash is 32 bytes")?;
        assert_eq!(Some(hash.to_string().as_str()), truth.as_str(), "block at {height}");
    }
    for pair in served.windows(2) {
        assert_eq!(pair[1].prev_hash, pair[0].hash, "{} unlinked", pair[1].height);
    }

    let empty = |tree: &str| if tree.is_empty() { "000000".to_owned() } else { tree.to_owned() };
    for height in lowest_fork_parent..=tip {
        let zaino = indexer.get_tree_state(height.into()).await?;
        let zebra = vrpc.call_value("z_gettreestate", json!([height.to_string()])).await?;
        let final_state = |pool: &str| {
            empty(zebra[pool]["commitments"]["finalState"].as_str().unwrap_or_default())
        };
        let served = [
            zaino.hash.clone(),
            empty(&zaino.sapling_tree),
            empty(&zaino.orchard_tree),
            empty(&zaino.ironwood_tree),
        ];
        let truth = [
            zebra["hash"].as_str().unwrap_or_default().to_owned(),
            final_state("sapling"),
            final_state("orchard"),
            final_state("ironwood"),
        ];
        assert_eq!(served, truth, "[hash, sapling, orchard, ironwood] at {height}");
    }
    for index in ZainoIndex::ALL {
        assert_eq!(indexer.synced(index).await?, Some(true), "{index:?} serving after the storm");
    }
    let after = indexer.pod().await?.sample().await?;
    assert_eq!(after.restarts, restarts, "zainod rode out the storm without a restart");
    Ok(())
}
