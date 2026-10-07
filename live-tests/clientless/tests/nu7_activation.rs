//! NU7 activation on regtest, clientless: the adopted schedule, the consensus
//! branch flip, and the served chain across the boundary, on both backends.
//!
//! NU7 (ZIP 259) adds no transaction format, so the served form on either side
//! of the boundary is the Ironwood era's. What changes is the consensus branch
//! (`0x77190ad9`) and the schedule zainod adopts from the validator, which is
//! what these tests pin: the hermetic replay of what The Public Testnet did at
//! height 4,465,026.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use zaino_testutils::{assert_rpc_parity, ZEBRAD_VERSION};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
/// NU6.3 (Ironwood) from height 2, NU7 from [`NU7_BOUNDARY`].
const NU7_BOUNDARY: u32 = 6;
const NU6_3_BRANCH: &str = "37a5165b";
const NU7_BRANCH: &str = "77190ad9";

fn nu7_schedule() -> ActivationHeights {
    ActivationHeights::builder()
        .set_overwinter(Some(1))
        .set_sapling(Some(1))
        .set_blossom(Some(1))
        .set_heartwood(Some(1))
        .set_canopy(Some(1))
        .set_nu5(Some(2))
        .set_nu6(Some(2))
        .set_nu6_1(Some(2))
        .set_nu6_2(Some(2))
        .set_nu6_3(Some(2))
        .set_nu7(Some(NU7_BOUNDARY))
        .build()
}

/// `(chaintip, nextblock)` from a `getblockchaininfo` reply.
fn consensus(info: &Value) -> Result<(String, String)> {
    let consensus = info
        .get("consensus")
        .context("getblockchaininfo must carry a consensus object")?;
    let branch = |key: &str| -> Result<String> {
        consensus
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("consensus.{key}"))
    };
    Ok((branch("chaintip")?, branch("nextblock")?))
}

/// The `(name, activationheight, status)` of the upgrade keyed by `branch`.
fn upgrade_entry(info: &Value, branch: &str) -> Result<(String, u64, String)> {
    let entry = info
        .get("upgrades")
        .and_then(|upgrades| upgrades.get(branch))
        .with_context(|| format!("getblockchaininfo.upgrades has no {branch} entry"))?;
    let text = |key: &str| -> Result<String> {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("upgrades.{branch}.{key}"))
    };
    let height = entry
        .get("activationheight")
        .and_then(Value::as_u64)
        .with_context(|| format!("upgrades.{branch}.activationheight"))?;
    Ok((text("name")?, height, text("status")?))
}

/// zainod adopts the NU7 schedule from the validator and both backends report
/// the consensus branch flipping from NU6.3 to NU7 exactly at the boundary,
/// over JSON-RPC (`getblockchaininfo.consensus`) and gRPC (`GetLightdInfo`).
#[ztest::qos::integration(footprint = "3c/6Gi")]
#[tokio::test(flavor = "multi_thread")]
async fn consensus_branch_flips_at_the_nu7_boundary_on_both_backends() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(nu7_schedule());
    let vol = env.shared_volume("zebra-db");
    let validator = env.add_validator(
        Validator::zebrad(ZEBRAD_VERSION)
            .regtest()
            .mine_to(Pool::Orchard)
            .mount(&vol),
    );
    let fetch = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::Fetch)
            .named("zaino-fetch"),
    );
    let state = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::State)
            .mount(&vol)
            .named("zaino-state"),
    );
    env.build().await?;
    let indexers = [&fetch, &state];

    // One block below the boundary: NU7 is scheduled and the next block activates it.
    let tip = validator.generate_blocks(NU7_BOUNDARY - 2).await?;
    assert_eq!(u32::from(tip), NU7_BOUNDARY - 1);
    for indexer in indexers {
        indexer.wait_for_block_num(tip, READY).await?;
        let info = indexer
            .json_rpc()
            .await?
            .call_value("getblockchaininfo", json!([]))
            .await?;
        assert_eq!(
            upgrade_entry(&info, NU7_BRANCH)?,
            (
                "NU7".to_string(),
                u64::from(NU7_BOUNDARY),
                "pending".to_string()
            )
        );
        assert_eq!(
            consensus(&info)?,
            (NU6_3_BRANCH.to_string(), NU7_BRANCH.to_string())
        );
        assert_eq!(
            indexer.indexer_info().await?.consensus_branch_id,
            NU6_3_BRANCH
        );
    }

    // The activation block, then two more: NU7 in force at the tip and beyond.
    for _ in 0..3 {
        let tip = validator.generate_blocks(1).await?;
        for indexer in indexers {
            indexer.wait_for_block_num(tip, READY).await?;
            let info = indexer
                .json_rpc()
                .await?
                .call_value("getblockchaininfo", json!([]))
                .await?;
            assert_eq!(
                upgrade_entry(&info, NU7_BRANCH)?,
                (
                    "NU7".to_string(),
                    u64::from(NU7_BOUNDARY),
                    "active".to_string()
                ),
                "at tip {tip}"
            );
            assert_eq!(
                consensus(&info)?,
                (NU7_BRANCH.to_string(), NU7_BRANCH.to_string()),
                "at tip {tip}"
            );
            assert_eq!(
                indexer.indexer_info().await?.consensus_branch_id,
                NU7_BRANCH,
                "at tip {tip}"
            );
        }
    }
    Ok(())
}

