//! Wallet-tier predicates across the Orchard→Ironwood activation boundary.
//!
//! Every test here runs a devtool wallet on
//! [`ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS`] — the hermetic replay of what
//! the public testnet did once at height 4,134,000: heights 2 through 5 are
//! Orchard era, [`NU6_3_TRANSITION_BOUNDARY`] (6) onward is Ironwood era.
//! The wallets derive their activation schedule from the running validator
//! (`WalletNetwork::from_validator`, infrastructure ADR 0003), so the
//! fixture heights are typed in exactly one place: the zebrad launch
//! config. Height drift between wallet, indexer, and validator is
//! unrepresentable — zainod adopts the same schedule over
//! `getblockchaininfo` (zaino#1076).
//!
//! # The predicates, and where each era's cell is covered
//!
//! | predicate (wallet-observable)                | Orchard era | Ironwood era |
//! |----------------------------------------------|-------------|--------------|
//! | unified-address receipt lands in Orchard     | here        | false — `devtool.rs` `send_to_ironwood` asserts the Orchard pool stays empty |
//! | unified-address receipt lands in Ironwood    | false (pool inactive) | `devtool.rs` `send_to_ironwood` |
//! | shielded-receiver coinbase pays the era pool | here (wallet view); wire tier in `compact_block_wire.rs` | `devtool.rs` `receives_mining_reward`; wire tier in `compact_block_wire.rs` |
//! | an Orchard note spends into an Ironwood receipt (ZIP 318 migration) | n/a (nothing to exit) | here |
//! | `shield` deposits into the era pool             | here        | `devtool.rs` `shield_for_validator` |
//! | receipt pool flips between the last Orchard block and the activation block | here (boundary − 1) | here (exactly at the boundary) |
//! | Orchard pool value grows only below the boundary; Ironwood holds value only from it | here (per-height value pools) | here |
//!
//! Era composition of the *served* chain (coinbase routing, compact-block
//! action fields) is covered clientless in
//! `clientless/tests/compact_block_consistency.rs` and over the real gRPC
//! wire in `compact_block_wire.rs`; this file owns the cells that need a
//! wallet on both sides of the boundary.
//!
//! The public testnet cannot host the migration cell for us: its pre-NU6.3
//! epoch closed at height 4,134,000, no new value may enter Orchard from
//! there (post-activation Orchard actions permit only same-receiver change
//! or withdrawal — the cross-address restriction,
//! <https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>),
//! and we hold no pre-activation Orchard TAZ — so this hermetic fixture is
//! the only controlled venue for it.
//!
//! Deferred cells the cross-address restriction implies (chain-walk tier,
//! not wallet tier): the Orchard pool value is non-increasing from the
//! boundary, and post-activation Orchard commitments exist only as
//! same-receiver change — note the Orchard note-commitment tree therefore
//! still grows after activation; do not encode a frozen-finalRoot predicate.
//!
//! Requires a `zcash-devtool` binary built with `--features regtest_support`
//! in `TEST_BINARIES_DIR`/`PATH`, alongside the usual validator binaries.

use e2e::devtool::DevtoolClients;
use zaino_state::ZcashIndexer;
use zaino_testutils::{
    all_pools_i32, collect_block_range, PollableTip, TestManager, ValidatorConnectionMarker,
    ValidatorKind, NU6_3_TRANSITION_BOUNDARY, ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS,
};
use zainodlib::error::IndexerError;
use zcash_local_net::validator::zebrad::Zebrad;

/// Launch an orchard-receiver-mining zebrad + Zaino on the transition
/// heights, build devtool faucet/recipient wallets (their schedule derived
/// from the launched validator), mine one block, and sync the faucet. The
/// launch itself already leaves the tip at 2 (`TestManager` mines an
/// NU-activation block after block 1), so the helper returns at tip 3 with
/// the faucet holding the Orchard coinbase notes of heights 2 and 3. Tests
/// that need exact boundary positioning mine to absolute heights from the
/// observed tip rather than counting from here. The transition-fixture
/// analogue of `devtool.rs::launch_and_fund_faucet`.
async fn launch_transition_chain_and_fund_faucet<Service>(
) -> (TestManager<Zebrad, Service>, DevtoolClients)
where
    Service: ValidatorConnectionMarker,
{
    let test_manager = TestManager::<Zebrad, Service>::launch_mining_to(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        &ValidatorKind::Zebrad,
        None,
        Some(ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS),
        None,
        true,
        false,
        false,
    )
    .await
    .expect("launch TestManager");

    let mut clients = e2e::devtool::build_clients(
        test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
        &test_manager.local_net,
    )
    .await;

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_faucet().await;

    (test_manager, clients)
}

