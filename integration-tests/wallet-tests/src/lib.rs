//! Wallet-to-validator integration tests.
//!
//! These exercise zingolib lightclients (a faucet and a recipient) against a
//! running Zaino indexer. They live in their own workspace so the zingolib
//! dependency stack stays out of the zingolib-free `integration-tests`
//! workspace. The clients are built from a launched
//! [`zaino_testutils::TestManager`]'s gRPC address via [`build_clients`].

#![forbid(unsafe_code)]

use nonempty::NonEmpty;
use std::path::PathBuf;
use zaino_common::network::{ActivationHeights, ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS};
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_state::{ZcashIndexer, ZcashService};
use zaino_testutils::{PollableTip, TestManager, TestService, ValidatorExt, ValidatorKind};
use zainodlib::error::IndexerError;
use zcash_primitives::transaction::TxId;
use zebra_chain::parameters::testnet::ConfiguredActivationHeights;
use zebra_chain::parameters::NetworkKind;
use zingo_test_vectors::seeds;
use zingolib::lightclient::LightClient;
use zingolib::wallet::balance::AccountBalance;
use zingolib_testutils::scenarios::ClientBuilder;

/// Re-export so relocated tests keep their original call sites.
pub use zingolib::testutils::lightclient::from_inputs;

/// Holds zingo lightclients along with the lightclient builder for
/// wallet-to-validator tests.
pub struct Clients {
    /// Lightclient builder.
    pub client_builder: ClientBuilder,
    /// Faucet (zingolib lightclient). Mining rewards are received here.
    pub faucet: LightClient,
    /// Recipient (zingolib lightclient).
    pub recipient: LightClient,
}

impl Clients {
    /// Returns the zcash address of the faucet.
    pub async fn get_faucet_address(&self, pool: &str) -> String {
        zingolib::get_base_address_macro!(self.faucet, pool)
    }

    /// Returns the zcash address of the recipient.
    pub async fn get_recipient_address(&self, pool: &str) -> String {
        zingolib::get_base_address_macro!(self.recipient, pool)
    }

    /// The faucet's account-0 balance.
    ///
    /// Only called from the `launch_clients` test module; without the
    /// `allow`, non-test builds (which compile that module out) would
    /// flag it dead.
    #[cfg_attr(not(test), allow(dead_code))]
    async fn faucet_balance(&self) -> AccountBalance {
        self.faucet
            .account_balance(zip32::AccountId::ZERO)
            .await
            .expect("faucet account_balance")
    }

    /// The recipient's account-0 balance.
    pub async fn recipient_balance(&self) -> AccountBalance {
        self.recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .expect("recipient account_balance")
    }

    /// Send `amount` zatoshis from the faucet to `address`, returning the
    /// transaction id(s).
    pub async fn send_from_faucet(&mut self, address: &str, amount: u64) -> NonEmpty<TxId> {
        from_inputs::quick_send(&mut self.faucet, vec![(address, amount, None)])
            .await
            .expect("quick_send from faucet")
    }

    /// Shield `client`'s account-0 transparent funds. Shared by
    /// [`Clients::shield_faucet`] and [`Clients::shield_recipient`].
    async fn shield(client: &mut LightClient, who: &str) {
        client
            .quick_shield(zip32::AccountId::ZERO)
            .await
            .unwrap_or_else(|e| panic!("quick_shield {who}: {e:?}"));
    }

    /// Shield the faucet's account-0 transparent funds.
    pub async fn shield_faucet(&mut self) {
        Self::shield(&mut self.faucet, "faucet").await;
    }

    /// Shield the recipient's account-0 transparent funds.
    pub async fn shield_recipient(&mut self) {
        Self::shield(&mut self.recipient, "recipient").await;
    }

    /// Sync `client`'s wallet to the chain tip. Shared by
    /// [`Clients::sync_faucet`] and [`Clients::sync_recipient`].
    async fn sync(client: &mut LightClient, who: &str) {
        client
            .sync_and_await()
            .await
            .unwrap_or_else(|e| panic!("sync {who}: {e:?}"));
    }

    /// Sync the faucet wallet to the chain tip.
    pub async fn sync_faucet(&mut self) {
        Self::sync(&mut self.faucet, "faucet").await;
    }

    /// Sync the recipient wallet to the chain tip.
    pub async fn sync_recipient(&mut self) {
        Self::sync(&mut self.recipient, "recipient").await;
    }
}

