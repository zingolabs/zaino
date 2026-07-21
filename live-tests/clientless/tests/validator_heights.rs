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
//!
//! ---
//!
//! MIGRATION NOTE (ztest harness): both tests below launch an
//! orchard-mining zebrad on `ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS` — a
//! mid-chain NU6.3 transition pinned at height 6 (`NU6_3_TRANSITION_BOUNDARY`).
//! The ztest harness derives network-upgrade heights from the validator
//! image version and can only *lower* the NU ceiling via
//! `Validator::activate_through(NetworkUpgrade::_)`; it cannot pin a
//! mid-chain NU6.3 transition at a chosen height. Both tests are therefore
//! emitted as documented ignore-stubs (see reshape-spec.md capability gap
//! #1). Their exact origin/dev assertions and expected values are preserved
//! verbatim in the bodies below so the coverage intent is on record, and
//! they should be re-homed to `packages/zaino-state` unit tests.

use anyhow::Result;

/// Boundary sync + no-recompile proof: a kind-only-configured zainod adopts
/// the NU6.3-at-6 schedule from the validator, syncs across the boundary,
/// and serves era-correct compact blocks for both eras.
///
/// multi_thread required: the test manager spawns the validator and indexer
/// services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "ztest gap: requires ORCHARD_THEN_IRONWOOD activation heights (NU6.3 pinned at height 6); ztest derives NU heights from image version and cannot pin a mid-chain transition. Re-home to a packages/zaino-state unit test."]
async fn zainod_syncs_a_schedule_its_config_never_saw() -> Result<()> {
    // origin/dev body preserved verbatim as comments (see MIGRATION NOTE):
    //
    // let mut test_manager = launch_transition_validator().await;
    // let subscriber = test_manager.subscriber().clone();
    //
    // // Two blocks past the boundary, so both eras carry more than one block.
    // // Reaching the tip at all is the core regression: pre-adoption, the
    // // chain-index sync died on the first block whose commitment scheme the
    // // misconfigured heights got wrong.
    // test_manager
    //     .generate_blocks_and_wait_for_tip(NU6_3_TRANSITION_BOUNDARY + 1, &subscriber)
    //     .await;
    // let tip = u64::from(subscriber.chain_height().await.expect("chain height").0);
    // assert!(
    //     tip > u64::from(NU6_3_TRANSITION_BOUNDARY),
    //     "sync must cross the boundary, tip is {tip}"
    // );
    //
    // // Era composition of the served chain proves the adopted schedule is the
    // // validator's, not the placeholder: under the placeholder (NU6.3 at 2)
    // // the pre-boundary orchard coinbases would be misread as ironwood-era.
    // let blocks = collect_block_range(&subscriber, 2, tip, all_pools_i32()).await;
    // for block in &blocks {
    //     let height = block.height;
    //     let has_orchard = block.vtx.iter().any(|tx| !tx.actions.is_empty());
    //     let has_ironwood = block.vtx.iter().any(|tx| !tx.ironwood_actions.is_empty());
    //     if height >= u64::from(NU6_3_TRANSITION_BOUNDARY) {
    //         assert!(
    //             has_ironwood && !has_orchard,
    //             "height {height} must be ironwood-era, got orchard={has_orchard} ironwood={has_ironwood}"
    //         );
    //     } else {
    //         assert!(
    //             has_orchard && !has_ironwood,
    //             "height {height} must be orchard-era, got orchard={has_orchard} ironwood={has_ironwood}"
    //         );
    //     }
    // }
    //
    // test_manager.close().await;
    Ok(())
}

/// The input contract for adoption: the `upgrades` map a live zebrad reports
/// for the transition schedule, pinned exactly — upgrade set, order, and
/// heights. Establishes (from real output, not reasoning) that nothing
/// pre-Overwinter appears: the map is keyed by consensus branch ID, which
/// pre-Overwinter eras don't have.
///
/// multi_thread required: the test manager spawns the validator and indexer
/// services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "ztest gap: requires ORCHARD_THEN_IRONWOOD activation heights (NU6.3 pinned at height 6); ztest derives NU heights from image version and cannot pin a mid-chain transition. Re-home to a packages/zaino-state unit test."]
async fn getblockchaininfo_reports_the_configured_schedule() -> Result<()> {
    // origin/dev body preserved verbatim as comments (see MIGRATION NOTE):
    //
    // use zebra_chain::parameters::NetworkUpgrade;
    //
    // let mut test_manager = launch_transition_validator().await;
    //
    // let blockchain_info = test_manager
    //     .full_node_jsonrpc_connector()
    //     .await
    //     .get_blockchain_info()
    //     .await
    //     .expect("getblockchaininfo");
    //
    // let reported: Vec<(NetworkUpgrade, u32)> = blockchain_info
    //     .upgrades
    //     .values()
    //     .map(|upgrade_info| {
    //         let (upgrade, height, _status) = upgrade_info.into_parts();
    //         (upgrade, height.0)
    //     })
    //     .collect();
    //
    // assert_eq!(
    //     reported,
    //     vec![
    //         (NetworkUpgrade::Overwinter, 1),
    //         (NetworkUpgrade::Sapling, 1),
    //         (NetworkUpgrade::Blossom, 1),
    //         (NetworkUpgrade::Heartwood, 1),
    //         (NetworkUpgrade::Canopy, 1),
    //         (NetworkUpgrade::Nu5, 2),
    //         (NetworkUpgrade::Nu6, 2),
    //         (NetworkUpgrade::Nu6_1, 2),
    //         (NetworkUpgrade::Nu6_2, 2),
    //         (NetworkUpgrade::Nu6_3, 6),
    //     ],
    // );
    //
    // test_manager.close().await;
    Ok(())
}