/// The chain value (in zatoshis) of the pool named `pool_id` as of
/// `height`, read from the served verbosity-1 block object — the same
/// per-height `valuePools` a zcashd `getblock` reports. Verbosity 1, not 2:
/// the fetch backend cannot deserialize a verbosity-2 block object (its
/// `tx` entries are maps where zaino-fetch expects txid strings), and the
/// value pools ride along at verbosity 1.
async fn pool_zats_at_height<S>(subscriber: &S, height: u32, pool_id: &str) -> i64
where
    S: ZcashIndexer,
    IndexerError: From<S::Error>,
{
    let response = subscriber
        .z_get_block(height.to_string(), Some(1))
        .await
        .map_err(IndexerError::from)
        .expect("z_get_block verbosity 1");
    let zebra_rpc::methods::GetBlock::Object(block) = response else {
        panic!("verbosity-1 getblock must return a block object");
    };
    let pools = block
        .value_pools()
        .as_ref()
        .expect("verbosity-1 block object carries value pools");
    pools
        .iter()
        .find(|pool| pool.id() == pool_id)
        .unwrap_or_else(|| panic!("value pools must include {pool_id}"))
        .chain_value_zat()
        .zatoshis()
}

/// Orchard-era receipt: with the tip still below the boundary, the faucet's
/// coinbase note is an Orchard note (not Ironwood), and a unified-address
/// send received before the boundary lands in the recipient's Orchard pool
/// with the Ironwood pool exactly empty — the era-mirror of
/// `devtool.rs::send_to_pool(Ironwood)`.
async fn unified_receipt_lands_in_orchard_before_boundary<Service>()
where
    Service: ValidatorConnectionMarker,
{
    let (mut test_manager, mut clients) =
        launch_transition_chain_and_fund_faucet::<Service>().await;

    // Tip is 3: inside the Orchard era, with room to confirm the send at
    // height 4 while staying below the boundary at 6.
    let faucet_balance = clients.faucet_balance().await;
    assert!(
        faucet_balance.orchard_spendable > 0,
        "pre-boundary coinbase should be an orchard note, got {faucet_balance:?}"
    );
    assert_eq!(
        faucet_balance.ironwood_spendable, 0,
        "no ironwood note can exist below the boundary, got {faucet_balance:?}"
    );

    let recipient = clients.get_recipient_address("unified").await;
    let txid = clients.send_from_faucet(&recipient, 250_000).await;
    dbg!(txid);

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    let balance = clients.recipient_balance().await;
    assert_eq!(e2e::Pool::Orchard.spendable_balance(&balance), 250_000);
    assert_eq!(e2e::Pool::Ironwood.spendable_balance(&balance), 0);

    test_manager.close().await;
}

