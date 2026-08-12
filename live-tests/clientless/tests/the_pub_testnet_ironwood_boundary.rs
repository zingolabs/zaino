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
//! That is ztest's `IRONWOOD` snapshot — pinned at 4,140,000, six thousand
//! blocks past the activation — paired with a zainod indexer on the same
//! artifact.
//!
//! `IRONWOOD` is the deep artifact (8.2 GiB compressed) and does **not**
//! currently mount: streaming it through the seed uploader's stdin cannot
//! finish inside `materialize::WAIT_BUDGET` (300 s), which covers every wait
//! including the upload. So this test fails at materialization with that
//! budget error rather than the `No such file or directory` it used to fail
//! with — the artifact now exists and is correctly pinned; the harness limit
//! is the remaining blocker.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;
use ztest::snapshots::testnet::IRONWOOD;

const READY: Duration = Duration::from_secs(300);

/// The height The Public Testnet activated NU6.3 at, recorded from the real
/// activation. A mismatch means either a reset of The Public Testnet or a
/// wrong validator pin — both worth failing loudly over.
const OBSERVED_NU6_3_ACTIVATION_ON_THE_PUB_TESTNET: u32 = 4_134_000;

/// multi_thread required: the test launches the validator pod and polls it
/// over RPC.
#[ztest::needs(IRONWOOD)]
#[ztest::qos::testnet]
#[tokio::test(flavor = "multi_thread")]
async fn value_pools_respect_the_boundary_on_the_pub_testnet() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").testnet(IRONWOOD));
    // Validator only. This test's claim is about the chain — what the consensus
    // rule did to the value pools — and every assertion below reads the
    // validator. The zainod pod that used to stand here was never queried, so
    // the only thing it asserted was its own readiness, undeclared, at the cost
    // of a second CoW clone of the deepest fixture in the suite. Parity for
    // zaino on this fixture belongs in `testnet_parity.rs` as an `IRONWOOD`
    // case, where it would be stated rather than implied.
    // `build` has already established that this validator serves the chain
    // IRONWOOD's manifest describes, and that it carries mature history above
    // the activation — so the walk below can spend its assertions on the
    // consensus rule rather than on the fixture.
    env.build().await?;

    let vrpc = validator.json_rpc().await?;

    // The validator's reported schedule is the source of truth for the
    // boundary; the recorded constant pins the real history of The Public
    // Testnet, which is a claim about the network rather than about the
    // artifact and so is checked here rather than by the harness.
    let boundary = vrpc.activation_height("NU6.3").await?;
    assert_eq!(
        boundary, OBSERVED_NU6_3_ACTIVATION_ON_THE_PUB_TESTNET,
        "the validator's NU6.3 height must match the observed activation on The Public Testnet"
    );

    // One block below the boundary and at it, plus a block of margin on
    // each side: the Ironwood pool holds no value below the activation
    // height, and the Orchard pool never grows from it.
    for height in (boundary - 2)..boundary {
        assert_eq!(
            vrpc.pool_zats(height, "ironwood").await?,
            0,
            "the ironwood pool must hold no value at height {height}, below the boundary"
        );
    }
    let walk_end = boundary + 1;
    let mut previous_orchard = vrpc.pool_zats(boundary - 1, "orchard").await?;
    for height in boundary..=walk_end {
        let orchard = vrpc.pool_zats(height, "orchard").await?;
        assert!(
            orchard <= previous_orchard,
            "the orchard pool must never grow from the boundary; \
             height {height} holds {orchard} zats after {previous_orchard}"
        );
        previous_orchard = orchard;
    }

    Ok(())
}
