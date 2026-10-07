//! Served compact-block content vs its `chainMetadata` tree sizes, per block, per pool
//!
//! - Tree-size delta != served commitments → scanning wallet sees a phantom reorg
//! - Empty `poolTypes` filter (what real, incl. pre-Ironwood, light clients send)

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);

/// Transition fixture's NU6.3 activation: `2..6` Orchard era, `6..` Ironwood era
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

/// Pool a coinbase reward lands in (one per era, each with its own tx version)
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum CoinbaseEra {
    Sapling,
    Orchard,
    Ironwood,
}

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn unfiltered_compact_blocks_match_chain_metadata_zebrad() -> Result<()> {
    // Orchard-receiver miner → Ironwood coinbase actions (transparent miner = vacuous walk)
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(8).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    // Oracle: zebrad's own per-block tree sizes (empty tree = key omitted = 0)
    let vrpc = validator.json_rpc().await?;
    let mut oracle: Vec<(u64, u64, u64)> = Vec::new();
    for height in 0..=u64::from(tip) {
        let block = vrpc.call_value("getblock", json!([height.to_string(), 1])).await?;
        let trees = block.get("trees").context("verbose getblock must carry a trees field")?;
        let size = |pool: &str| {
            trees.get(pool).and_then(|t| t.get("size")).and_then(Value::as_u64).unwrap_or(0)
        };
        oracle.push((size("sapling"), size("orchard"), size("ironwood")));
    }

    // Empty pool filter → every shielded pool's actions served
    let start = BlockHeight::from(1u32);
    let blocks = indexer.get_block_range(start, tip).await?;
    assert_eq!(blocks.len() as u64, u64::from(tip), "every height in [1, {tip}]");

    let (mut prev_sapling, mut prev_orchard, mut prev_ironwood) = oracle[0];
    let mut total_orchard_actions = 0u64;
    let mut total_ironwood_actions = 0u64;
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(block.height, index as u64 + 1, "contiguous (running totals)");
        let metadata = block
            .chain_metadata
            .as_ref()
            .context("every served compact block carries chain metadata")?;

        let sapling_outputs: u64 = block.vtx.iter().map(|tx| tx.outputs.len() as u64).sum();
        let orchard_actions: u64 = block.vtx.iter().map(|tx| tx.actions.len() as u64).sum();
        let ironwood_actions: u64 =
            block.vtx.iter().map(|tx| tx.ironwood_actions.len() as u64).sum();
        total_orchard_actions += orchard_actions;
        total_ironwood_actions += ironwood_actions;

        let sapling_size = u64::from(metadata.sapling_commitment_tree_size);
        let orchard_size = u64::from(metadata.orchard_commitment_tree_size);
        let ironwood_size = u64::from(metadata.ironwood_commitment_tree_size);

        let sizes = (sapling_size, orchard_size, ironwood_size);
        let height = block.height;
        // Metadata counting omitted actions (e.g. ironwood stripped) = phantom reorg
        let served = (
            prev_sapling + sapling_outputs,
            prev_orchard + orchard_actions,
            prev_ironwood + ironwood_actions,
        );
        assert_eq!(sizes, served, "tree-size delta = served commitments at {height}");

        // Oracle parity (live-only: package tests' truth = the served object itself)
        assert_eq!(sizes, oracle[height as usize], "tree sizes = validator's at {height}");

        prev_sapling = sapling_size;
        prev_orchard = orchard_size;
        prev_ironwood = ironwood_size;
    }

    assert!(total_ironwood_actions > 0, "fixture produced no ironwood actions");
    // Pool-swap (orchard > 0, ironwood = 0) vs pool-drop (both 0) (NU6.3: Orchard coinbase empty)
    assert_eq!(total_orchard_actions, 0, "Orchard coinbase: no Orchard actions from NU6.3");

    Ok(())
}