/// A value pool, pairing the recipient-address kind that routes funds into it
/// with the [`AccountBalance`] field that reflects funds received there. Lets a
/// send-and-check test take a single `Pool` instead of an address string plus a
/// balance-field closure.
#[derive(Clone, Copy, Debug)]
pub enum Pool {
    /// Orchard (funds routed via a unified address).
    Orchard,
    /// Sapling.
    Sapling,
    /// Transparent.
    Transparent,
}

impl Pool {
    /// The `get_recipient_address` / `get_faucet_address` pool name that routes
    /// funds into this pool.
    pub fn address_kind(self) -> &'static str {
        match self {
            Pool::Orchard => "unified",
            Pool::Sapling => "sapling",
            Pool::Transparent => "transparent",
        }
    }

    /// The balance received in this pool, in zatoshis.
    pub fn received_balance(self, balance: &AccountBalance) -> u64 {
        match self {
            Pool::Orchard => balance.total_orchard_balance,
            Pool::Sapling => balance.total_sapling_balance,
            Pool::Transparent => balance.confirmed_transparent_balance,
        }
        .expect("pool balance present")
        .into_u64()
    }
}

/// Whether the compact tx with `txid` carries no data for `pool` (transparent
/// `vout` / sapling `outputs` / orchard `actions`).
fn pool_tx_field_empty(block: &CompactBlock, txid: &TxId, pool: Pool) -> bool {
    let tx = block
        .vtx
        .iter()
        .find(|tx| tx.txid == txid.as_ref().to_vec())
        .expect("sent tx present in compact block");
    match pool {
        Pool::Transparent => tx.vout.is_empty(),
        Pool::Sapling => tx.outputs.is_empty(),
        Pool::Orchard => tx.actions.is_empty(),
    }
}

/// Assert the compact tx with `txid` carries `pool` data.
pub fn assert_pool_present(block: &CompactBlock, txid: &TxId, pool: Pool) {
    assert!(
        !pool_tx_field_empty(block, txid, pool),
        "{pool:?} data should be present in the compact block"
    );
}

/// Assert the compact tx with `txid` carries no `pool` data.
pub fn assert_pool_absent(block: &CompactBlock, txid: &TxId, pool: Pool) {
    assert!(
        pool_tx_field_empty(block, txid, pool),
        "{pool:?} data should be absent from the compact block"
    );
}

/// Builds the faucet + recipient lightclients pointed at a running Zaino's
/// gRPC port, seeded from the shared test mnemonic.
///
/// `activation_heights` must match the heights the validator was launched
/// with: [`ActivationHeights::default`] for zcashd,
/// [`zaino_common::network::ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS`] for zebrad.
pub fn build_clients(
    zaino_grpc_listen_port: u16,
    activation_heights: ActivationHeights,
) -> Clients {
    let mut client_builder = ClientBuilder::new(
        zaino_testutils::make_uri(zaino_grpc_listen_port),
        tempfile::tempdir().expect("create tempdir for lightclient wallets"),
    );

    let configured_activation_heights: ConfiguredActivationHeights = activation_heights.into();
    let faucet = client_builder.build_faucet(true, configured_activation_heights);
    let recipient = client_builder.build_client(
        seeds::HOSPITAL_MUSEUM_SEED.to_string(),
        1,
        true,
        configured_activation_heights,
    );
    Clients {
        client_builder,
        faucet,
        recipient,
    }
}

/// The activation heights `TestManager::launch` uses by default for a given
/// validator (i.e. when launched with `activation_heights: None`). Relocated
/// wallet helpers that are generic over the validator use this to build clients
/// whose view matches the launched chain.
pub fn default_heights(validator: &ValidatorKind) -> ActivationHeights {
    match validator {
        ValidatorKind::Zcashd => ActivationHeights::default(),
        ValidatorKind::Zebrad => ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS,
    }
}

/// Launch a `TestManager<C, Service>` and build faucet/recipient lightclients
/// whose view matches the launched chain — the shared "launch + build_clients"
/// step used by both the smoke tests and the wallet_to_validator tests.
pub async fn launch_and_build<C, Service>(
    validator: &ValidatorKind,
    network: Option<NetworkKind>,
    chain_cache: Option<PathBuf>,
) -> (TestManager<C, Service>, Clients)
where
    C: ValidatorExt,
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let test_manager = TestManager::<C, Service>::launch(
        validator,
        network,
        None,
        chain_cache,
        true,
        false,
        false,
    )
    .await
    .expect("launch TestManager");
    let clients = build_clients(
        test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
        default_heights(validator),
    );
    (test_manager, clients)
}

