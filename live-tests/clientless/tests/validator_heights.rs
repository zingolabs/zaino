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

use zaino_state::ZcashIndexer as _;
#[allow(deprecated)]
use zaino_testutils::Rpc;
use zaino_testutils::{
    all_pools_i32, collect_block_range, MinerPool, TestManager, ValidatorKind,
    NU6_3_TRANSITION_BOUNDARY, ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS,
    ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS,
};
use zcash_local_net::validator::zebrad::Zebrad;

/// Launches an orchard-receiver-mining zebrad on the transition schedule
/// with zainod enabled. The harness hands zainod only the canonical
/// placeholder (see `launch_mining_to`), so the launch itself is the
/// deliberate config/validator misalignment under test.
#[allow(deprecated)]
async fn launch_transition_validator() -> TestManager<Zebrad, Rpc> {
    assert_ne!(
        ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS, ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS,
        "premise: zainod's config placeholder must differ from the fixture \
         schedule, or this proves nothing"
    );

    TestManager::<Zebrad, Rpc>::launch_mining_to(
        MinerPool::Orchard,
        &ValidatorKind::Zebrad,
        None,
        Some(ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS),
        None,
        true,
        false,
        false,
    )
    .await
    .expect("launch TestManager")
}

/// Boundary sync + no-recompile proof: a kind-only-configured zainod adopts
/// the NU6.3-at-6 schedule from the validator, syncs across the boundary,
/// and serves era-correct compact blocks for both eras.
///
/// multi_thread required: the test manager spawns the validator and indexer
/// services.
#[tokio::test(flavor = "multi_thread")]
async fn zainod_syncs_a_schedule_its_config_never_saw() {
    let mut test_manager = launch_transition_validator().await;
    let subscriber = test_manager.subscriber().clone();

    // Two blocks past the boundary, so both eras carry more than one block.
    // Reaching the tip at all is the core regression: pre-adoption, the
    // chain-index sync died on the first block whose commitment scheme the
    // misconfigured heights got wrong.
    test_manager
        .generate_blocks_and_wait_for_tip(NU6_3_TRANSITION_BOUNDARY + 1, &subscriber)
        .await;
    let tip = u64::from(subscriber.chain_height().await.expect("chain height").0);
    assert!(
        tip > u64::from(NU6_3_TRANSITION_BOUNDARY),
        "sync must cross the boundary, tip is {tip}"
    );

    // Era composition of the served chain proves the adopted schedule is the
    // validator's, not the placeholder: under the placeholder (NU6.3 at 2)
    // the pre-boundary orchard coinbases would be misread as ironwood-era.
    let blocks = collect_block_range(&subscriber, 2, tip, all_pools_i32()).await;
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

    test_manager.close().await;
}

/// The input contract for adoption: the `upgrades` map a live zebrad reports
/// for the transition schedule, pinned exactly — upgrade set, order, and
/// heights. Establishes (from real output, not reasoning) that nothing
/// pre-Overwinter appears: the map is keyed by consensus branch ID, which
/// pre-Overwinter eras don't have.
///
/// multi_thread required: the test manager spawns the validator and indexer
/// services.
#[tokio::test(flavor = "multi_thread")]
async fn getblockchaininfo_reports_the_configured_schedule() {
    use zebra_chain::parameters::NetworkUpgrade;

    let mut test_manager = launch_transition_validator().await;

    let blockchain_info = test_manager
        .full_node_jsonrpc_connector()
        .await
        .get_blockchain_info()
        .await
        .expect("getblockchaininfo");

    let reported: Vec<(NetworkUpgrade, u32)> = blockchain_info
        .upgrades
        .values()
        .map(|upgrade_info| {
            let (upgrade, height, _status) = upgrade_info.into_parts();
            (upgrade, height.0)
        })
        .collect();

    assert_eq!(
        reported,
        vec![
            (NetworkUpgrade::Overwinter, 1),
            (NetworkUpgrade::Sapling, 1),
            (NetworkUpgrade::Blossom, 1),
            (NetworkUpgrade::Heartwood, 1),
            (NetworkUpgrade::Canopy, 1),
            (NetworkUpgrade::Nu5, 2),
            (NetworkUpgrade::Nu6, 2),
            (NetworkUpgrade::Nu6_1, 2),
            (NetworkUpgrade::Nu6_2, 2),
            (NetworkUpgrade::Nu6_3, 6),
        ],
    );

    test_manager.close().await;
}