/// Across the boundary the served chain is one contiguous chain whose per-block
/// tree-size deltas match its served actions and whose final tree sizes match the
/// validator's; the boundary treestate agrees between both backends and the
/// validator; and `getstandardfee` passes the validator's answer through.
#[ztest::qos::integration(footprint = "3c/6Gi")]
#[tokio::test(flavor = "multi_thread")]
async fn served_chain_is_continuous_across_the_nu7_boundary() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(nu7_schedule());
    let vol = env.shared_volume("zebra-db");
    let validator = env.add_validator(
        Validator::zebrad(ZEBRAD_VERSION)
            .regtest()
            .mine_to(Pool::Orchard)
            .mount(&vol),
    );
    let fetch = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::Fetch)
            .named("zaino-fetch"),
    );
    let state = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::State)
            .mount(&vol)
            .named("zaino-state"),
    );
    env.build().await?;
    let indexers = [&fetch, &state];

    let tip = validator.generate_blocks(NU7_BOUNDARY + 1).await?;
    for indexer in indexers {
        indexer.wait_for_block_num(tip, READY).await?;
    }

    // The validator's own tree sizes at the tip, the oracle the walk must land on.
    let vrpc = validator.json_rpc().await?;
    let tip_block = vrpc
        .call_value("getblock", json!([u64::from(tip).to_string(), 1]))
        .await?;
    let trees = tip_block
        .get("trees")
        .context("verbose getblock must carry a trees field")?;
    let oracle = |pool: &str| {
        trees
            .get(pool)
            .and_then(|tree| tree.get("size"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let (oracle_sapling, oracle_orchard, oracle_ironwood) =
        (oracle("sapling"), oracle("orchard"), oracle("ironwood"));
    assert!(
        oracle_ironwood > 0,
        "shielded mining must grow the ironwood tree"
    );

    for indexer in indexers {
        let blocks = indexer
            .get_block_range(BlockHeight::from(1u32), tip)
            .await?;
        assert_eq!(blocks.len() as u64, u64::from(tip));
        let (mut sapling, mut orchard, mut ironwood) = (0u64, 0u64, 0u64);
        for (index, block) in blocks.iter().enumerate() {
            assert_eq!(
                block.height,
                index as u64 + 1,
                "served blocks must be contiguous"
            );
            let metadata = block
                .chain_metadata
                .as_ref()
                .context("every served compact block carries chain metadata")?;
            sapling += block
                .vtx
                .iter()
                .map(|tx| tx.outputs.len() as u64)
                .sum::<u64>();
            orchard += block
                .vtx
                .iter()
                .map(|tx| tx.actions.len() as u64)
                .sum::<u64>();
            ironwood += block
                .vtx
                .iter()
                .map(|tx| tx.ironwood_actions.len() as u64)
                .sum::<u64>();
            assert_eq!(
                (
                    u64::from(metadata.sapling_commitment_tree_size),
                    u64::from(metadata.orchard_commitment_tree_size),
                    u64::from(metadata.ironwood_commitment_tree_size),
                ),
                (sapling, orchard, ironwood),
                "tree sizes must equal the served commitments through height {}",
                block.height
            );
        }
        assert_eq!(
            (sapling, orchard, ironwood),
            (oracle_sapling, oracle_orchard, oracle_ironwood),
            "the walk must land on the validator's tree sizes"
        );
    }

    // The boundary treestate: both backends against the validator.
    let expected = vrpc
        .call_value("z_gettreestate", json!([NU7_BOUNDARY.to_string()]))
        .await?;
    let final_state = |pool: &str| -> Result<String> {
        expected
            .get(pool)
            .and_then(|pool| pool.get("commitments"))
            .and_then(|commitments| commitments.get("finalState"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("z_gettreestate.{pool}.commitments.finalState"))
    };
    for indexer in indexers {
        let treestate = indexer
            .get_tree_state(BlockHeight::from(NU7_BOUNDARY))
            .await?;
        assert_eq!(treestate.height, u64::from(NU7_BOUNDARY));
        assert_eq!(treestate.sapling_tree, final_state("sapling")?);
        assert_eq!(treestate.orchard_tree, final_state("orchard")?);
    }

    // A test network reports ZIP 317's marginal fee at every height.
    for indexer in indexers {
        let irpc = indexer.json_rpc().await?;
        assert_rpc_parity("getstandardfee", "", &vrpc, &irpc, &[]).await?;
        let fee = irpc.call_value("getstandardfee", json!([])).await?;
        assert_eq!(fee, json!({ "standard_fee": 1000, "version": 0 }));
    }
    Ok(())
}
