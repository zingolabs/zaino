//! Observational walk of the real NU6.3 activation boundary on the public
//! testnet (glossary: "testnet" means the public test network and nothing
//! else). Historical blocks are permanent, so these invariants are checkable
//! forever, long after the epoch that produced them closed.
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
//! heights — and cross-checked against the observed public-testnet
//! activation at height 4,134,000 (~2026-07-04).
//!
//! Non-hermetic and therefore env-gated: the test needs a zebrad chain
//! cache at `~/.cache/zebra` (`ZEBRAD_TESTNET_CACHE_DIR`) synced past the
//! activation height, and skips with a message when the cache is absent or
//! short. Run it manually, or from a job that maintains the cache.
//!
//! The validator is launched through `ZebradConfig` directly rather than
//! `TestManager`: the manager's launch path always writes regtest test
//! parameters into the config, while this test needs the public testnet
//! (`network_type: NetworkType::Testnet`, which makes zebrad ignore
//! `activation_heights` and `miner_address`).

use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_fetch::jsonrpsee::response::GetBlockResponse;
use zaino_testutils::ZEBRAD_TESTNET_CACHE_DIR;
use zcash_local_net::process::Process as _;
use zcash_local_net::protocol::NetworkType;
use zcash_local_net::validator::zebrad::{Zebrad, ZebradConfig};
use zcash_local_net::validator::Validator as _;
use zebra_chain::parameters::NetworkUpgrade;

/// The height the public testnet activated NU6.3 at, recorded from the real
/// activation. A mismatch means either a testnet reset or a wrong validator
/// pin — both worth failing loudly over.
const OBSERVED_TESTNET_NU6_3_ACTIVATION: u32 = 4_134_000;

/// The chain value of `pool_id` as of `height`, from the validator's
/// verbosity-2 block object.
async fn pool_zats(connector: &JsonRpSeeConnector, height: u32, pool_id: &str) -> i64 {
    let response = connector
        .get_block(height.to_string(), Some(2))
        .await
        .expect("getblock verbosity 2");
    let GetBlockResponse::Object(block) = response else {
        panic!("verbosity-2 getblock must return a block object");
    };
    block
        .value_pools()
        .expect("verbosity-2 block object carries value pools")
        .iter()
        .map(|pool| pool.balance())
        .find(|pool| pool.id() == pool_id)
        .unwrap_or_else(|| panic!("value pools must include {pool_id}"))
        .chain_value_zat()
        .zatoshis()
}

/// multi_thread required: the test launches the validator process and polls
/// it over RPC.
#[tokio::test(flavor = "multi_thread")]
async fn value_pools_respect_the_boundary_on_testnet() {
    let Some(cache_dir) = ZEBRAD_TESTNET_CACHE_DIR.clone() else {
        eprintln!("skipping: no testnet cache dir configured");
        return;
    };
    if !cache_dir.exists() {
        eprintln!(
            "skipping: no zebrad testnet chain cache at {}",
            cache_dir.display()
        );
        return;
    }

    let config = ZebradConfig {
        network_type: NetworkType::Testnet,
        chain_cache: Some(cache_dir),
        ..ZebradConfig::default()
    };
    let mut zebrad = Zebrad::launch(config).await.expect("launch testnet zebrad");

    let rpc_address = format!("127.0.0.1:{}", zebrad.get_port());
    let connector = JsonRpSeeConnector::new_with_basic_auth(
        test_node_and_return_url(
            &rpc_address,
            None,
            Some("xxxxxx".to_string()),
            Some("xxxxxx".to_string()),
        )
        .await
        .expect("validator RPC reachable"),
        "xxxxxx".to_string(),
        "xxxxxx".to_string(),
    )
    .expect("connect to the validator RPC");

    let blockchain_info = connector
        .get_blockchain_info()
        .await
        .expect("getblockchaininfo");

    // The validator's reported schedule is the source of truth for the
    // boundary; the recorded constant pins the real public-testnet history.
    let boundary = blockchain_info
        .upgrades
        .values()
        .find_map(|upgrade_info| {
            let (upgrade, height, _status) = upgrade_info.into_parts();
            (upgrade == NetworkUpgrade::Nu6_3).then_some(height.0)
        })
        .expect("the testnet validator must report an NU6.3 activation height");
    assert_eq!(
        boundary, OBSERVED_TESTNET_NU6_3_ACTIVATION,
        "the validator's NU6.3 height must match the observed testnet activation"
    );

    let tip = blockchain_info.blocks.0;
    if tip <= boundary {
        eprintln!(
            "skipping: testnet cache tip {tip} has not crossed the NU6.3 boundary {boundary}"
        );
        zebrad.stop();
        return;
    }

    // One block below the boundary and at it, plus a block of margin on
    // each side: the Ironwood pool holds no value below the activation
    // height, and the Orchard pool never grows from it.
    for height in (boundary - 2)..boundary {
        assert_eq!(
            pool_zats(&connector, height, "ironwood").await,
            0,
            "the ironwood pool must hold no value at height {height}, below the boundary"
        );
    }
    let walk_end = boundary + 1;
    let mut previous_orchard = pool_zats(&connector, boundary - 1, "orchard").await;
    for height in boundary..=walk_end {
        let orchard = pool_zats(&connector, height, "orchard").await;
        assert!(
            orchard <= previous_orchard,
            "the orchard pool must never grow from the boundary; \
             height {height} holds {orchard} zats after {previous_orchard}"
        );
        previous_orchard = orchard;
    }

    zebrad.stop();
}