/// Mine `blocks` and sync the faucet to the new tip, waiting on
/// `mined_against` and then `then_synced`. One half of the faucet funding
/// step; the other half is [`Clients::shield_faucet`]. Callers with a single
/// subscriber pass it as both `mined_against` and `then_synced`
/// ([`TestManager::generate_blocks_and_wait_for_tips`] waits on each in turn,
/// so the repeated wait is a no-op).
#[allow(deprecated)]
pub async fn mine_and_sync_faucet<C, Service, A, B>(
    test_manager: &TestManager<C, Service>,
    clients: &mut Clients,
    mined_against: &A,
    then_synced: &B,
    blocks: u32,
) where
    C: ValidatorExt,
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
    A: PollableTip,
    B: PollableTip,
{
    test_manager
        .generate_blocks_and_wait_for_tips(blocks, mined_against, then_synced)
        .await;
    clients.sync_faucet().await;
}

/// Run faucet shield rounds: for each entry of `round_blocks`, mine that many
/// blocks, sync the faucet, and shield its transparent funds; then mine
/// `final_blocks` and sync so the last shield is spendable. `&[100; n]` matures
/// a fresh coinbase batch before every shield; `&[100, 1, 1]` matures once and
/// spreads three shields over consecutive blocks.
#[allow(deprecated)]
pub async fn shield_faucet_rounds<C, Service, A, B>(
    test_manager: &TestManager<C, Service>,
    clients: &mut Clients,
    mined_against: &A,
    then_synced: &B,
    round_blocks: &[u32],
    final_blocks: u32,
) where
    C: ValidatorExt,
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
    A: PollableTip,
    B: PollableTip,
{
    for &blocks in round_blocks {
        mine_and_sync_faucet(test_manager, clients, mined_against, then_synced, blocks).await;
        clients.shield_faucet().await;
    }
    mine_and_sync_faucet(test_manager, clients, mined_against, then_synced, final_blocks).await;
}

/// Sync the faucet and, on zebrad, run `shield_rounds` rounds of "mature 100
/// coinbase blocks, sync, shield", then mine one block and sync — waiting on
/// both subscribers each time. The dual-subscriber funding preamble shared by
/// the state_service and json_server tests; zcashd needs no shield, so the
/// zebrad-only branch is skipped there.
#[allow(deprecated)]
pub async fn fund_faucet_dual<C, Service, A, B>(
    test_manager: &TestManager<C, Service>,
    clients: &mut Clients,
    validator: &ValidatorKind,
    mined_against: &A,
    then_synced: &B,
    shield_rounds: u32,
) where
    C: ValidatorExt,
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
    A: PollableTip,
    B: PollableTip,
{
    clients.sync_faucet().await;
    if matches!(validator, ValidatorKind::Zebrad) {
        shield_faucet_rounds(
            test_manager,
            clients,
            mined_against,
            then_synced,
            &vec![100; shield_rounds as usize],
            1,
        )
        .await;
    }
}

/// Fund the faucet via [`fund_faucet_dual`], optionally send 250_000 to the
/// recipient's `send` pool address, then mine one block on both subscribers.
/// Returns the recipient's transparent and unified addresses and the send txid
/// (if `send` was `Some`). The shared dual-subscriber "fund and send one tx"
/// setup behind state_service's `fund_and_send` and json_server's `jsonrpc_fund`.
#[allow(deprecated)]
pub async fn fund_and_send_dual<C, Service, A, B>(
    test_manager: &TestManager<C, Service>,
    clients: &mut Clients,
    validator: &ValidatorKind,
    mined_against: &A,
    then_synced: &B,
    shield_rounds: u32,
    send: Option<Pool>,
) -> (String, String, Option<NonEmpty<TxId>>)
where
    C: ValidatorExt,
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
    A: PollableTip,
    B: PollableTip,
{
    fund_faucet_dual(
        test_manager,
        clients,
        validator,
        mined_against,
        then_synced,
        shield_rounds,
    )
    .await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    let recipient_ua = clients.get_recipient_address("unified").await;
    let txid = if let Some(pool) = send {
        let addr = clients.get_recipient_address(pool.address_kind()).await;
        Some(clients.send_from_faucet(&addr, 250_000).await)
    } else {
        None
    };
    test_manager
        .generate_blocks_and_wait_for_tips(1, mined_against, then_synced)
        .await;
    (recipient_taddr, recipient_ua, txid)
}

