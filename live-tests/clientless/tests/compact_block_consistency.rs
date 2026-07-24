//! Per-block consistency between served compact-block content and its chain metadata.
//!
//! A compact block's `chainMetadata` commitment-tree sizes are cumulative counts of the
//! note commitments the chain has produced. A scanning wallet advances its trees by the
//! actions/outputs each served block carries, so whenever a served block's tree-size
//! delta disagrees with its served commitment count the wallet observes a tree-size
//! discontinuity and treats it as a chain reorg. This walk pins that invariant per
//! block, per pool, for the request shape real (including pre-Ironwood) light clients
//! send: an empty `poolTypes` filter.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);

/// The mid-chain NU6.3 (Ironwood) activation height for the transition fixture:
/// an Orchard era `[2, 6)` that flips to Ironwood at height 6.
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

/// Orchard-only schedule: NU5..=NU6.2 active from height 2, NU6.3 never — the
/// orchard-receiver coinbase stays in Orchard actions for every block.
fn orchard_only() -> ActivationHeights {
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
        .build()
}

/// Mid-chain transition schedule: Orchard era `[2, boundary)`, Ironwood from
/// `boundary`, so the same orchard-receiver coinbase flips pools at `boundary`.
fn orchard_then_ironwood_at(boundary: u32) -> ActivationHeights {
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
        .set_nu6_3(Some(boundary))
        .build()
}

/// The validator's own `(sapling, orchard, ironwood)` commitment-tree sizes at `height`,
/// read from its verbose `getblock` `trees` field. This is the independent oracle:
/// zebrad's answer, computed without reference to what zaino serves, so the comparison is
/// not circular. zebra omits a pool's key when its tree is empty, so a missing key reads
/// as size 0 — which is also how `ironwood` reads on a pre-NU6.3 node.
async fn oracle_trees(validator: &impl ValidatorBackend, height: u64) -> Result<(u64, u64, u64)> {
    let block = validator
        .json_rpc()
        .await?
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
    Ok((size("sapling"), size("orchard"), size("ironwood")))
}

/// multi_thread required: the test manager spawns the validator and indexer services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn unfiltered_compact_blocks_match_chain_metadata_zebrad() -> Result<()> {
    // Shielded mining: from NU6.3 an orchard-receiver coinbase is built as Ironwood
    // actions (the coinbase's Orchard component must be empty from NU6.3), so every
    // generated block carries ironwood data for the walk to check. A transparent
    // miner would leave the ironwood assertions vacuous.
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(8).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    // The empty pool filter is what unfiltered (pre-Ironwood) clients send; the served
    // stream must include every shielded pool's actions.
    let start = BlockHeight::from(1u32);
    let blocks = indexer.get_block_range(start, tip).await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    let (mut prev_sapling, mut prev_orchard, mut prev_ironwood) =
        oracle_trees(&validator, 0).await?;
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

        // Clientless-exclusive predicate — oracle parity: zebrad's verbose getblock
        // reports the validator's own per-block tree sizes, an independent
        // implementation's answer to compare zaino's served metadata against. Package
        // tests cannot express this: their "source of truth" is the object being
        // served, so any such comparison is circular.
        let (oracle_sapling, oracle_orchard, oracle_ironwood) =
            oracle_trees(&validator, block.height).await?;
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
    // The counterpart of the guard above: the miner *asked* for Orchard, and from NU6.3
    // consensus requires an empty Orchard coinbase component, routing the reward into
    // Ironwood actions instead. With coinbase-only blocks, served orchard actions must
    // therefore be exactly zero. Together the two totals distinguish failure modes:
    // pool-swap (orchard > 0, ironwood == 0: ironwood served under the orchard field)
    // vs pool-drop (both zero) vs a broken routing premise.
    assert_eq!(
        total_orchard_actions, 0,
        "an Orchard-receiver coinbase must carry no Orchard actions from NU6.3"
    );

    Ok(())
}

/// Which shielded pool the fixture's coinbase reward must land in at a given height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoinbaseEra {
    /// Below NU5: the shielded-receiver reward cannot land in either pool.
    Neither,
    /// NU5 through NU6.2: the orchard-receiver reward is paid as Orchard actions.
    Orchard,
    /// From NU6.3: the same receiver's reward is routed to Ironwood actions.
    Ironwood,
}

