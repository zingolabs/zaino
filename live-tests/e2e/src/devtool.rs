//! zcash-devtool-backed wallet clients: the in-progress replacement for the
//! zingolib lightclients in [`crate::Clients`]
//! (zingolabs/infrastructure#269).
//!
//! [`DevtoolClients`] mirrors [`crate::Clients`]' method names one-for-one so
//! tests can swap backends mechanically. The clients are managed by
//! zcash_local_net's [`zcash_local_net::wallet`] module: each wallet
//! operation is a run-to-completion `zcash-devtool` subprocess invocation
//! (the binary must be built with `--features regtest_support` and be
//! locatable via `TEST_BINARIES_DIR`/`PATH`).
//!
//! # Known gaps vs the zingolib backend
//!
//! - **Unconfirmed (mempool) balances**: devtool sync is block-based;
//!   `monitor_unverified_mempool` cannot swap backends.
//! - **No transaction listing**: the `transaction_summaries` asserts in
//!   `get_address_utxos{,_stream}` need raw-gRPC / demotion treatment before
//!   their tests swap. (`do_info` is covered now: see
//!   [`DevtoolClients::get_info_faucet`].)
//! - **Fee constants**: ZIP-317 applies on both backends, but asserted
//!   constants derived from zingolib note selection (e.g. 235_000 after
//!   shielding 250_000) must be re-verified on first devtool runs.

use zcash_local_net::wallet::{
    zcash_devtool::{ZcashDevtool, ZcashDevtoolConfig},
    AddressReceiver, GetInfo, Wallet as _, WalletBalance,
};
use zcash_primitives::transaction::TxId;

use crate::Pool;

/// Holds devtool wallet clients for wallet-to-validator tests: the faucet
/// (mining rewards are received here) and the recipient.
pub struct DevtoolClients {
    /// Faucet wallet (abandon-art mnemonic — owns the miner addresses).
    pub faucet: ZcashDevtool,
    /// Recipient wallet (HOSPITAL_MUSEUM mnemonic).
    pub recipient: ZcashDevtool,
}

/// Launch faucet + recipient devtool wallets against a running Zaino gRPC
/// listener, deriving the wallet network from the running `validator`. That
/// derivation is the only construction the client API offers
/// (infrastructure ADR 0003: the Validator is the single source of truth
/// for activation heights), so wallet/validator height drift is
/// unrepresentable — the wallets follow whatever schedule the validator was
/// launched with, fixture or canonical. The devtool analogue of
/// [`crate::build_clients`]; Zaino must already be serving (wallet
/// initialization fetches the chain tip and birthday tree state from it).
pub async fn build_clients<V: zcash_local_net::validator::Validator>(
    zaino_grpc_listen_port: u16,
    validator: &V,
) -> DevtoolClients {
    let network = zcash_local_net::wallet::WalletNetwork::from_validator(validator).await;

    let mut faucet_config = ZcashDevtoolConfig::faucet(network);
    faucet_config.indexer_port = zaino_grpc_listen_port;
    let faucet = ZcashDevtool::launch(faucet_config)
        .await
        .expect("launch devtool faucet wallet");

    let mut recipient_config = ZcashDevtoolConfig::recipient(network);
    recipient_config.indexer_port = zaino_grpc_listen_port;
    let recipient = ZcashDevtool::launch(recipient_config)
        .await
        .expect("launch devtool recipient wallet");

    DevtoolClients { faucet, recipient }
}

/// Convert a devtool txid — the hex string `send`/`shield` return, which
/// devtool prints in display (reversed) order via `TxId`'s `Display` — into
/// the internal-order 32 bytes that zaino's `TxFilter` and compact-tx
/// comparisons use (the order zingolib's `TxId::as_ref()` yields). Any test
/// that then queries zaino with the result validates the order: a wrong one
/// simply fails to match the indexed transaction.
pub fn txid_internal_bytes(devtool_txid_hex: &str) -> Vec<u8> {
    let mut bytes = hex::decode(devtool_txid_hex.trim()).expect("devtool txid is valid hex");
    bytes.reverse();
    bytes
}

