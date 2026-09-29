//! Reorgs around the NU6.3 activation height serve the validator's blocks, trees and branch id
//!
//! - Orchard coinbase → Orchard tree before activation, Ironwood from it (zebra `Reset` edge)
//! - Contract C16 in `docs/design/non-finalized-state-tests.md`

use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::json;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
const DEPTH: u32 = 12;
const NU6_3: u32 = 24;
/// Old tip, `DEPTH − 2` past activation
const CHAIN: u32 = NU6_3 + DEPTH - 2;
/// `PoolType` wire enum: transparent, sapling, orchard, ironwood
const ALL_POOLS: [i32; 4] = [1, 2, 3, 4];

#[rstest]
#[case::replaces_the_last_pre_activation_block(NU6_3 - 2)]
#[case::replaces_the_activation_block(NU6_3 - 1)]
#[case::keeps_the_activation_block(NU6_3)]
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn reorg_across_nu6_3_activation_matches_the_validator(
    #[case] fork_parent: u32,
) -> Result<()> {
    let heights = ActivationHeights::builder()
        .set_overwinter(Some(1))
        .set_sapling(Some(1))
        .set_blossom(Some(1))
        .set_heartwood(Some(1))
        .set_canopy(Some(1))
        .set_nu5(Some(2))
        .set_nu6(Some(2))
        .set_nu6_1(Some(2))
        .set_nu6_2(Some(2))
        .set_nu6_3(Some(NU6_3))
        .build();
    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(heights);
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
    let old_trees = indexer.get_latest_tree_state().await?;

    let reorg = validator.reorg(CHAIN - fork_parent, DEPTH - 1, Some(FILLER_ADDRESS)).await?;
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;
    let tip = u32::from(reorg.tip.0);

    let vrpc = validator.json_rpc().await?;
    let served =
        indexer.get_block_range_with_pools(1u32.into(), reorg.tip.0, ALL_POOLS.to_vec()).await?;
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
    for height in fork_parent..=tip {
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
    let latest = indexer.get_latest_tree_state().await?;
    let parent = indexer.get_tree_state(fork_parent.into()).await?;
    assert_ne!(old_trees.ironwood_tree, latest.ironwood_tree, "A's Ironwood commitments orphaned");
    assert_eq!(
        (&latest.orchard_tree, &latest.ironwood_tree),
        (&parent.orchard_tree, &parent.ironwood_tree),
        "B appends no commitment: trees = fork parent's",
    );

    let chain = vrpc.call_value("getblockchaininfo", json!([])).await?;
    let branch = chain["consensus"]["chaintip"].as_str().unwrap_or_default();
    assert_eq!(indexer.indexer_info().await?.consensus_branch_id, branch, "branch id at {tip}");
    Ok(())
}
