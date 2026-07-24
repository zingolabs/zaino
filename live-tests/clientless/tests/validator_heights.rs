//! Regression tests for the single source of truth for activation heights
//! (zaino#1076, `zainod-heights-from-validator-spec.md`).
//!
//! The invariant: the validator's configured activation heights are
//! authoritative. zainod's config carries only a network kind — its regtest
//! placeholder is the canonical `ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS`
//! regardless of the fixture — and both backends adopt the real schedule
//! from `getblockchaininfo.upgrades` at spawn.
//!
//! Two halves, matching the spec's acceptance criteria:
//!
//! 1. [`zainod_syncs_a_schedule_its_config_never_saw`] — boundary sync and
//!    the no-recompile proof in one: the same zainod build and (kind-only)
//!    configuration that every canonical-heights test runs is here pointed
//!    at a validator on the NU6.3-at-6 transition schedule, and must sync
//!    across the boundary and serve era-correct compact blocks. Before
//!    adoption this exact misalignment killed the chain-index sync with
//!    `InvalidData("Block commitment could not be computed")`.
//! 2. [`getblockchaininfo_reports_the_configured_schedule`] — the input
//!    contract: what zebrad actually puts in the `upgrades` map for a
//!    configured schedule, pinned against a live node rather than assumed.
//!    The mapping from that shape to adopted heights is unit-tested next to
//!    `activation_heights_from_upgrades` in zaino-state.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);

const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

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

/// Boundary sync + no-recompile proof: a kind-only-configured zainod adopts
/// the NU6.3-at-6 schedule from the validator, syncs across the boundary,
/// and serves era-correct compact blocks for both eras.
///
/// multi_thread required: the test manager spawns the validator and indexer
/// services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn zainod_syncs_a_schedule_its_config_never_saw() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    // Two blocks past the boundary, so both eras carry more than one block.
    // Reaching the tip at all is the core regression: pre-adoption, the
    // chain-index sync died on the first block whose commitment scheme the
    // misconfigured heights got wrong.
    let tip = validator
        .generate_blocks(NU6_3_TRANSITION_BOUNDARY + 1)
        .await?;
    indexer.wait_for_block_num(tip, READY).await?;
    assert!(
        u64::from(tip) > u64::from(NU6_3_TRANSITION_BOUNDARY),
        "sync must cross the boundary, tip is {}",
        u64::from(tip)
    );

    // Era composition of the served chain proves the adopted schedule is the
    // validator's, not the placeholder: under the placeholder (NU6.3 at 2)
    // the pre-boundary orchard coinbases would be misread as ironwood-era.
    let blocks = indexer
        .get_block_range(BlockHeight::from(2u32), tip)
        .await?;
    assert!(!blocks.is_empty(), "no compact blocks served");
    for block in &blocks {
        let height = block.height;
        let has_orchard = block.vtx.iter().any(|tx| !tx.actions.is_empty());
        let has_ironwood = block.vtx.iter().any(|tx| !tx.ironwood_actions.is_empty());
        if height >= u64::from(NU6_3_TRANSITION_BOUNDARY) {
            assert!(
                has_ironwood && !has_orchard,
                "height {height} must be ironwood-era, got orchard={has_orchard} ironwood={has_ironwood}"
            );
        } else {
            assert!(
                has_orchard && !has_ironwood,
                "height {height} must be orchard-era, got orchard={has_orchard} ironwood={has_ironwood}"
            );
        }
    }

    Ok(())
}

/// The input contract for adoption: the `upgrades` map a live zebrad reports
/// for the transition schedule, pinned exactly — upgrade set and heights.
/// Establishes (from real output, not reasoning) that nothing pre-Overwinter
/// appears: the map is keyed by consensus branch ID, which pre-Overwinter eras
/// don't have.
///
/// multi_thread required: the test manager spawns the validator and indexer
/// services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn getblockchaininfo_reports_the_configured_schedule() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    env.build().await?;

    let blockchain_info = validator
        .json_rpc()
        .await?
        .call_value("getblockchaininfo", json!([]))
        .await?;
    let upgrades = blockchain_info
        .get("upgrades")
        .and_then(Value::as_object)
        .context("getblockchaininfo must carry an upgrades object")?;

    let mut reported: BTreeMap<String, u64> = BTreeMap::new();
    for entry in upgrades.values() {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .context("each upgrade entry must carry a name")?
            .to_string();
        let height = entry
            .get("activationheight")
            .and_then(Value::as_u64)
            .context("each upgrade entry must carry an activationheight")?;
        reported.insert(name, height);
    }

    let expected: BTreeMap<String, u64> = [
        ("Overwinter", 1),
        ("Sapling", 1),
        ("Blossom", 1),
        ("Heartwood", 1),
        ("Canopy", 1),
        ("NU5", 2),
        ("NU6", 2),
        ("NU6.1", 2),
        ("NU6.2", 2),
        ("NU6.3", u64::from(NU6_3_TRANSITION_BOUNDARY)),
    ]
    .into_iter()
    .map(|(name, height)| (name.to_string(), height))
    .collect();

    assert_eq!(
        reported, expected,
        "reported upgrade schedule must match the pinned transition heights"
    );

    Ok(())
}