/// [`txid_internal_bytes`] as a [`TxId`], for asserting on compact-block
/// contents (e.g. [`crate::assert_pool_present`]).
pub fn txid_from_devtool(devtool_txid_hex: &str) -> TxId {
    let bytes: [u8; 32] = txid_internal_bytes(devtool_txid_hex)
        .try_into()
        .expect("devtool txid is 32 bytes");
    TxId::from_bytes(bytes)
}

impl DevtoolClients {
    /// The address of `client` that routes funds into `pool`, read from the
    /// wallet's default unified address (`"transparent"`/`"sapling"` emit the
    /// bare receiver, `"unified"`/`"orchard"` the unified/orchard-only
    /// address). Shared by [`DevtoolClients::get_faucet_address`] and
    /// [`DevtoolClients::get_recipient_address`].
    async fn address(client: &ZcashDevtool, who: &str, pool: &str) -> String {
        let receiver = match pool {
            "transparent" => AddressReceiver::Transparent,
            "sapling" => AddressReceiver::Sapling,
            "unified" => AddressReceiver::Unified,
            "orchard" => AddressReceiver::Orchard,
            other => panic!("unknown pool address kind {other:?} for {who}"),
        };
        client
            .address(receiver)
            .await
            .unwrap_or_else(|e| panic!("address({pool}) for {who}: {e:?}"))
    }

    /// The faucet address that routes funds into `pool`
    /// (`"transparent" | "sapling" | "unified"`). For the faucet (the miner's
    /// wallet), the transparent address is the one the miner pays coinbase to.
    pub async fn get_faucet_address(&self, pool: &str) -> String {
        Self::address(&self.faucet, "faucet", pool).await
    }

    /// The recipient address that routes funds into `pool`
    /// (`"transparent" | "sapling" | "unified"`).
    pub async fn get_recipient_address(&self, pool: &str) -> String {
        Self::address(&self.recipient, "recipient", pool).await
    }

    /// The faucet's balance snapshot. Sync first; this reads the local
    /// wallet database.
    pub async fn faucet_balance(&self) -> WalletBalance {
        Self::balance(&self.faucet, "faucet").await
    }

    /// The recipient's balance snapshot. Sync first; this reads the local
    /// wallet database.
    pub async fn recipient_balance(&self) -> WalletBalance {
        Self::balance(&self.recipient, "recipient").await
    }

    async fn balance(client: &ZcashDevtool, who: &str) -> WalletBalance {
        client
            .balance()
            .await
            .unwrap_or_else(|e| panic!("balance for {who}: {e:?}"))
    }

    /// The faucet wallet's server/chain info (devtool `wallet get-info`).
    /// The connect smoke test only asserts the call succeeds.
    pub async fn get_info_faucet(&self) -> GetInfo {
        Self::get_info(&self.faucet, "faucet").await
    }

    /// The recipient wallet's server/chain info (devtool `wallet get-info`).
    pub async fn get_info_recipient(&self) -> GetInfo {
        Self::get_info(&self.recipient, "recipient").await
    }

    async fn get_info(client: &ZcashDevtool, who: &str) -> GetInfo {
        client
            .get_info()
            .await
            .unwrap_or_else(|e| panic!("get_info for {who}: {e:?}"))
    }

    /// Send `amount` zatoshis from `client` to `address`. Shared by
    /// [`DevtoolClients::send_from_faucet`] and
    /// [`DevtoolClients::send_from_recipient`]. Returns the broadcast txid
    /// as a hex string (the zingolib backend returns `NonEmpty<TxId>`;
    /// callers that compare txids adapt at the call site).
    async fn send(client: &ZcashDevtool, who: &str, address: &str, amount: u64) -> String {
        client
            .send(address, amount)
            .await
            .unwrap_or_else(|e| panic!("send from {who}: {e:?}"))
    }

    /// Send `amount` zatoshis from the faucet to `address`, returning the
    /// txid hex of the broadcast (unmined) transaction.
    pub async fn send_from_faucet(&mut self, address: &str, amount: u64) -> String {
        Self::send(&self.faucet, "faucet", address, amount).await
    }