/// NU6.3 never active → every post-NU5 coinbase Orchard, no ironwood anywhere
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_only_coinbase_routing_zebrad() -> Result<()> {
    // Explicit schedule (zebrad default = NU6.3 at 2)
    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(
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
            .build(),
    );
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(6).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer.get_block_range(BlockHeight::from(1u32), tip).await?;
    assert_eq!(blocks.len() as u64, u64::from(tip), "every height in [1, {tip}]");

    // - Raw-block predicate = zebrad's routing; raw-vs-served = zaino's (both collected per run)
    // - <https://github.com/zingolabs/zaino/issues/1368>
    let vrpc = validator.json_rpc().await?;
    let mut violations: Vec<String> = Vec::new();
    for (index, served) in blocks.iter().enumerate() {
        let height = index as u64 + 1;
        assert_eq!(served.height, height, "served blocks must be contiguous");

        let block = vrpc.call_value("getblock", json!([height.to_string(), 2])).await?;
        let coinbase = &block["tx"][0];
        let count = |v: &Value| v.as_array().map_or(0, Vec::len);

        let is_coinbase =
            count(&coinbase["vin"]) == 1 && coinbase["vin"][0].get("coinbase").is_some();
        let version = coinbase["version"]
            .as_u64()
            .context("every block carries a versioned coinbase transaction")?;
        let sapling = count(&coinbase["vShieldedOutput"]);
        let orchard = count(&coinbase["orchard"]["actions"]);
        let ironwood = count(&coinbase["ironwood"]["actions"]);

        // Reward in exactly one pool, else `None` (fails)
        let observed = match (version, sapling, orchard, ironwood) {
            (4, 1.., 0, 0) => Some(CoinbaseEra::Sapling),
            (5, 0, 1.., 0) => Some(CoinbaseEra::Orchard),
            (6, 0, 0, 1..) => Some(CoinbaseEra::Ironwood),
            _ => None,
        };

        let expected = match height {
            h if h >= 2 => CoinbaseEra::Orchard,
            _ => CoinbaseEra::Sapling,
        };

        let served_orchard: usize = served.vtx.iter().map(|tx| tx.actions.len()).sum();
        let served_ironwood: usize = served.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        let wire_ok = served_orchard == orchard && served_ironwood == ironwood;

        if !is_coinbase || observed != Some(expected) || !wire_ok {
            violations.push(format!(
                "height {height}: want {expected:?}, raw is {observed:?} \
                 (is_coinbase {is_coinbase}, v{version}, sapling {sapling}, \
                 orchard {orchard}, ironwood {ironwood}); \
                 zaino served orchard {served_orchard}, ironwood {served_ironwood}"
            ));
        }
    }
    let (bad, of) = (violations.len(), blocks.len());
    assert!(violations.is_empty(), "coinbase routing ({bad}/{of}):\n{}", violations.join("\n"));

    Ok(())
}

/// NU6.3 active from 2 → every post-activation coinbase Ironwood, never Orchard
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only_coinbase_routing_zebrad() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(6).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer.get_block_range(BlockHeight::from(1u32), tip).await?;
    assert_eq!(blocks.len() as u64, u64::from(tip), "every height in [1, {tip}]");

    // - Raw-block predicate = zebrad's routing; raw-vs-served = zaino's (both collected per run)
    // - <https://github.com/zingolabs/zaino/issues/1368>
    let vrpc = validator.json_rpc().await?;
    let mut violations: Vec<String> = Vec::new();
    for (index, served) in blocks.iter().enumerate() {
        let height = index as u64 + 1;
        assert_eq!(served.height, height, "served blocks must be contiguous");

        let block = vrpc.call_value("getblock", json!([height.to_string(), 2])).await?;
        let coinbase = &block["tx"][0];
        let count = |v: &Value| v.as_array().map_or(0, Vec::len);

        let is_coinbase =
            count(&coinbase["vin"]) == 1 && coinbase["vin"][0].get("coinbase").is_some();
        let version = coinbase["version"]
            .as_u64()
            .context("every block carries a versioned coinbase transaction")?;
        let sapling = count(&coinbase["vShieldedOutput"]);
        let orchard = count(&coinbase["orchard"]["actions"]);
        let ironwood = count(&coinbase["ironwood"]["actions"]);

        // Reward in exactly one pool, else `None` (fails)
        let observed = match (version, sapling, orchard, ironwood) {
            (4, 1.., 0, 0) => Some(CoinbaseEra::Sapling),
            (5, 0, 1.., 0) => Some(CoinbaseEra::Orchard),
            (6, 0, 0, 1..) => Some(CoinbaseEra::Ironwood),
            _ => None,
        };

        let expected = match height {
            h if h >= 2 => CoinbaseEra::Ironwood,
            _ => CoinbaseEra::Sapling,
        };

        let served_orchard: usize = served.vtx.iter().map(|tx| tx.actions.len()).sum();
        let served_ironwood: usize = served.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        let wire_ok = served_orchard == orchard && served_ironwood == ironwood;

        if !is_coinbase || observed != Some(expected) || !wire_ok {
            violations.push(format!(
                "height {height}: want {expected:?}, raw is {observed:?} \
                 (is_coinbase {is_coinbase}, v{version}, sapling {sapling}, \
                 orchard {orchard}, ironwood {ironwood}); \
                 zaino served orchard {served_orchard}, ironwood {served_ironwood}"
            ));
        }
    }
    let (bad, of) = (violations.len(), blocks.len());
    assert!(violations.is_empty(), "coinbase routing ({bad}/{of}):\n{}", violations.join("\n"));

    Ok(())
}

