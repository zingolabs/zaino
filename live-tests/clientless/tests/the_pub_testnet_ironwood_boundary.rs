//! Observational walk of the real NU6.3 activation boundary on The Public
//! Testnet (glossary: The Public Testnet is the public test network and
//! nothing else). Historical blocks are permanent, so these invariants are
//! checkable forever, long after the epoch that produced them closed.
//!
//! The invariants, enforced one block below the boundary and at it:
//!
//! - Below the activation height the Ironwood pool holds no value.
//! - From the activation height the Orchard pool's value never increases:
//!   the no-new-value rule admits only withdrawals and same-receiver change
//!   (the cross-address restriction,
//!   <https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>).
//!
//! The activation height itself is read from the running validator's
//! `getblockchaininfo.upgrades` — the single source of truth for activation
//! heights — and cross-checked against the observed activation on The
//! Public Testnet at height 4,134,000 (~2026-07-04).
//!
//! Non-hermetic: the walk needs a validator whose chain is a pre-synced
//! snapshot of The Public Testnet reaching past the NU6.3 activation height.
//! Under ztest that snapshot is supplied by a named testnet fixture
//! (`Validator::zebrad(..).testnet(<variant>)`, paired with a zainod indexer
//! consuming the same `zebra.tar.xz` archive). No such boundary-crossing
//! variant exists yet, so this test fails at materialization until one lands
//! (see the `TODO` on the test body and ztest `fixtures/testnet/README.md`).

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(300);

/// The height The Public Testnet activated NU6.3 at, recorded from the real
/// activation. A mismatch means either a reset of The Public Testnet or a
/// wrong validator pin — both worth failing loudly over.
const OBSERVED_NU6_3_ACTIVATION_ON_THE_PUB_TESTNET: u32 = 4_134_000;

/// The chain value of `pool_id` as of `height`, from the validator's
/// verbosity-2 block object.
async fn pool_zats(rpc: &JsonRpcClient, height: u32, pool_id: &str) -> Result<i64> {
    let block = rpc
        .call_value("getblock", json!([height.to_string(), 2]))
        .await?;
    let pools = block
        .get("valuePools")
        .and_then(Value::as_array)
        .context("verbosity-2 block object must carry valuePools")?;
    let pool = pools
        .iter()
        .find(|pool| pool.get("id").and_then(Value::as_str) == Some(pool_id))
        .with_context(|| format!("value pools must include {pool_id}"))?;
    pool.get("chainValueZat")
        .and_then(Value::as_i64)
        .with_context(|| format!("{pool_id}.chainValueZat must be an integer"))
}

/// multi_thread required: the test launches the validator pod and polls it
/// over RPC.
#[ztest::qos::testnet]
#[tokio::test(flavor = "multi_thread")]
async fn value_pools_respect_the_boundary_on_the_pub_testnet() -> Result<()> {
    // TODO: needs a ztest testnet fixture variant whose synced chain crosses the
    // NU6.3/ironwood boundary (height 4,134,000 on The Public Testnet). The only
    // named variants today ("orchard", synced past NU5; "sapling") stop far below
    // it, and their archives are not in tree yet, so `env.build()` fails at
    // materialization until such a fixture lands. See ztest
    // fixtures/testnet/README.md.
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.0").testnet("orchard"));
    let _indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").testnet("orchard"));
    env.build().await?;

    let vrpc = validator.json_rpc().await?;
    let blockchain_info = vrpc.call_value("getblockchaininfo", json!([])).await?;

    // The validator's reported schedule is the source of truth for the
    // boundary; the recorded constant pins the real history of The Public Testnet.
    let boundary = blockchain_info
        .get("upgrades")
        .and_then(Value::as_object)
        .context("getblockchaininfo.upgrades")?
        .values()
        .find_map(|upgrade| {
            (upgrade.get("name").and_then(Value::as_str) == Some("NU6.3"))
                .then(|| upgrade.get("activationheight").and_then(Value::as_u64))
                .flatten()
        })
        .context("the validator on The Public Testnet must report an NU6.3 activation height")?
        as u32;
    assert_eq!(
        boundary, OBSERVED_NU6_3_ACTIVATION_ON_THE_PUB_TESTNET,
        "the validator's NU6.3 height must match the observed activation on The Public Testnet"
    );

    let tip = blockchain_info
        .get("blocks")
        .and_then(Value::as_u64)
        .context("getblockchaininfo.blocks")? as u32;
    assert!(
        tip > boundary,
        "the testnet fixture's tip {tip} has not crossed the NU6.3 boundary {boundary}; \
         a fixture variant synced past the boundary is required (see the TODO above)"
    );

    // One block below the boundary and at it, plus a block of margin on
    // each side: the Ironwood pool holds no value below the activation
    // height, and the Orchard pool never grows from it.
    for height in (boundary - 2)..boundary {
        assert_eq!(
            pool_zats(&vrpc, height, "ironwood").await?,
            0,
            "the ironwood pool must hold no value at height {height}, below the boundary"
        );
    }
    let walk_end = boundary + 1;
    let mut previous_orchard = pool_zats(&vrpc, boundary - 1, "orchard").await?;
    for height in boundary..=walk_end {
        let orchard = pool_zats(&vrpc, height, "orchard").await?;
        assert!(
            orchard <= previous_orchard,
            "the orchard pool must never grow from the boundary; \
             height {height} holds {orchard} zats after {previous_orchard}"
        );
        previous_orchard = orchard;
    }

    Ok(())
}