/// Smoke tests relocated from `zaino-testutils`: launch a validator + Zaino,
/// build wallet clients against it, and exercise mining-reward receipt and
/// sends. Organised by validator / service backend.
///
/// The scenario bodies are shared across the validator × service matrix via
/// the [`Scenario`] extension trait; each per-backend test is a one-line
/// wrapper that supplies the concrete `C`/`Service` by turbofish.
#[cfg(test)]
mod launch_clients {
    use super::{from_inputs, Clients};
    use std::path::PathBuf;
    use zaino_state::{ZcashIndexer, ZcashService};
    use zaino_testutils::{PollableTip, TestManager, TestService, ValidatorExt, ValidatorKind};
    use zainodlib::error::IndexerError;
    use zebra_chain::parameters::NetworkKind;

    /// Shared test bodies for the validator × service backend matrix.
    ///
    /// The `TestManager<C, Service>` method bound — mirroring zaino-testutils'
    /// own `impl` — is stated once on the [`impl`](Scenario) below instead of
    /// on every test, so a per-backend test collapses to a single turbofished
    /// call.
    trait Scenario: Sized {
        /// Launch a validator + Zaino and build faucet/recipient clients whose
        /// view matches the launched chain.
        async fn launch_and_build(
            kind: &ValidatorKind,
            network: Option<NetworkKind>,
            chain_cache: Option<PathBuf>,
        ) -> (Self, Clients);

        /// Connect clients and confirm both answer `do_info`.
        async fn check_clients_connect(
            kind: &ValidatorKind,
            network: Option<NetworkKind>,
            chain_cache: Option<PathBuf>,
        );

        /// Assert the faucet received a mining reward, maturing `mature_blocks`
        /// first (`0` for zcashd, whose launch reward is already spendable).
        async fn check_received_mining_reward(kind: &ValidatorKind, mature_blocks: u32);

        /// Mature a reward, shield it, send to the recipient, and assert receipt.
        async fn check_received_mining_reward_and_send(kind: &ValidatorKind);
    }

