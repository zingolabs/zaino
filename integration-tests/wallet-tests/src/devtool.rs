//! zcash-devtool-backed wallet clients: the in-progress replacement for the
//! zingolib lightclients (zingolabs/infrastructure#269).
//!
//! Implements [`crate::TestWallet`] for [`ZcashDevtool`] and re-exports
//! [`DevtoolClients`] as `TestClients<ZcashDevtool>`. Tests import the type
//! alias so they can later swap the backend without changing call sites.
//!
//! The clients are managed by zcash_local_net's
//! [`zcash_local_net::client`] module: each wallet operation is a
//! run-to-completion `zcash-devtool` subprocess invocation (the binary must
//! be built with `--features regtest_support` and be locatable via
//! `TEST_BINARIES_DIR`/`PATH`).
//!
//! # Known gaps vs the zingolib backend
//!
//! - **Unconfirmed (mempool) balances**: devtool sync is block-based;
//!   `monitor_unverified_mempool` cannot swap backends.
//! - **Fee constants**: ZIP-317 applies on both backends, but asserted
//!   constants derived from zingolib note selection (e.g. 235_000 after
//!   shielding 250_000) must be re-verified on first devtool runs.

use zcash_local_net::client::{
    zcash_devtool::{ZcashDevtool, ZcashDevtoolConfig},
    AddressReceiver, Client,
};
use zcash_primitives::transaction::TxId;
use zcash_protocol::value::Zatoshis;

use crate::{TestClients, TestWallet, WalletPoolBalances};

/// Devtool-backed test clients: a faucet and a recipient.
pub type DevtoolClients = TestClients<ZcashDevtool>;

/// Launch faucet + recipient devtool wallets against a running Zaino gRPC
/// listener. Zaino must already be serving (wallet initialization fetches
/// the chain tip and birthday tree state from it).
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

/// Parse a devtool display-order txid hex string into a [`TxId`].
///
/// Devtool (like all zcash/bitcoin tooling) prints transaction ids in
/// reversed ("display") byte order. [`TxId`] stores the canonical
/// internal order and handles the reversal, so callers get a
/// type-safe handle instead of raw bytes.
///
/// Callers that need the internal-order bytes for proto fields
/// (e.g. `TxFilter.hash`) can use `txid.as_ref().to_vec()`.
pub fn txid_from_devtool(devtool_txid_hex: &str) -> TxId {
    let mut bytes: [u8; 32] = hex::decode(devtool_txid_hex.trim())
        .expect("devtool txid is valid hex")
        .try_into()
        .expect("devtool txid is 32 bytes");
    bytes.reverse();
    TxId::from_bytes(bytes)
}

impl TestWallet for ZcashDevtool {
    async fn address(&self, pool: &str) -> String {
        let receiver = match pool {
            "transparent" => AddressReceiver::Transparent,
            "sapling" => AddressReceiver::Sapling,
            "unified" => AddressReceiver::Unified,
            "orchard" => AddressReceiver::Orchard,
            other => panic!("unknown pool address kind {other:?}"),
        };
        Client::address(self, receiver)
            .await
            .unwrap_or_else(|e| panic!("address({pool}): {e:?}"))
    }

    async fn balance(&self) -> WalletPoolBalances {
        let b = Client::balance(self)
            .await
            .unwrap_or_else(|e| panic!("balance: {e:?}"));
        WalletPoolBalances {
            orchard: Zatoshis::from_u64(b.orchard_spendable)
                .expect("orchard balance in valid range"),
            sapling: Zatoshis::from_u64(b.sapling_spendable)
                .expect("sapling balance in valid range"),
            transparent: Zatoshis::from_u64(b.transparent_spendable)
                .expect("transparent balance in valid range"),
        }
    }

    async fn send(&self, address: &str, amount: u64) -> String {
        Client::send(self, address, amount)
            .await
            .unwrap_or_else(|e| panic!("send: {e:?}"))
    }

    async fn shield(&self) {
        Client::shield(self)
            .await
            .unwrap_or_else(|e| panic!("shield: {e:?}"));
    }

    async fn sync(&self) {
        Client::sync(self)
            .await
            .unwrap_or_else(|e| panic!("sync: {e:?}"));
    }

    async fn rescan(&self) {
        Client::rescan(self)
            .await
            .unwrap_or_else(|e| panic!("rescan: {e:?}"));
    }
}
