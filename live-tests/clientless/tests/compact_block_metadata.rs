//! Per-block consistency between served compact-block content and its chain metadata.
//!
//! A compact block's `chainMetadata` commitment-tree sizes are cumulative counts of the
//! note commitments the chain has produced. A scanning wallet advances its trees by the
//! actions/outputs each served block carries, so whenever a served block's tree-size
//! delta disagrees with its served commitment count the wallet observes a tree-size
//! discontinuity and treats it as a chain reorg. This walk pins that invariant per
//! block, per pool, for the request shape real (including pre-Ironwood) light clients
//! send: an empty `poolTypes` filter.
//!
//! Boots the greenfield runtime through the `ztest-fixture` Direct bridge; kept in its
//! own file because ztest resolves one dev image per test binary, so a fixture SUT must
//! not share a file with default-feature SUTs.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn unfiltered_compact_blocks_match_chain_metadata_zebrad() -> Result<()> {
    // Shielded mining: from NU6.3 an orchard-receiver coinbase is built as Ironwood
    // actions (the coinbase's Orchard component must be empty from NU6.3), so every
    // generated block carries ironwood data for the walk to check. A transparent
    // miner would leave the ironwood assertions vacuous.
    let mut env = TestEnv::builder().ready_timeout(READY);
    let zebra_vol = env.shared_volume("zebra-state");
    let validator = env.add_validator(
        Validator::zebrad("6.2.3")
            .regtest()
            .mine_to(Pool::Orchard)
            .mount(&zebra_vol),
    );
    let indexer = env.add_indexer(
        dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = ["ztest-fixture"]
        )
        .regtest()
        .tuning(ZainoTuning::State)
        .mount(&zebra_vol)
        .env("ZAINO_TEST_REGTEST_DIRECT_FIXTURE", "1")
        .env("ZAINO_TEST_ZEBRA_CACHE_DIR", zebra_vol.mount_path())
        .env(
            "ZAINO_TEST_ZEBRA_JSONRPC",
            format!("zebrad:{}", ztest::ports::ZEBRAD_RPC),
        ),
    );
    env.build().await?;

    let tip = validator.generate_blocks(8).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    // The independent oracle: zebrad's own per-block `(sapling, orchard, ironwood)` tree
    // sizes, computed without reference to what zaino serves. A pool's key is omitted
    // when its tree is empty, so a missing key reads as size 0.
    let vrpc = validator.json_rpc().await?;
    let mut oracle: Vec<(u64, u64, u64)> = Vec::new();
    for height in 0..=u64::from(tip) {
        let block = vrpc
            .call_value("getblock", json!([height.to_string(), 1]))
            .await?;
        let trees = block
            .get("trees")
            .context("verbose getblock must carry a trees field")?;
        let size = |pool: &str| {
            trees
                .get(pool)
                .and_then(|t| t.get("size"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        oracle.push((size("sapling"), size("orchard"), size("ironwood")));
    }

    // The empty pool filter is what unfiltered (pre-Ironwood) clients send; the served
    // stream must include every shielded pool's actions.
    let start = BlockHeight::from(1u32);
    let blocks = indexer.get_block_range(start, tip).await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "the served range must cover every height in [1, {tip}]"
    );

    let (mut prev_sapling, mut prev_orchard, mut prev_ironwood) = oracle[0];
    let mut total_orchard_actions = 0u64;
    let mut total_ironwood_actions = 0u64;
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(
            block.height,
            index as u64 + 1,
            "served blocks must be contiguous for the walk's running totals"
        );
        let metadata = block
            .chain_metadata
            .as_ref()
            .context("every served compact block carries chain metadata")?;

        let sapling_outputs: u64 = block.vtx.iter().map(|tx| tx.outputs.len() as u64).sum();
        let orchard_actions: u64 = block.vtx.iter().map(|tx| tx.actions.len() as u64).sum();
        let ironwood_actions: u64 = block
            .vtx
            .iter()
            .map(|tx| tx.ironwood_actions.len() as u64)
            .sum();
        total_orchard_actions += orchard_actions;
        total_ironwood_actions += ironwood_actions;

        let sapling_size = u64::from(metadata.sapling_commitment_tree_size);
        let orchard_size = u64::from(metadata.orchard_commitment_tree_size);
        let ironwood_size = u64::from(metadata.ironwood_commitment_tree_size);

        assert_eq!(
            sapling_size,
            prev_sapling + sapling_outputs,
            "sapling tree-size delta must equal the served output count at height {}",
            block.height
        );
        assert_eq!(
            orchard_size,
            prev_orchard + orchard_actions,
            "orchard tree-size delta must equal the served action count at height {}",
            block.height
        );
        // The regression this walk exists for: a served block whose metadata counts
        // commitments from actions the block omits (e.g. ironwood stripped from an
        // unfiltered request) reads to a scanning wallet as a phantom chain reorg.
        assert_eq!(
            ironwood_size,
            prev_ironwood + ironwood_actions,
            "ironwood tree-size delta must equal the served action count at height {}",
            block.height
        );

        let (oracle_sapling, oracle_orchard, oracle_ironwood) = oracle[block.height as usize];
        assert_eq!(
            sapling_size, oracle_sapling,
            "served sapling tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            orchard_size, oracle_orchard,
            "served orchard tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            ironwood_size, oracle_ironwood,
            "served ironwood tree size must match the validator's own at height {}",
            block.height
        );

        prev_sapling = sapling_size;
        prev_orchard = orchard_size;
        prev_ironwood = ironwood_size;
    }

    assert!(
        total_ironwood_actions > 0,
        "the fixture produced no ironwood actions; the walk asserted nothing about ironwood"
    );
    // An Orchard-receiver coinbase carries no Orchard actions from NU6.3 (consensus
    // requires an empty Orchard component; the reward routes into Ironwood), so a
    // coinbase-only chain must serve zero orchard actions.
    assert_eq!(
        total_orchard_actions, 0,
        "an Orchard-receiver coinbase must carry no Orchard actions from NU6.3"
    );

    Ok(())
}
