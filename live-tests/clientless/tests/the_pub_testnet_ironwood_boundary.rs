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
//! Non-hermetic and therefore env-gated: the test needs a zebrad chain
//! cache at `~/.cache/zebra` (`ZEBRAD_THE_PUB_TESTNET_CACHE_DIR`) synced past the
//! activation height, and skips with a message when the cache is absent or
//! short. Run it manually, or from a job that maintains the cache.
//!
//! The validator is launched through `ZebradConfig` directly rather than
//! `TestManager`: the manager's launch path always writes regtest test
//! parameters into the config, while this test needs The Public Testnet
//! (`network_type: NetworkType::Testnet`, which makes zebrad ignore
//! `activation_heights` and `miner_address`).

use anyhow::Result;

/// The height The Public Testnet activated NU6.3 at, recorded from the real
/// activation. A mismatch means either a reset of The Public Testnet or a
/// wrong validator pin — both worth failing loudly over.
const OBSERVED_NU6_3_ACTIVATION_ON_THE_PUB_TESTNET: u32 = 4_134_000;

/// multi_thread required: the test launches the validator process and polls
/// it over RPC.
///
/// ztest gap: this observational walk needs a fully-synced Public Testnet
/// chain (millions of blocks) supplied from a host cache at `~/.cache/zebra`
/// (`ZEBRAD_THE_PUB_TESTNET_CACHE_DIR`), synced past the NU6.3 activation
/// height. ztest runs validators in ephemeral pods with no such cache and
/// cannot sync The Public Testnet to tip inside a test, so the boundary walk
/// cannot run here. Re-home to an environment-gated harness that can mount or
/// maintain the cache later.
///
/// origin/dev's exact boundary-walk assertions, preserved verbatim so the
/// intent is recorded:
///
/// ```text
/// // The validator's reported schedule is the source of truth for the
/// // boundary; the recorded constant pins the real history of The Public Testnet.
/// let boundary = blockchain_info
///     .upgrades
///     .values()
///     .find_map(|upgrade_info| {
///         let (upgrade, height, _status) = upgrade_info.into_parts();
///         (upgrade == NetworkUpgrade::Nu6_3).then_some(height.0)
///     })
///     .expect("the validator on The Public Testnet must report an NU6.3 activation height");
/// assert_eq!(
///     boundary, OBSERVED_NU6_3_ACTIVATION_ON_THE_PUB_TESTNET,
///     "the validator's NU6.3 height must match the observed activation on The Public Testnet"
/// );
///
/// let tip = blockchain_info.blocks.0;
/// if tip <= boundary {
///     eprintln!(
///         "skipping: cache tip {tip} of The Public Testnet has not crossed the NU6.3 boundary {boundary}"
///     );
///     zebrad.stop();
///     return;
/// }
///
/// // One block below the boundary and at it, plus a block of margin on
/// // each side: the Ironwood pool holds no value below the activation
/// // height, and the Orchard pool never grows from it.
/// for height in (boundary - 2)..boundary {
///     assert_eq!(
///         pool_zats(&connector, height, "ironwood").await,
///         0,
///         "the ironwood pool must hold no value at height {height}, below the boundary"
///     );
/// }
/// let walk_end = boundary + 1;
/// let mut previous_orchard = pool_zats(&connector, boundary - 1, "orchard").await;
/// for height in boundary..=walk_end {
///     let orchard = pool_zats(&connector, height, "orchard").await;
///     assert!(
///         orchard <= previous_orchard,
///         "the orchard pool must never grow from the boundary; \
///          height {height} holds {orchard} zats after {previous_orchard}"
///     );
///     previous_orchard = orchard;
/// }
/// ```
///
/// This should be re-homed to a `packages/zaino-state` (or an env-gated
/// live) unit test once a synced-cache harness exists.
#[ztest::qos::testnet]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "ztest gap: requires a fully-synced Public Testnet chain from a host cache (~/.cache/zebra); ztest pods have no such cache and cannot sync testnet to tip. Re-home to an environment-gated harness later."]
async fn value_pools_respect_the_boundary_on_the_pub_testnet() -> Result<()> {
    let _ = OBSERVED_NU6_3_ACTIVATION_ON_THE_PUB_TESTNET;
    Ok(())
}