/// The ZIP 318 migration shape: an Orchard note minted before the boundary
/// is spent after it, to a unified address generated after activation, and
/// the receipt lands in the Ironwood pool with the recipient's Orchard pool
/// exactly empty. The faucet's Orchard balance must shrink: from the
/// boundary, the cross-address restriction limits each Orchard action to
/// same-receiver change or withdrawal
/// (<https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>),
/// so a genuine Orchard spend nets sent-amount-plus-fee out of the pool even
/// when change returns to the spent note's address.
async fn orchard_note_spends_to_ironwood_across_boundary<Service>()
where
    Service: ValidatorConnectionMarker,
{
    let (mut test_manager, mut clients) =
        launch_transition_chain_and_fund_faucet::<Service>().await;

    let pre_boundary_balance = clients.faucet_balance().await;
    assert!(
        pre_boundary_balance.orchard_spendable > 0,
        "pre-boundary coinbase should be an orchard note, got {pre_boundary_balance:?}"
    );

    // Mine to the boundary itself, from the observed tip rather than a
    // hand-count. The blocks below the boundary add more Orchard coinbase
    // notes; the boundary block is the first Ironwood-era block, and its
    // coinbase is the faucet's first Ironwood note.
    let tip = u32::try_from(test_manager.subscriber().tip_height().await)
        .expect("regtest tips fit in u32");
    test_manager
        .generate_blocks_and_wait_for_tip(
            NU6_3_TRANSITION_BOUNDARY
                .checked_sub(tip)
                .expect("the launch preamble must leave room below the boundary"),
            test_manager.subscriber(),
        )
        .await;
    clients.sync_faucet().await;
    let crossed_balance = clients.faucet_balance().await;
    let orchard_before_send = crossed_balance.orchard_spendable;
    assert!(
        crossed_balance.ironwood_spendable > 0,
        "the boundary coinbase should be an ironwood note, got {crossed_balance:?}"
    );

    // Generated only now — after activation — per the migration shape under
    // test: old-pool note, new-era address.
    let recipient = clients.get_recipient_address("unified").await;
    let txid = clients.send_from_faucet(&recipient, 250_000).await;
    dbg!(txid);

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_faucet().await;
    clients.sync_recipient().await;

    let balance = clients.recipient_balance().await;
    assert_eq!(e2e::Pool::Ironwood.spendable_balance(&balance), 250_000);
    assert_eq!(e2e::Pool::Orchard.spendable_balance(&balance), 0);

    // Pins that the send actually exited the Orchard pool rather than
    // spending the boundary-height Ironwood coinbase — the note-selection
    // question the first live runs of this suite exist to answer.
    assert!(
        clients.faucet_balance().await.orchard_spendable < orchard_before_send,
        "the migration send must spend an orchard note"
    );

    // Chain-tier consequences, read from the served per-height value pools.
    // The Ironwood pool holds no value below the activation height; the
    // Orchard pool grows only below it (its coinbases), holds exactly
    // steady across the boundary edge (the activation block's coinbase pays
    // Ironwood, and no new value may enter Orchard), and shrinks at the
    // migration block by the withdrawn amount plus fee.
    let boundary = NU6_3_TRANSITION_BOUNDARY;
    let migration_height = boundary + 1;
    let subscriber = test_manager.subscriber();
    let mut orchard_at = Vec::new();
    for height in 1..=migration_height {
        let ironwood = pool_zats_at_height(subscriber, height, "ironwood").await;
        let orchard = pool_zats_at_height(subscriber, height, "orchard").await;
        if height < boundary {
            assert_eq!(
                ironwood, 0,
                "the ironwood pool must hold no value at height {height}, below the boundary"
            );
        }
        orchard_at.push(orchard);
    }
    for (index, orchard) in orchard_at.iter().enumerate() {
        let height = index + 1;
        let ironwood = pool_zats_at_height(subscriber, height as u32, "ironwood").await;
        eprintln!("pool values at height {height}: orchard={orchard} ironwood={ironwood}");
    }
    let orchard = |height: u32| orchard_at[height as usize - 1];
    for height in 2..boundary {
        assert!(
            orchard(height) > orchard(height - 1),
            "each pre-boundary coinbase must grow the orchard pool (height {height}: {} after {})",
            orchard(height),
            orchard(height - 1)
        );
    }
    assert_eq!(
        orchard(boundary),
        orchard(boundary - 1),
        "the orchard pool must hold exactly steady across the boundary edge"
    );
    assert!(
        pool_zats_at_height(subscriber, boundary, "ironwood").await > 0,
        "the activation block's coinbase must give the ironwood pool its first value"
    );
    assert!(
        orchard(migration_height) < orchard(boundary),
        "the migration block must shrink the orchard pool ({} at the boundary, {} at the migration block)",
        orchard(boundary),
        orchard(migration_height)
    );

    test_manager.close().await;
}

/// The receipt pool flips between adjacent blocks: a send confirmed in the
/// last Orchard-era block (boundary − 1) lands in Orchard, and a send built
/// one block below the boundary but confirmed in the activation block
/// itself lands in Ironwood. The second send also pins the wallet's era
/// anticipation at the edge: it is constructed while the tip is still
/// Orchard-era, targeting the first Ironwood-era height, and it spends an
/// Orchard note (the faucet holds no Ironwood note until the activation
/// block is mined) — a migration transaction in the activation block.
async fn receipts_flip_pools_exactly_at_the_boundary<Service>()
where
    Service: ValidatorConnectionMarker,
{
    let (mut test_manager, mut clients) =
        launch_transition_chain_and_fund_faucet::<Service>().await;

    // Position the tip at exactly boundary − 2, from the observed tip
    // rather than a hand-count, so the two sends below confirm at exactly
    // boundary − 1 and the boundary.
    let tip = u32::try_from(test_manager.subscriber().tip_height().await)
        .expect("regtest tips fit in u32");
    test_manager
        .generate_blocks_and_wait_for_tip(
            NU6_3_TRANSITION_BOUNDARY
                .checked_sub(2 + tip)
                .expect("the launch preamble must leave room below the boundary"),
            test_manager.subscriber(),
        )
        .await;
    clients.sync_faucet().await;

    let recipient = clients.get_recipient_address("unified").await;

    // Confirmed in block 5: the last Orchard-era block.
    let last_orchard_txid = clients.send_from_faucet(&recipient, 250_000).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_faucet().await;
    clients.sync_recipient().await;
    let balance = clients.recipient_balance().await;
    assert_eq!(
        e2e::Pool::Orchard.spendable_balance(&balance),
        250_000,
        "receipt confirmed at boundary - 1 must be orchard, got {balance:?}"
    );
    assert_eq!(
        e2e::Pool::Ironwood.spendable_balance(&balance),
        0,
        "no ironwood receipt below the boundary, got {balance:?}"
    );

    // Built at tip boundary − 1, confirmed in block 6: the activation block.
    let first_ironwood_txid = clients.send_from_faucet(&recipient, 250_000).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;
    let balance = clients.recipient_balance().await;
    assert_eq!(e2e::Pool::Ironwood.spendable_balance(&balance), 250_000);
    assert_eq!(
        e2e::Pool::Orchard.spendable_balance(&balance),
        250_000,
        "the pre-boundary receipt must survive the flip unchanged"
    );

    // Era composition of the two served edge blocks.
    let subscriber = test_manager.subscriber();
    let boundary = u64::from(NU6_3_TRANSITION_BOUNDARY);
    let blocks = collect_block_range(subscriber, boundary - 1, boundary, all_pools_i32()).await;
    let [last_orchard_block, activation_block] = blocks.as_slice() else {
        panic!("expected exactly the two edge blocks, got {}", blocks.len());
    };
    assert_eq!(last_orchard_block.height, boundary - 1);
    assert_eq!(activation_block.height, boundary);
    let last_orchard_txid = e2e::devtool::txid_from_devtool(&last_orchard_txid);
    e2e::assert_pool_present(last_orchard_block, &last_orchard_txid, e2e::Pool::Orchard);
    e2e::assert_pool_absent(last_orchard_block, &last_orchard_txid, e2e::Pool::Ironwood);
    let first_ironwood_txid = e2e::devtool::txid_from_devtool(&first_ironwood_txid);
    e2e::assert_pool_present(activation_block, &first_ironwood_txid, e2e::Pool::Ironwood);
    // The second send is itself migration-shaped: built one block below the
    // boundary, it spends an Orchard note (the faucet's only spendable kind
    // at that tip), so its compact form carries the Orchard spend's data
    // alongside the Ironwood receipt. The receipt's pool routing is what
    // flips at the boundary, and that is asserted at the wallet tier above.
    e2e::assert_pool_present(activation_block, &first_ironwood_txid, e2e::Pool::Orchard);

    test_manager.close().await;
}