/// Asserts each served compact block's coinbase routing satisfies exactly the era
/// predicate `expected_era` assigns its height. Blocks are coinbase-only, so a block's
/// per-pool action totals are the coinbase's: an Orchard-era coinbase pays the reward as
/// Orchard actions (`vtx[].actions`, no ironwood), an Ironwood-era coinbase as Ironwood
/// actions (`vtx[].ironwood_actions`, no orchard).
///
/// Mismatches are collected across the whole chain and reported together rather than
/// aborting at the first failing height, so one run distinguishes the hypotheses of
/// <https://github.com/zingolabs/zaino/issues/1368>: a boundary-only mismatch is the
/// zebrad builder off-by-one (A1), every-post-activation-height mismatches mean the
/// routing is absent (A2), and a clean pass here while the e2e wire tests fail moves
/// the blame to a zaino pool-swap (B).
fn assert_coinbase_routing(
    blocks: &[CompactBlock],
    expected_era: impl Fn(u64) -> CoinbaseEra,
) -> Result<()> {
    let mut violations: Vec<String> = Vec::new();
    for block in blocks {
        let orchard_actions: u64 = block.vtx.iter().map(|tx| tx.actions.len() as u64).sum();
        let ironwood_actions: u64 = block
            .vtx
            .iter()
            .map(|tx| tx.ironwood_actions.len() as u64)
            .sum();

        let expected = expected_era(block.height);
        let (want_orchard, want_ironwood) = match expected {
            CoinbaseEra::Neither => (false, false),
            CoinbaseEra::Orchard => (true, false),
            CoinbaseEra::Ironwood => (false, true),
        };
        let got_orchard = orchard_actions > 0 && ironwood_actions == 0;
        let got_ironwood = ironwood_actions > 0 && orchard_actions == 0;
        if got_orchard != want_orchard || got_ironwood != want_ironwood {
            violations.push(format!(
                "height {}: expected {expected:?}, wire says \
                 (orchard: {got_orchard}, ironwood: {got_ironwood}) — \
                 orchard actions {orchard_actions}, ironwood actions {ironwood_actions}, \
                 txs in block {}",
                block.height,
                block.vtx.len(),
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "coinbase routing mismatches ({} of {} served blocks; see \
         https://github.com/zingolabs/zaino/issues/1368 for the hypothesis map):\n{}",
        violations.len(),
        blocks.len(),
        violations.join("\n")
    );

    Ok(())
}

/// Orchard-only era: NU6.3 never activates, so every post-NU5 coinbase stays an
/// Orchard coinbase and no ironwood ever appears. (The zebrad *default* heights are
/// now the canonical NU6.3-at-2 set, so this fixture is explicit.)
///
/// multi_thread required: the test manager spawns the validator and indexer services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_only_coinbase_routing_zebrad() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(orchard_only());
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(6).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    assert_coinbase_routing(&blocks, |height| {
        if height >= 2 {
            CoinbaseEra::Orchard
        } else {
            CoinbaseEra::Neither
        }
    })
}

/// Ironwood-only era: NU6.3 active from height 2 (with every prior upgrade), so every
/// post-activation coinbase is an Ironwood coinbase and no Orchard coinbase ever
/// appears.
///
/// multi_thread required: the test manager spawns the validator and indexer services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only_coinbase_routing_zebrad() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(6).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    assert_coinbase_routing(&blocks, |height| {
        if height >= 2 {
            CoinbaseEra::Ironwood
        } else {
            CoinbaseEra::Neither
        }
    })
}

/// The transition: the same unchanged orchard-receiver miner produces Orchard
/// coinbases through NU6.2 and Ironwood coinbases from the NU6.3 activation height —
/// each predicate exactly delimiting its era, so a mis-timed flip fails on both sides
/// of the boundary.
///
/// multi_thread required: the test manager spawns the validator and indexer services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_coinbase_routing_flips_to_ironwood_at_activation_zebrad() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    // Two blocks past the boundary, so both eras carry more than one block.
    let tip = validator
        .generate_blocks(NU6_3_TRANSITION_BOUNDARY + 2)
        .await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    assert_coinbase_routing(&blocks, |height| {
        if height >= u64::from(NU6_3_TRANSITION_BOUNDARY) {
            CoinbaseEra::Ironwood
        } else if height >= 2 {
            CoinbaseEra::Orchard
        } else {
            CoinbaseEra::Neither
        }
    })
}