/// One orchard-receiver miner: Orchard coinbases below NU6.3, Ironwood from it (a mis-timed
/// flip fails on both sides)
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_coinbase_routing_flips_to_ironwood_at_activation_zebrad() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(
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
            .set_nu6_3(Some(NU6_3_TRANSITION_BOUNDARY))
            .build(),
    );
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    // Two past the boundary (each era > 1 block)
    let tip = validator.generate_blocks(NU6_3_TRANSITION_BOUNDARY + 2).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer.get_block_range(BlockHeight::from(1u32), tip).await?;
    assert_eq!(blocks.len() as u64, u64::from(tip), "every height in [1, {tip}]");

    // - Raw-block predicate = zebrad's routing; raw-vs-served = zaino's (both collected per run)
    // - <https://github.com/zingolabs/zaino/issues/1368>
    let vrpc = validator.json_rpc().await?;
    let mut violations: Vec<String> = Vec::new();
    for (index, served) in blocks.iter().enumerate() {
        let height = index as u64 + 1;
        assert_eq!(served.height, height, "served blocks must be contiguous");

        let block = vrpc.call_value("getblock", json!([height.to_string(), 2])).await?;
        let coinbase = &block["tx"][0];
        let count = |v: &Value| v.as_array().map_or(0, Vec::len);

        let is_coinbase =
            count(&coinbase["vin"]) == 1 && coinbase["vin"][0].get("coinbase").is_some();
        let version = coinbase["version"]
            .as_u64()
            .context("every block carries a versioned coinbase transaction")?;
        let sapling = count(&coinbase["vShieldedOutput"]);
        let orchard = count(&coinbase["orchard"]["actions"]);
        let ironwood = count(&coinbase["ironwood"]["actions"]);

        // Reward in exactly one pool, else `None` (fails)
        let observed = match (version, sapling, orchard, ironwood) {
            (4, 1.., 0, 0) => Some(CoinbaseEra::Sapling),
            (5, 0, 1.., 0) => Some(CoinbaseEra::Orchard),
            (6, 0, 0, 1..) => Some(CoinbaseEra::Ironwood),
            _ => None,
        };

        let expected = match height {
            h if h >= u64::from(NU6_3_TRANSITION_BOUNDARY) => CoinbaseEra::Ironwood,
            h if h >= 2 => CoinbaseEra::Orchard,
            _ => CoinbaseEra::Sapling,
        };

        let served_orchard: usize = served.vtx.iter().map(|tx| tx.actions.len()).sum();
        let served_ironwood: usize = served.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        let wire_ok = served_orchard == orchard && served_ironwood == ironwood;

        if !is_coinbase || observed != Some(expected) || !wire_ok {
            violations.push(format!(
                "height {height}: want {expected:?}, raw is {observed:?} \
                 (is_coinbase {is_coinbase}, v{version}, sapling {sapling}, \
                 orchard {orchard}, ironwood {ironwood}); \
                 zaino served orchard {served_orchard}, ironwood {served_ironwood}"
            ));
        }
    }
    let (bad, of) = (violations.len(), blocks.len());
    assert!(violations.is_empty(), "coinbase routing ({bad}/{of}):\n{}", violations.join("\n"));

    Ok(())
}