    /// Send `amount` zatoshis from the recipient to `address`, returning the
    /// txid hex of the broadcast (unmined) transaction.
    pub async fn send_from_recipient(&mut self, address: &str, amount: u64) -> String {
        Self::send(&self.recipient, "recipient", address, amount).await
    }

    /// Shield `client`'s transparent funds into orchard. Shared by
    /// [`DevtoolClients::shield_faucet`] and
    /// [`DevtoolClients::shield_recipient`].
    async fn shield(client: &ZcashDevtool, who: &str) {
        client
            .shield()
            .await
            .unwrap_or_else(|e| panic!("shield {who}: {e:?}"));
    }

    /// Shield the faucet's transparent funds into orchard.
    pub async fn shield_faucet(&mut self) {
        Self::shield(&self.faucet, "faucet").await;
    }

    /// Shield the recipient's transparent funds into orchard.
    pub async fn shield_recipient(&mut self) {
        Self::shield(&self.recipient, "recipient").await;
    }

    /// Sync `client`'s wallet to the chain tip. Shared by
    /// [`DevtoolClients::sync_faucet`] and [`DevtoolClients::sync_recipient`].
    async fn sync(client: &ZcashDevtool, who: &str) {
        client
            .sync()
            .await
            .unwrap_or_else(|e| panic!("sync {who}: {e:?}"));
    }

    /// Sync the faucet wallet to the chain tip.
    pub async fn sync_faucet(&mut self) {
        Self::sync(&self.faucet, "faucet").await;
    }

    /// Sync the recipient wallet to the chain tip.
    pub async fn sync_recipient(&mut self) {
        Self::sync(&self.recipient, "recipient").await;
    }

    /// Forget all of the recipient wallet's state, then sync from scratch.
    ///
    /// Unlike the zingolib backend, the rebuilt view contains only mined
    /// history — devtool sync does not scan the mempool, so unmined
    /// transactions will not reappear (see module docs).
    pub async fn rescan_recipient(&mut self) {
        self.recipient
            .rescan()
            .await
            .unwrap_or_else(|e| panic!("rescan recipient: {e:?}"));
        self.sync_recipient().await;
    }
}

impl Pool {
    /// The spendable balance received in this pool, in zatoshis — the
    /// devtool-backend counterpart of [`Pool::received_balance`]. Spendable
    /// equals received once the funding transaction is mined and the wallet
    /// synced, which is the state every asserting test establishes first.
    pub fn spendable_balance(self, balance: &WalletBalance) -> u64 {
        match self {
            Pool::Orchard => balance.orchard_spendable,
            Pool::Ironwood => balance.ironwood_spendable,
            Pool::Sapling => balance.sapling_spendable,
            Pool::Transparent => balance.transparent_spendable,
        }
    }
}

/// Launch a shielded-mining validator of `validator` kind (at `activation_heights`,
/// or the kind's defaults on `None`) with Zaino serving gRPC, and build the devtool
/// faucet/recipient wallets against it, without mining or syncing. The shared body
/// of the per-validator `launch_*_and_build_clients` preambles in the devtool test
/// binaries.
pub async fn launch_and_build_devtool_clients<V, Conn>(
    validator: &zaino_testutils::ValidatorKind,
    activation_heights: Option<zaino_common::network::ActivationHeights>,
) -> (zaino_testutils::TestManager<V, Conn>, DevtoolClients)
where
    V: zaino_testutils::ValidatorExt,
    Conn: zaino_testutils::ValidatorConnectionMarker,
{
    let test_manager = zaino_testutils::TestManager::<V, Conn>::launch_mining_to(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        validator,
        None, // network -> Regtest
        activation_heights,
        None,  // no chain cache: build fresh
        true,  // enable zaino
        false, // no json-rpc server
        false, // no clients (the devtool wallet is built separately)
    )
    .await
    .expect("launch TestManager");

    let clients = build_clients(
        test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
        &test_manager.local_net,
    )
    .await;

    (test_manager, clients)
}