/// Below the boundary, `shield` deposits into the Orchard pool: the faucet
/// funds the recipient's transparent address, the recipient shields, and
/// the shielded balance (net of the ZIP-317 fee, mirroring
/// `devtool.rs::shield_for_validator`) lands in Orchard with the Ironwood
/// pool exactly empty — the era-mirror of the Ironwood-era shield cell.
async fn shield_deposits_to_orchard_before_boundary<Service>()
where
    Service: ValidatorConnectionMarker,
{
    let (mut test_manager, mut clients) =
        launch_transition_chain_and_fund_faucet::<Service>().await;

    // Tip is 3; the transparent receipt confirms at 4 and the shield at 5,
    // all below the boundary at 6.
    let recipient_t = clients.get_recipient_address("transparent").await;
    clients.send_from_faucet(&recipient_t, 250_000).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;
    assert_eq!(
        e2e::Pool::Transparent.spendable_balance(&clients.recipient_balance().await),
        250_000
    );

    clients.shield_recipient().await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    let balance = clients.recipient_balance().await;
    assert_eq!(e2e::Pool::Orchard.spendable_balance(&balance), 235_000);
    assert_eq!(e2e::Pool::Ironwood.spendable_balance(&balance), 0);

    test_manager.close().await;
}

mod zebrad {
    // FetchService is a deprecated re-export; the deprecation fires at the
    // turbofish use sites below, so the allow covers the whole module.
    #[allow(deprecated)]
    mod fetch_service {
        use zaino_testutils::Rpc;

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn unified_receipt_lands_in_orchard_before_boundary() {
            crate::unified_receipt_lands_in_orchard_before_boundary::<Rpc>().await;
        }

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn orchard_note_spends_to_ironwood_across_boundary() {
            crate::orchard_note_spends_to_ironwood_across_boundary::<Rpc>().await;
        }

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn receipts_flip_pools_exactly_at_the_boundary() {
            crate::receipts_flip_pools_exactly_at_the_boundary::<Rpc>().await;
        }

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn shield_deposits_to_orchard_before_boundary() {
            crate::shield_deposits_to_orchard_before_boundary::<Rpc>().await;
        }
    }

    mod state_service {
        use zaino_testutils::Direct;

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn unified_receipt_lands_in_orchard_before_boundary() {
            crate::unified_receipt_lands_in_orchard_before_boundary::<Direct>().await;
        }

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn orchard_note_spends_to_ironwood_across_boundary() {
            crate::orchard_note_spends_to_ironwood_across_boundary::<Direct>().await;
        }

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn receipts_flip_pools_exactly_at_the_boundary() {
            crate::receipts_flip_pools_exactly_at_the_boundary::<Direct>().await;
        }

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn shield_deposits_to_orchard_before_boundary() {
            crate::shield_deposits_to_orchard_before_boundary::<Direct>().await;
        }
    }
}
