//! zcash-devtool-backed wallet clients: the in-progress replacement for the
//! zingolib lightclients in [`crate::Clients`]
//! (zingolabs/infrastructure#269).
//!
//! [`DevtoolClients`] mirrors [`crate::Clients`]' method names one-for-one so
//! tests can swap backends mechanically. The clients are managed by
//! zcash_local_net's [`zcash_local_net::client`] module: each wallet
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

use zcash_local_net::client::{
    zcash_devtool::{ZcashDevtool, ZcashDevtoolConfig},
    AddressReceiver, Client as _, GetInfo, WalletBalance,
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
/// listener. The devtool analogue of [`crate::build_clients`]; Zaino must
/// already be serving (wallet initialization fetches the chain tip and
/// birthday tree state from it).
pub async fn build_clients(zaino_grpc_listen_port: u16) -> DevtoolClients {
    let mut faucet_config = ZcashDevtoolConfig::faucet();
    faucet_config.indexer_port = zaino_grpc_listen_port;
    let faucet = ZcashDevtool::launch(faucet_config)
        .await
        .expect("launch devtool faucet wallet");

    let mut recipient_config = ZcashDevtoolConfig::recipient();
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
            Pool::Sapling => balance.sapling_spendable,
            Pool::Transparent => balance.transparent_spendable,
        }
    }
}