    impl<C, Service> Scenario for TestManager<C, Service>
    where
        C: ValidatorExt,
        Service: TestService,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: PollableTip,
    {
        async fn launch_and_build(
            kind: &ValidatorKind,
            network: Option<NetworkKind>,
            chain_cache: Option<PathBuf>,
        ) -> (Self, Clients) {
            super::launch_and_build::<C, Service>(kind, network, chain_cache).await
        }

        async fn check_clients_connect(
            kind: &ValidatorKind,
            network: Option<NetworkKind>,
            chain_cache: Option<PathBuf>,
        ) {
            let (mut test_manager, clients) =
                Self::launch_and_build(kind, network, chain_cache).await;
            dbg!(clients.faucet.do_info().await);
            dbg!(clients.recipient.do_info().await);
            test_manager.close().await;
        }

        async fn check_received_mining_reward(kind: &ValidatorKind, mature_blocks: u32) {
            let (mut test_manager, mut clients) = Self::launch_and_build(kind, None, None).await;

            if mature_blocks > 0 {
                clients.sync_faucet().await;
                dbg!(clients.faucet_balance().await);
                test_manager
                    .generate_blocks_and_wait_for_tip(mature_blocks, test_manager.subscriber())
                    .await;
            }

            clients.sync_faucet().await;
            dbg!(clients.faucet_balance().await);

            assert!(
                clients.faucet_balance().await.total_orchard_balance.unwrap().into_u64() > 0
                    || clients.faucet_balance().await.confirmed_transparent_balance.unwrap().into_u64() > 0,
                "No mining reward received from {kind:?}. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                clients.faucet_balance().await.total_orchard_balance.unwrap().into_u64(),
                clients.faucet_balance().await.confirmed_transparent_balance.unwrap().into_u64()
            );

            test_manager.close().await;
        }

        async fn check_received_mining_reward_and_send(kind: &ValidatorKind) {
            let (mut test_manager, mut clients) = Self::launch_and_build(kind, None, None).await;

            super::mine_and_sync_faucet(
                &test_manager,
                &mut clients,
                test_manager.subscriber(),
                test_manager.subscriber(),
                100,
            )
            .await;
            dbg!(clients.faucet_balance().await);

            assert!(
                clients
                    .faucet_balance()
                    .await
                    .confirmed_transparent_balance
                    .unwrap()
                    .into_u64()
                    > 0,
                "No mining reward received from {kind:?}. Faucet Transparent Balance: {:}.",
                clients
                    .faucet_balance()
                    .await
                    .confirmed_transparent_balance
                    .unwrap()
                    .into_u64()
            );

            // *Send all transparent funds to own orchard address.
            clients.shield_faucet().await;
            super::mine_and_sync_faucet(
                &test_manager,
                &mut clients,
                test_manager.subscriber(),
                test_manager.subscriber(),
                1,
            )
            .await;
            dbg!(clients.faucet_balance().await);

            assert!(
                clients.faucet_balance().await.total_orchard_balance.unwrap().into_u64() > 0,
                "No funds received from shield. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                clients.faucet_balance().await.total_orchard_balance.unwrap().into_u64(),
                clients.faucet_balance().await.confirmed_transparent_balance.unwrap().into_u64()
            );

            let recipient_zaddr = clients.get_recipient_address("sapling").await.to_string();
            from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_zaddr, 250_000, None)])
                .await
                .unwrap();

            test_manager
                .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
                .await;
            clients.sync_recipient().await;
            dbg!(clients.recipient_balance().await);

            assert_eq!(
                clients
                    .recipient_balance()
                    .await
                    .confirmed_sapling_balance
                    .unwrap()
                    .into_u64(),
                250_000
            );

            test_manager.close().await;
        }
    }

    mod zcashd {
        use super::*;
        #[allow(deprecated)]
        use zaino_state::FetchService;
        use zcash_local_net::validator::zcashd::Zcashd;

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn zaino_clients() {
            TestManager::<Zcashd, FetchService>::check_clients_connect(
                &ValidatorKind::Zcashd,
                None,
                None,
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn zaino_clients_receive_mining_reward() {
            TestManager::<Zcashd, FetchService>::check_received_mining_reward(
                &ValidatorKind::Zcashd,
                0,
            )
            .await;
        }
    }

    mod zebrad {
        use super::*;
        use zaino_testutils::ZEBRAD_TESTNET_CACHE_DIR;
        use zcash_local_net::validator::zebrad::Zebrad;

        mod fetch_service {
            use super::*;
            #[allow(deprecated)]
            use zaino_state::FetchService;

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            async fn zaino_clients() {
                TestManager::<Zebrad, FetchService>::check_clients_connect(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            async fn zaino_clients_receive_mining_reward() {
                TestManager::<Zebrad, FetchService>::check_received_mining_reward(
                    &ValidatorKind::Zebrad,
                    100,
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            async fn zaino_clients_receive_mining_reward_and_send() {
                TestManager::<Zebrad, FetchService>::check_received_mining_reward_and_send(
                    &ValidatorKind::Zebrad,
                )
                .await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            async fn zaino_testnet() {
                TestManager::<Zebrad, FetchService>::check_clients_connect(
                    &ValidatorKind::Zebrad,
                    Some(NetworkKind::Testnet),
                    ZEBRAD_TESTNET_CACHE_DIR.clone(),
                )
                .await;
            }
        }

        mod state_service {
            use super::*;
            #[allow(deprecated)]
            use zaino_state::StateService;

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            async fn zaino_clients() {
                TestManager::<Zebrad, StateService>::check_clients_connect(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            async fn zaino_clients_receive_mining_reward() {
                TestManager::<Zebrad, StateService>::check_received_mining_reward(
                    &ValidatorKind::Zebrad,
                    100,
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            async fn zaino_clients_receive_mining_reward_and_send() {
                TestManager::<Zebrad, StateService>::check_received_mining_reward_and_send(
                    &ValidatorKind::Zebrad,
                )
                .await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            async fn zaino_testnet() {
                TestManager::<Zebrad, StateService>::check_clients_connect(
                    &ValidatorKind::Zebrad,
                    Some(NetworkKind::Testnet),
                    ZEBRAD_TESTNET_CACHE_DIR.clone(),
                )
                .await;
            }
        }
    }
}
