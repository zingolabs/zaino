//! Wallet-to-validator integration tests.
//!
//! These exercise the zcash-devtool wallet client (a faucet and a recipient,
//! see [`devtool`]) against a running Zaino indexer, built from a launched
//! [`zaino_testutils::TestManager`]'s gRPC address. The clients are managed by
//! `zcash_local_net`; this workspace keeps its own Cargo.lock for that stack.
//!
//! # Client abstraction
//!
//! [`TestWallet`] defines the wallet operations tests rely on (send, shield,
//! sync, balance, addresses). Backends implement this trait so test code
//! depends on the interface, not on a specific implementation. The devtool
//! backend lives in [`devtool`].

#![forbid(unsafe_code)]

use zcash_protocol::value::Zatoshis;
use zaino_proto::proto::compact_formats::CompactBlock;
use zcash_primitives::transaction::TxId;

pub mod devtool;

/// A shielded/transparent pool, paired with the address kind that routes funds
/// into it. Lets a send-and-check test take a single `Pool` instead of an
/// address string plus a balance-field closure.
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
}

/// Backend-agnostic per-pool balance snapshot.
///
/// Each field is the spendable balance in its pool, as [`Zatoshis`].
/// Backends populate this from their native balance type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalletPoolBalances {
    pub orchard: Zatoshis,
    pub sapling: Zatoshis,
    pub transparent: Zatoshis,
}

impl WalletPoolBalances {
    /// The spendable balance in the given pool.
    pub fn spendable(self, pool: Pool) -> Zatoshis {
        match pool {
            Pool::Orchard => self.orchard,
            Pool::Sapling => self.sapling,
            Pool::Transparent => self.transparent,
        }
    }
}

/// The operations a test wallet backend must support.
///
/// Each method panics on failure — test wallets are not expected to
/// recover from errors, so the trait surface stays minimal.
pub trait TestWallet {
    /// Return the address that routes funds into `pool`
    /// (`"transparent" | "sapling" | "unified" | "orchard"`).
    fn address(
        &self,
        pool: &str,
    ) -> impl std::future::Future<Output = String> + Send;

    /// Return the wallet's per-pool spendable balances.
    fn balance(
        &self,
    ) -> impl std::future::Future<Output = WalletPoolBalances> + Send;

    /// Send `amount` zatoshis to `address`, returning the broadcast txid as a
    /// display-order hex string.
    fn send(
        &self,
        address: &str,
        amount: u64,
    ) -> impl std::future::Future<Output = String> + Send;

    /// Shield transparent funds into orchard.
    fn shield(&self) -> impl std::future::Future<Output = ()> + Send;

    /// Sync the wallet to the chain tip.
    fn sync(&self) -> impl std::future::Future<Output = ()> + Send;

    /// Forget wallet state and re-sync from scratch.
    fn rescan(&self) -> impl std::future::Future<Output = ()> + Send;
}

/// A pair of test wallets: a funded faucet and a recipient.
///
/// Generic over the wallet backend so tests do not depend on a specific
/// implementation.
pub struct TestClients<W> {
    /// Faucet wallet (owns the miner addresses).
    pub faucet: W,
    /// Recipient wallet.
    pub recipient: W,
}

impl<W: TestWallet> TestClients<W> {
    /// The faucet address that routes funds into `pool`.
    pub async fn get_faucet_address(&self, pool: &str) -> String {
        self.faucet.address(pool).await
    }

    /// The recipient address that routes funds into `pool`.
    pub async fn get_recipient_address(&self, pool: &str) -> String {
        self.recipient.address(pool).await
    }

    /// The faucet's per-pool spendable balances.
    pub async fn faucet_balance(&self) -> WalletPoolBalances {
        self.faucet.balance().await
    }

    /// The recipient's per-pool spendable balances.
    pub async fn recipient_balance(&self) -> WalletPoolBalances {
        self.recipient.balance().await
    }

    /// Send `amount` zatoshis from the faucet to `address`.
    pub async fn send_from_faucet(&self, address: &str, amount: u64) -> String {
        self.faucet.send(address, amount).await
    }

    /// Send `amount` zatoshis from the recipient to `address`.
    pub async fn send_from_recipient(&self, address: &str, amount: u64) -> String {
        self.recipient.send(address, amount).await
    }

    /// Shield the faucet's transparent funds into orchard.
    pub async fn shield_faucet(&self) {
        self.faucet.shield().await;
    }

    /// Shield the recipient's transparent funds into orchard.
    pub async fn shield_recipient(&self) {
        self.recipient.shield().await;
    }

    /// Sync the faucet wallet to the chain tip.
    pub async fn sync_faucet(&self) {
        self.faucet.sync().await;
    }

    /// Sync the recipient wallet to the chain tip.
    pub async fn sync_recipient(&self) {
        self.recipient.sync().await;
    }

    /// Forget the recipient's wallet state and re-sync from scratch.
    pub async fn rescan_recipient(&self) {
        self.recipient.rescan().await;
        self.recipient.sync().await;
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
