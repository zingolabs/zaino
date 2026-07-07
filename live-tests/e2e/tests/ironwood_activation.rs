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
use zaino_state::{ZcashIndexer, ZcashService};
use zaino_testutils::{
    PollableTip, TestManager, TestService, ValidatorKind, NU6_3_TRANSITION_BOUNDARY,
    ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS,
};
use zainodlib::error::IndexerError;
use zcash_local_net::validator::zebrad::Zebrad;

/// Launch an orchard-receiver-mining zebrad + Zaino on the transition
/// heights, build devtool faucet/recipient wallets (their schedule derived
/// from the launched validator), mine one block (height 2: the first
/// Orchard-era coinbase note), and sync the faucet. The transition-fixture
/// analogue of `devtool.rs::launch_and_fund_faucet`.
async fn launch_transition_chain_and_fund_faucet<Service>(
) -> (TestManager<Zebrad, Service>, DevtoolClients)
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
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

/// Orchard-era receipt: with the tip still below the boundary, the faucet's
/// coinbase note is an Orchard note (not Ironwood), and a unified-address
/// send received before the boundary lands in the recipient's Orchard pool
/// with the Ironwood pool exactly empty — the era-mirror of
/// `devtool.rs::send_to_pool(Ironwood)`.
async fn unified_receipt_lands_in_orchard_before_boundary<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (mut test_manager, mut clients) =
        launch_transition_chain_and_fund_faucet::<Service>().await;

    // Tip is 2: inside the Orchard era, with room to confirm the send at
    // height 3 while staying below the boundary at 6.
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
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (mut test_manager, mut clients) =
        launch_transition_chain_and_fund_faucet::<Service>().await;

    let pre_boundary_balance = clients.faucet_balance().await;
    assert!(
        pre_boundary_balance.orchard_spendable > 0,
        "pre-boundary coinbase should be an orchard note, got {pre_boundary_balance:?}"
    );

    // Tip is 2; mine to the boundary itself. Heights 3–5 add more Orchard
    // coinbase notes, height 6 is the first Ironwood-era block (its coinbase
    // is the faucet's first Ironwood note).
    test_manager
        .generate_blocks_and_wait_for_tip(NU6_3_TRANSITION_BOUNDARY - 2, test_manager.subscriber())
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

    test_manager.close().await;
}

mod zebrad {
    // FetchService is a deprecated re-export; the deprecation fires at the
    // turbofish use sites below, so the allow covers the whole module.
    #[allow(deprecated)]
    mod fetch_service {
        use zaino_state::FetchService;

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn unified_receipt_lands_in_orchard_before_boundary() {
            crate::unified_receipt_lands_in_orchard_before_boundary::<FetchService>().await;
        }

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn orchard_note_spends_to_ironwood_across_boundary() {
            crate::orchard_note_spends_to_ironwood_across_boundary::<FetchService>().await;
        }
    }

    mod state_service {
        use zaino_state::StateService;

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn unified_receipt_lands_in_orchard_before_boundary() {
            crate::unified_receipt_lands_in_orchard_before_boundary::<StateService>().await;
        }

        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[tokio::test(flavor = "multi_thread")]
        async fn orchard_note_spends_to_ironwood_across_boundary() {
            crate::orchard_note_spends_to_ironwood_across_boundary::<StateService>().await;
        }
    }
}
