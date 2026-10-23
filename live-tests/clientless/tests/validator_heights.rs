//! Activation heights = the validator's `getblockchaininfo.upgrades` (zaino#1076)
//!
//! - zainod config = network kind only (regtest placeholder = `ActivationHeights::regtest_default`)
//! - Upgrades map → schedule: unit-tested at `parse_blockchain_info` (zaino-source `json.rs`)

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);

const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

/// Input contract: zebrad's `upgrades` map for the transition schedule, pinned (set, order,
/// heights; nothing pre-Overwinter: keyed by branch id)
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn getblockchaininfo_reports_the_configured_schedule() -> Result<()> {
    let nu6_3_at_boundary = ActivationHeights::builder()
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
        .build();
    let placeholder = ActivationHeights::regtest_default();
    assert_ne!(nu6_3_at_boundary, placeholder, "premise: schedule != zainod's config placeholder");

    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(nu6_3_at_boundary);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    env.build().await?;

    let blockchain_info =
        validator.json_rpc().await?.call_value("getblockchaininfo", json!([])).await?;
    let upgrades = blockchain_info
        .get("upgrades")
        .and_then(Value::as_object)
        .context("getblockchaininfo must carry an upgrades object")?;

    // Activation order kept (serde_json `preserve_order`) → ordered `Vec` compare pins it too
    let mut reported: Vec<(String, u64)> = Vec::new();
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
        reported.push((name, height));
    }

    let expected: Vec<(String, u64)> = [
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

    assert_eq!(reported, expected, "upgrades (name, height): zebrad's vs configured, in order");

    Ok(())
}