/// The faucet sends 250_000 zatoshis to the recipient's `pool` address, the send is
/// mined in, and the recipient's synced wallet shows the receipt in that pool.
/// Shared body of the per-validator `send_to_pool` tests; the caller launches and
/// funds the faucet first.
pub async fn assert_send_to_pool<V, Conn>(
    mut test_manager: zaino_testutils::TestManager<V, Conn>,
    mut clients: DevtoolClients,
    pool: Pool,
) where
    V: zaino_testutils::ValidatorExt,
    Conn: zaino_testutils::ValidatorConnectionMarker,
{
    let recipient = clients.get_recipient_address(pool.address_kind()).await;
    let txid = clients.send_from_faucet(&recipient, 250_000).await;
    dbg!(txid);

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    assert_eq!(
        pool.spendable_balance(&clients.recipient_balance().await),
        250_000
    );

    test_manager.close().await;
}

/// The recipient receives a transparent send, confirms it, shields it, and confirms
/// the shielded balance net of the ZIP-317 fee (250_000 − 15_000 = 235_000) in
/// `shielded_pool` — [`Pool::Ironwood`] under NU6.3-era heights (devtool routes
/// `shield` there), [`Pool::Orchard`] under pre-NU6.3 heights. Shared body of the
/// per-validator `shield` tests; the caller launches and funds the faucet first.
pub async fn assert_shield_for_validator<V, Conn>(
    mut test_manager: zaino_testutils::TestManager<V, Conn>,
    mut clients: DevtoolClients,
    shielded_pool: Pool,
) where
    V: zaino_testutils::ValidatorExt,
    Conn: zaino_testutils::ValidatorConnectionMarker,
{
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    assert_eq!(
        Pool::Transparent.spendable_balance(&clients.recipient_balance().await),
        250_000
    );

    clients.shield_recipient().await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    assert_eq!(
        shielded_pool.spendable_balance(&clients.recipient_balance().await),
        235_000
    );

    test_manager.close().await;
}

/// A transparent send returns the same address txids from the non-finalized chain
/// and again after a seam-deep advance lands it in the finalized DB. Shared body of
/// the per-validator gated `send_to_transparent_finalization` tests; the caller
/// launches and funds the faucet first. The advance mines shielded coinbase, so the
/// callers stay `#[ignore]`d until per-call cheap filler mining lands.
pub async fn assert_send_to_transparent_finalization<V, Conn>(
    mut test_manager: zaino_testutils::TestManager<V, Conn>,
    mut clients: DevtoolClients,
) where
    V: zaino_testutils::ValidatorExt,
    Conn: zaino_testutils::ValidatorConnectionMarker,
{
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;

    let oracle = test_manager.full_node_jsonrpc_connector().await;
    let height = oracle.get("getblockchaininfo").await["blocks"]
        .as_u64()
        .expect("a chain height") as u32;
    let address_txids = async |address: &str| {
        oracle
            .call(
                "getaddresstxids",
                vec![serde_json::json!({
                    "addresses": [address],
                    "start": height,
                    "end": height,
                })],
            )
            .await
    };
    let unfinalised_transactions = address_txids(&recipient_taddr).await;

    // The load-bearing advance: these blocks push the send below the seam
    // (`FAST_TEST_MAX_NONFINALISED_DEPTH`) into the finalized DB.
    test_manager
        .generate_blocks_bulk_and_wait_for_tips(
            // Advance past the seam so the send crosses the finalised floor
            // (`tip - seam`); a small margin above it keeps the boundary unambiguous.
            zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH + 5,
            test_manager.subscriber(),
            test_manager.subscriber(),
        )
        .await;

    let finalised_transactions = address_txids(&recipient_taddr).await;

    clients.sync_recipient().await;
    assert_eq!(
        Pool::Transparent.spendable_balance(&clients.recipient_balance().await),
        250_000
    );
    assert_eq!(unfinalised_transactions, finalised_transactions);

    test_manager.close().await;
}
